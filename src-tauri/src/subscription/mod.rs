//! 订阅额度模块：与 collector 平行的独立数据链路。
//!
//! - 数据源：各 agent CLI 的**本机凭据文件**（只读）+ 平台 usage 端点（逆向）;
//!   Claude 另有桌面端本地采样回落（claude_desktop.rs,零凭据）;
//! - 存储：`<数据根>/subscriptions.db`（快照 + 绑定开关;凭据永不落库）;
//! - 轮询：独立 daemon 线程（对齐 collector 模式）——默认 5 分钟,长期安静时
//!   待机放慢（idle.rs,封顶 30 分钟;变化 / 本地 agent 活动 / 用户注意即退出）;
//!   绑定前零网络,凭据判死后零网络（mtime 自愈探针复活）;
//! - 红线：凭据文件只读不写,刷新 token 只存内存;单平台失败不拖垮整体。

pub mod boost;
pub mod claude;
pub mod claude_desktop;
pub mod codex;
pub mod credentials;
pub mod idle;
pub mod model;
pub mod store;

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Manager};

use model::{CredentialInfo, FetchStatus, Platform, SubscriptionSnapshot};
use store::SubStore;

/// 默认轮询间隔（5 分钟起步,S4 设置页 5/10/15/30 分钟可调）。
const DEFAULT_POLL_SECS: u64 = 300;
const MIN_POLL_SECS: u64 = 60;
const MAX_POLL_SECS: u64 = 1800;

/// 轮询间隔（prefs 前的运行时默认;S4 设置页接线后经 AppState 存储）。
static POLL_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(DEFAULT_POLL_SECS);

/// 唤醒（立即刷新一次;bind/手动 refresh 用）。
static WAKE: (Mutex<u64>, Condvar) = (Mutex::new(0), Condvar::new());

pub fn wake() {
    // poison 容忍（审计 P3-）：持锁线程 panic 后唤醒通道仍须可用,
    // 否则主轮询的刷新/bind/间隔变更全部连锁失效。
    let mut gen = WAKE.0.lock().unwrap_or_else(|e| e.into_inner());
    *gen += 1;
    WAKE.1.notify_all();
}

/// 重算睡眠（不推进代际 = 不触发全量轮）：待机应检时刻被提前后,让主轮询醒来
/// 按 `idle:due` 只检到期平台。持 WAKE.0 再 notify——主线程读代际→算时长→
/// 进等待全程持锁,通知不会落进解锁间隙（同审计 P1-）。
pub fn reschedule() {
    let _g = WAKE.0.lock().unwrap_or_else(|e| e.into_inner());
    WAKE.1.notify_all();
}

/// 待机退出入口（用户注意 / 本地 agent 活动,语义见 `idle:note_attention`）:
/// 翻转广播 `subscription:idle`,应检时刻提前则唤醒主轮询重算睡眠。
pub fn nudge_standby(app: &AppHandle, fetch_now: Option<Platform>) {
    let now = chrono::Utc::now().timestamp();
    let a = idle::note_attention(now, fetch_now);
    if a.flipped {
        idle::emit_idle(app);
        crate::dev_log!("[subscription] standby exited (attention, fetch_now={:?})", fetch_now);
    }
    if a.rescheduled {
        reschedule();
    }
}

pub fn poll_secs() -> u64 {
    POLL_SECS.load(std::sync::atomic::Ordering::SeqCst)
}

/// 调整轮询间隔（S4 设置页接线;调用方按 clamp 规则传值）。
pub fn set_poll_secs(v: u64) {
    POLL_SECS.store(
        v.clamp(MIN_POLL_SECS, MAX_POLL_SECS),
        std::sync::atomic::Ordering::SeqCst,
    );
}

/// 共享适配器（内存 token 缓存生命周期 = 线程;命令侧经 Arc 仅做 invalidate,S4 接线）。
pub struct Adapters {
    codex: codex::CodexAdapter,
    claude: claude::ClaudeAdapter,
    /// 判死时记录的凭据文件 mtime（复活探针基线,预留诊断;当前自愈走 last_mtime 局部表）。
    #[allow(dead_code)]
    dead_mtime: Mutex<std::collections::HashMap<Platform, Option<i64>>>,
    /// 取数互斥（每平台一枚,下标 = `fetch_lock_index`）：boost 线程与主轮询共享
    /// 同一个 Adapters,无互斥时两者可能在冷却检查与 429 冷却写入之间各发一请求
    /// （限流桶风险,审计 P3-）。主轮询阻塞等锁;boost 走 try 让行（不阻塞
    /// 其 1s 节拍）。
    fetch_locks: [Mutex<()>; 2],
}

impl Adapters {
    /// 取数互斥下标（两平台固定映射;与 `Platform` 枚举一一对应）。
    fn fetch_lock_index(platform: Platform) -> usize {
        match platform {
            Platform::Codex => 0,
            Platform::Claude => 1,
        }
    }

    /// 取数互斥守卫（阻塞;主轮询用）：同一平台同一时刻只有一个线程在取数。
    pub fn lock_fetch(&self, platform: Platform) -> std::sync::MutexGuard<'_, ()> {
        self.fetch_locks[Self::fetch_lock_index(platform)]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 取数互斥守卫（try;boost 用）：主轮询正在取数时让行（None）,不阻塞线程;
    /// 锁被 poison 同样让行（下一 interval 自然重试）。
    pub fn try_lock_fetch(&self, platform: Platform) -> Option<std::sync::MutexGuard<'_, ()>> {
        self.fetch_locks[Self::fetch_lock_index(platform)]
            .try_lock()
            .ok()
    }
}

/// 共享读取连接（命令线程查快照;WAL 一写多读,写连接归轮询线程）。
pub struct SubscriptionReader(pub Arc<Mutex<SubStore>>);

/// 共享适配器句柄（S4 设置页 bind 后 invalidate 内存缓存用;本阶段管理进 AppState 保生命周期）。
#[allow(dead_code)]
pub struct SubscriptionAdapters(pub Arc<Adapters>);

pub fn http_agent() -> Option<ureq::Agent> {
    Some(ureq::AgentBuilder::new().build())
}

/// 429 冷却闸（每平台一枚,嵌在各适配器里）：`Retry-After` 期内不打网络——
/// 限流桶（Claude 侧按 User-Agent 分档,见 claude.rs）最忌重试轰炸。
#[derive(Default)]
pub struct RateGate(std::sync::Mutex<Option<i64>>);

impl RateGate {
    /// 取闸位（poison 容忍,审计 P3-：单次 panic 不应让整个订阅链路失去限流保护）。
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<i64>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 剩余冷却秒数（None = 未冷却）。
    pub fn remaining(&self, now: i64) -> Option<i64> {
        self.slot().map(|until| until - now).filter(|rest| *rest > 0)
    }

    /// 触发限流：从 now 起冷却 secs 秒（多次触发取更晚者）。
    pub fn arm(&self, now: i64, secs: i64) {
        let until = now + secs;
        let mut g = self.slot();
        if g.map(|cur| until > cur).unwrap_or(true) {
            *g = Some(until);
        }
    }

    pub fn clear(&self) {
        *self.slot() = None;
    }
}

// ---------- 请求日志（限流分析用,dev-only） ----------

thread_local! {
    /// 本线程最近一次 usage 请求的 HTTP 结果标签（适配器发请求后写入,
    /// `fetch_one` 取走记日志;None = 没发请求）。取数全程在同一线程。
    static LAST_HTTP: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// 适配器在拿到 usage 响应后调用：记 HTTP 状态码（429 附原始 Retry-After）
/// 或传输层错误种类。**只记状态,不含 URL 参数 / 头 / body——无 token 泄露面**。
pub(crate) fn note_http(resp: &Result<ureq::Response, ureq::Error>) {
    let label = match resp {
        Ok(r) => r.status().to_string(),
        Err(ureq::Error::Status(code, r)) => match r.header("retry-after") {
            Some(ra) => format!("{code} retry-after={}", ra.trim()),
            None => code.to_string(),
        },
        Err(ureq::Error::Transport(t)) => format!("transport:{:?}", t.kind()),
    };
    LAST_HTTP.with(|c| *c.borrow_mut() = Some(label));
}

/// 一轮取数一行：`via` = main / boost;`http` = 状态码或 `-`（未发请求）;
/// `cooldown` = 结束时生效的限流暂停剩余秒数。
fn log_fetch(via: &str, platform: Platform, adapters: &Adapters, snap: &SubscriptionSnapshot, note: &str) {
    let http = LAST_HTTP.with(|c| c.borrow_mut().take()).unwrap_or_else(|| "-".into());
    let cooldown = match platform {
        Platform::Codex => adapters.codex.cooldown_remaining(),
        Platform::Claude => adapters.claude.cooldown_remaining(),
    };
    let cooldown = cooldown.map_or_else(|| "-".to_string(), |s| format!("{s}s"));
    crate::dev_log!(
        "[subscription] fetch via={via} platform={} http={http} status={} cooldown={cooldown}{note}",
        platform.as_str(),
        snap.status.as_str(),
    );
}

// ---------- 采集编排 ----------

/// 单平台取数一轮。
pub(crate) fn fetch_one(adapters: &Adapters, platform: Platform, via: &str) -> SubscriptionSnapshot {
    LAST_HTTP.with(|c| *c.borrow_mut() = None);
    // 冷却期内不碰网络（限流退避,静默保留旧数据）
    let cooling = match platform {
        Platform::Codex => adapters.codex.cooldown_remaining(),
        Platform::Claude => adapters.claude.cooldown_remaining(),
    };
    if cooling.is_some() {
        let snap = rate_limited_snapshot(platform);
        log_fetch(via, platform, adapters, &snap, " skipped=cooling");
        return snap;
    }

    let mut note = "";
    let snap = match platform {
        Platform::Codex => match adapters.codex.obtain_access(platform) {
            codex::Access::Ok(cred) => codex::fetch(&adapters.codex, cred),
            codex::Access::Dead => dead_snapshot(platform),
            codex::Access::None => idle_snapshot(platform),
            codex::Access::Transient => transient_snapshot(platform),
        },
        // 主路径拿不到数（凭据过期 / 无凭据文件）→ 回落桌面端采样（claude_desktop.rs）
        Platform::Claude => {
            let primary = match adapters.claude.obtain_access(platform) {
                claude::Access::Ok(cred) => claude::fetch(&adapters.claude, cred),
                claude::Access::Dead => dead_snapshot(platform),
                claude::Access::None => idle_snapshot(platform),
            };
            with_desktop_fallback(primary, &mut note)
        }
    };
    log_fetch(via, platform, adapters, &snap, note);
    snap
}

/// Claude 主路径结果套桌面端回落;真的换成了桌面端样本时给日志附 `source=desktop`
/// 及主路径原状态。
fn with_desktop_fallback(primary: SubscriptionSnapshot, note: &mut &'static str) -> SubscriptionSnapshot {
    if primary.platform != Platform::Claude {
        return primary;
    }
    let before = primary.status;
    let snap = claude_desktop::fallback(primary, chrono::Utc::now().timestamp());
    if before != snap.status {
        *note = match before {
            FetchStatus::AuthFailed => " source=desktop primary=auth_failed",
            FetchStatus::Idle => " source=desktop primary=idle",
            _ => " source=desktop",
        };
    }
    snap
}

fn dead_snapshot(platform: Platform) -> SubscriptionSnapshot {
    SubscriptionSnapshot {
        platform,
        plan_type: "unknown".into(),
        windows: vec![],
        fetched_at: None,
        status: FetchStatus::AuthFailed,
    }
}

fn idle_snapshot(platform: Platform) -> SubscriptionSnapshot {
    SubscriptionSnapshot {
        platform,
        plan_type: "unknown".into(),
        windows: vec![],
        fetched_at: None,
        status: FetchStatus::Idle,
    }
}

fn transient_snapshot(platform: Platform) -> SubscriptionSnapshot {
    SubscriptionSnapshot {
        platform,
        plan_type: "unknown".into(),
        windows: vec![],
        fetched_at: None,
        status: FetchStatus::NetworkFailed,
    }
}

fn rate_limited_snapshot(platform: Platform) -> SubscriptionSnapshot {
    SubscriptionSnapshot {
        platform,
        plan_type: "unknown".into(),
        windows: vec![],
        fetched_at: None,
        status: FetchStatus::RateLimited,
    }
}

fn run(app: AppHandle, write_store: SubStore, adapters: Arc<Adapters>) {
    crate::dev_log!("[subscription] thread started");
    let mut last_mtime: std::collections::HashMap<Platform, Option<i64>> =
        std::collections::HashMap::new();
    // wake 代际：wait 返回后代际有变 = 手动刷新/bind/间隔调整唤醒 → 全量一轮
    //（绕过待机退档的应检时刻;初始 true = 启动先全量一轮）。代际未变的醒来
    //（`reschedule` / 虚假唤醒）只检到期平台。
    let mut woke = true;

    loop {
        let now = chrono::Utc::now().timestamp();
        let bound = write_store.bound_platforms();
        idle::prune(&bound);
        let mut checked = false;
        let mut idle_flipped = false;
        if !bound.is_empty() {
            let base = poll_secs();
            for platform in &bound {
                // 待机放慢（idle.rs）：未到该平台应检时刻就跳过;唤醒轮不受限
                if !woke && !idle::due(*platform, now) {
                    continue;
                }

                // 凭据文件 mtime 探针:变化 → 清内存 token 缓存（判死自愈入口;
                // 无变化且未判死 → 走缓存/现读）。纳秒级 stat,无网络。
                let mtime = credentials::credential_mtime(*platform);
                let changed = last_mtime.get(platform) != Some(&mtime);
                if changed {
                    adapters.codex.invalidate(*platform);
                    adapters.claude.invalidate(*platform);
                }
                last_mtime.insert(*platform, mtime);

                // 判死态:仅当文件刚变化（用户重新登录/CLI 续期）才复活重试,
                // 否则**零网络**（预案）。但快照仍要落成 auth_failed
                // ——判死是「不试」,不是「UI 停在旧结论」。
                let dead = match platform {
                    Platform::Codex =>
                        matches!(adapters.codex.obtain_access(*platform), codex::Access::Dead),
                    Platform::Claude => adapters.claude.is_dead(*platform),
                };
                let snap = if dead && !changed {
                    // 判死零网络,但桌面端采样是本地文件,照读（Claude 专属回落）
                    let mut note = "";
                    let snap = with_desktop_fallback(dead_snapshot(*platform), &mut note);
                    let note = if note.is_empty() { " skipped=dead" } else { " skipped=dead source=desktop" };
                    log_fetch("main", *platform, &adapters, &snap, note);
                    snap
                } else {
                    // 取数互斥（审计 P3-）：与 boost 线程共用同一把平台锁,
                    // 防两线程同时进入 fetch_one（冷却检查与 429 冷却写入之间
                    // 无原子性,各发一请求会加速触限）。主轮询阻塞等锁。
                    let _lease = adapters.lock_fetch(*platform);
                    fetch_one(&adapters, *platform, "main")
                };
                let _ = write_store.save_snapshot(&snap);
                // 待机评估：变化退出待机 / 无变化计安静轮（达标进入待机后退档）;翻转即广播
                if idle::observe(*platform, &snap, base, now) {
                    idle_flipped = true;
                    crate::dev_log!("[subscription] {} standby toggled", platform.as_str());
                }
                checked = true;
            }
            if checked {
                emit_changed(&app);
            }
            if idle_flipped {
                idle::emit_idle(&app);
            }
        }

        // 可中断睡眠（wake 提前返回）：睡到最近的应检时刻——待机退档后一觉
        // 最长可到 30 分钟;无绑定时仍按基础档空转睡眠（零网络）。
        // ⚠ 代际读取与 wait 必须**同一把锁贯穿**（审计 P1-）：分两次 lock 会在
        // 解锁间隙丢 wake 通知（notify 无等待者即失效,待机深档时唤醒最长被吞
        // 30 分钟）——读代际、算时长、进等待三步之间不得释放 WAKE.0。
        let guard = WAKE.0.lock().unwrap_or_else(|e| e.into_inner());
        let gen_before = *guard;
        let wait = idle::next_wait_secs(now, poll_secs());
        let (guard, _) = WAKE
            .1
            .wait_timeout(guard, Duration::from_secs(wait))
            .unwrap_or_else(|e| e.into_inner());
        woke = *guard != gen_before;
    }
}

fn emit_changed(app: &AppHandle) {
    use tauri::Emitter;
    let _ = app.emit("subscription:changed", true);
}

/// setup 调用:开库（写连接归线程;读连接进 AppState）+ spawn daemon。
/// 绑定前线程空转睡眠,**零网络**（S2 行为保证）。
pub fn spawn(app: AppHandle) -> Result<(), String> {
    let root = crate::data_root::current(&app)?;
    std::fs::create_dir_all(&root.root).map_err(|e| e.to_string())?;
    let write_store = SubStore::open(&root.subscriptions_db_path())?;
    let reader = Arc::new(Mutex::new(SubStore::open(&root.subscriptions_db_path())?));

    let adapters = Arc::new(Adapters {
        codex: codex::CodexAdapter::new(),
        claude: claude::ClaudeAdapter::new(),
        dead_mtime: Mutex::new(std::collections::HashMap::new()),
        fetch_locks: [Mutex::new(()), Mutex::new(())],
    });

    app.manage(SubscriptionReader(reader.clone()));
    app.manage(SubscriptionAdapters(adapters.clone()));

    // boost 监控（独立补充路由,见 boost.rs）:共享只读连接与适配器句柄
    // （token 缓存/RateGate 同源,两线程不各持一套限流闸）。
    boost::spawn(
        app.clone(),
        SubscriptionReader(reader),
        adapters.clone(),
    )?;

    std::thread::Builder::new()
        .name("subscription".into())
        .spawn(move || run(app, write_store, adapters))
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------- 命令面（snake_case,契约风格） ----------

/// 两平台归一化快照（前端唯一读口;未绑定平台 = idle 占位）。
#[tauri::command]
pub fn get_subscription_snapshots(
    state: tauri::State<'_, SubscriptionReader>,
) -> Result<Vec<SubscriptionSnapshot>, String> {
    let store = state.0.lock().unwrap();
    Ok(store.snapshots_for_frontend())
}

/// 扫描本机凭据文件（设置页发现列表;**只含存在性/可解析性/掩码,绝无 token 值**）。
#[tauri::command]
pub fn scan_subscription_credentials() -> Result<Vec<CredentialInfo>, String> {
    let mut out = vec![];
    for platform in [Platform::Codex, Platform::Claude] {
        let info = match credentials::read_credential(platform) {
            // 只用桌面端（无 CLI 凭据文件）也可绑定:桌面端采样文件即数据源
            None if platform == Platform::Claude && claude_desktop::is_present() => CredentialInfo {
                platform,
                present: true,
                parseable: true,
                account_hint: Some("claude-desktop".into()),
            },
            Some(cred) => CredentialInfo {
                platform,
                present: true,
                parseable: true,
                account_hint: cred.account_hint,
            },
            None => {
                let present = credentials::credential_path(platform)
                    .map(|p| p.exists())
                    .unwrap_or(false);
                CredentialInfo {
                    platform,
                    present,
                    parseable: false,
                    account_hint: None,
                }
            }
        };
        out.push(info);
    }
    Ok(out)
}

/// 绑定（= 接受该平台凭据源并立即轮询一轮;凭据本体永远在 agent 自己的文件里）。
#[tauri::command]
pub fn bind_subscription(
    app: AppHandle,
    state: tauri::State<'_, SubscriptionReader>,
    platform: String,
) -> Result<(), String> {
    let platform =
        Platform::from_str(&platform).ok_or_else(|| format!("unknown platform: {platform}"))?;
    // 绑定前置校验:凭据必须存在且可解析（避免绑了个空壳）
    if credentials::read_credential(platform).is_none()
        && !(platform == Platform::Claude && claude_desktop::is_present())
    {
        return Err("credential file not found or unparseable".to_string());
    }
    let now = chrono::Utc::now().timestamp();
    {
        let store = state.0.lock().unwrap();
        store.bind(platform, now)?;
    }
    wake();
    emit_changed(&app);
    Ok(())
}

/// 解绑（停该平台轮询 + 清快照）。
#[tauri::command]
pub fn unbind_subscription(
    app: AppHandle,
    state: tauri::State<'_, SubscriptionReader>,
    platform: String,
) -> Result<(), String> {
    let platform =
        Platform::from_str(&platform).ok_or_else(|| format!("unknown platform: {platform}"))?;
    {
        let store = state.0.lock().unwrap();
        store.unbind(platform)?;
        let _ = store.save_snapshot(&SubscriptionSnapshot {
            platform,
            plan_type: "unknown".into(),
            windows: vec![],
            fetched_at: None,
            status: FetchStatus::Idle,
        });
    }
    wake();
    emit_changed(&app);
    Ok(())
}

/// 手动立即刷新（右键菜单/设置页按钮）。手动刷新 = 用户注意到额度 → 先退出待机
///再全量取数;注意在前,唤醒轮的无变化只计第 1 轮安静。
#[tauri::command]
pub fn refresh_subscriptions_now(app: AppHandle) -> Result<(), String> {
    nudge_standby(&app, None);
    wake();
    emit_changed(&app);
    Ok(())
}

/// 设置轮询间隔（秒;设置页 Subscriptions tab 5/10/15/30 分钟下拉）。
/// 存 prefs 侧的持久化由前端 designPrefs 管理（subscriptionPollSecs 键）,
/// 此处只改运行时值——重启后前端初查时再调用本命令恢复。
#[tauri::command]
pub fn set_subscription_poll_secs(secs: u64) -> Result<(), String> {
    // 幂等早退（审计 P2-）：设置页直调 + 装载恢复 effect 会重复下发同一值,
    // 不早退则每次多唤醒主轮询一整轮（两平台各多发一次请求）。
    let next = secs.clamp(MIN_POLL_SECS, MAX_POLL_SECS);
    if poll_secs() == next {
        return Ok(());
    }
    set_poll_secs(next);
    wake();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_gate_arms_expires_and_clears() {
        let g = RateGate::default();
        assert_eq!(g.remaining(1000), None);
        g.arm(1000, 120);
        assert_eq!(g.remaining(1000), Some(120));
        assert_eq!(g.remaining(1100), Some(20));
        assert_eq!(g.remaining(1200), None); // 到期即失效
        g.arm(1000, 300);
        g.arm(1000, 60); // 多次触发取更晚者（不退避到更早）
        assert_eq!(g.remaining(1000), Some(300));
        g.clear();
        assert_eq!(g.remaining(1000), None);
    }
}
