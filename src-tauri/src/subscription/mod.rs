//! 订阅额度模块：与 collector 平行的独立数据链路。
//!
//! - 数据源：各 agent CLI 的**本机凭据文件**（只读）+ 平台 usage 端点（逆向）;
//!   Claude 另有桌面端本地采样回落（claude_desktop.rs,零凭据）;
//! - 存储：`<数据根>/subscriptions.db`（快照 + 绑定开关;凭据永不落库）;
//! - 取数节奏：**按预计消耗取数**——采集线程报来的分模型 token 经 cost.rs 折成代价、
//!   经 calib.rs 的标定系数换成「大约消耗了百分之几」,达到阈值就取一轮（demand.rs）;
//!   本地无 token 则不取。固定间隔只是**兜底**,覆盖不产生本地 token 的在线 / 网页用量;
//!   Claude 侧兜底先用桌面端采样零请求探测有没有涨（claude_desktop:probe_growth）——
//!   **没涨也会把样本里的下降正进快照**（滚动窗口空闲期余量会自己恢复,
//!   `apply_flat_sample`）。本地 token 静默即待机（idle.rs,只翻转悬浮球减淡,不改取数
//!   频次）;绑定前零网络,凭据判死后零网络（mtime 探针复活）;**没取到更新读数的轮按
//!   demand.rs 的退避推后重试**,故障期间不按采集频率空转;
//! - 红线：凭据文件只读不写,刷新 token 只存内存;单平台失败不拖垮整体。

pub mod bootstrap;
pub mod calib;
pub mod claude;
pub mod claude_desktop;
pub mod codex;
pub mod codex_rollout;
pub mod cost;
pub mod credentials;
pub mod demand;
pub mod idle;
pub mod model;
pub mod price;
pub mod query;
pub mod store;
#[cfg(test)]
mod smoke;

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Manager};

use model::{CredentialInfo, FetchStatus, Platform, SnapshotSource, SubscriptionSnapshot};
use store::SubStore;

/// 默认**兜底**取数间隔（设置页 5/10/15/30 分钟可调）。
/// 兜底没有任何动态检测能力,只是「本地 token 看不见的用量」的最后一道保底,
/// 故取封顶档;真正的取数时机由本地 token 驱动（demand.rs）。
const DEFAULT_POLL_SECS: u64 = 1800;
const MIN_POLL_SECS: u64 = 60;
const MAX_POLL_SECS: u64 = 1800;

/// 桌面端采样的**收割节律**（秒）。与取数无关——收割只读本机文件、零网络,所以可以比
/// 兜底档密得多：桌面端 15 分钟写一条,5 分钟扫一次即可「样本一落地就进库」,也让兜底轮
/// 的读数正（`apply_flat_sample`）最多滞后这么久。mtime 没变时这一轮只花一次 stat。
const HARVEST_SECS: u64 = 300;

/// 手动刷新的最小间隔（秒）。手动刷新走 `wake` 推进代际,一轮进行期间的连点会被代际
/// 吸收成一轮,但「一轮结束后再点」不受约束——Claude 的 usage 端点按 User-Agent 分限流桶,
/// 连打最容易把自己打进 429 冷却。被挡下的那次仍然退待机 + 广播,前端刷新动画照常落地。
const MANUAL_REFRESH_MIN_GAP_SECS: i64 = 15;

/// 上次放行的手动刷新时刻（unix 秒;0 = 从未）。
static LAST_MANUAL_REFRESH: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// 兜底取数间隔（秒;运行时值,由 `set_poll_secs` 下发,持久化在前端）。
static POLL_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(DEFAULT_POLL_SECS);

/// 唤醒（立即刷新一次;bind/手动 refresh 用）。
static WAKE: (Mutex<u64>, Condvar) = (Mutex::new(0), Condvar::new());

pub fn wake() {
    // poison 容忍：持锁线程 panic 后唤醒通道仍须可用,
    // 否则主轮询的刷新/bind/间隔变更全部连锁失效。
    let mut gen = WAKE.0.lock().unwrap_or_else(|e| e.into_inner());
    *gen += 1;
    WAKE.1.notify_all();
}

/// 重算睡眠（不推进代际 = 不触发全量轮）：应检时刻被提前后,让主轮询醒来
/// 按 `idle:due` 只检到期平台。持 WAKE.0 再 notify——主线程读代际→算时长→
/// 进等待全程持锁,通知不会落进解锁间隙。
pub fn reschedule() {
    let _g = WAKE.0.lock().unwrap_or_else(|e| e.into_inner());
    WAKE.1.notify_all();
}

/// 本地 token 信号（采集线程每源一轮调用一次）：退出待机 + 按**预计消耗**排定取数。
/// `usage` = 提交的分模型 token 明细（模型 → [输入, 输出, 缓存读, 缓存写]）;
/// `platform` = 该源计入的订阅平台（None = 该源不对应订阅,只退待机）。
pub fn note_local_tokens(
    app: &AppHandle,
    platform: Option<Platform>,
    usage: &std::collections::BTreeMap<String, [i64; 4]>,
) {
    let now = chrono::Utc::now().timestamp();
    nudge_standby(app);
    let Some(platform) = platform else { return };
    if usage.is_empty() {
        return;
    }
    // 余量决定阈值（开「低余量收紧」时 5h 剩余 ≤ 20% 阈值减半）
    let remaining = remaining_5h(app, platform);
    if let Some(due) = demand::note_tokens(platform, usage, now, remaining) {
        if idle::pull_forward(platform, due) {
            reschedule();
        }
        crate::dev_log!(
            "[subscription] {} est +{:.2}% (threshold {:.2}%) → fetch due in {}s",
            platform.as_str(),
            demand::estimated_pct(platform),
            demand::effective_threshold(remaining),
            (due - now).max(0)
        );
    }
}

/// 该平台最近一次读数的 5h 剩余百分比（拿不到 → None,按原阈值处理）。
fn remaining_5h(app: &AppHandle, platform: Platform) -> Option<f64> {
    let state = app.try_state::<SubscriptionReader>()?;
    let store = state.0.lock().unwrap_or_else(|e| e.into_inner());
    let snap = store.load_snapshot(platform)?;
    snap.windows
        .iter()
        .find(|w| w.kind == "5h")
        .map(|w| 100.0 - w.used_percent)
}

/// 待机退出入口（用户注意 / 本地 agent 活动,语义见 `idle:note_attention`）:
/// 只翻转视觉态并广播 `subscription:idle`——取数时机单一源在 demand.rs。
pub fn nudge_standby(app: &AppHandle) {
    let now = chrono::Utc::now().timestamp();
    if idle::note_attention(now) {
        idle::emit_idle(app);
        crate::dev_log!("[subscription] standby exited (local tokens / attention)");
    }
}

pub fn poll_secs() -> u64 {
    POLL_SECS.load(std::sync::atomic::Ordering::SeqCst)
}

/// 调整兜底取数间隔（调用方按 clamp 规则传值）。
pub fn set_poll_secs(v: u64) {
    POLL_SECS.store(
        v.clamp(MIN_POLL_SECS, MAX_POLL_SECS),
        std::sync::atomic::Ordering::SeqCst,
    );
}

/// 共享适配器（内存 token 缓存生命周期 = 线程;命令侧经 Arc 共享）。
pub struct Adapters {
    codex: codex::CodexAdapter,
    claude: claude::ClaudeAdapter,
    /// 取数互斥（每平台一枚,下标 = `fetch_lock_index`）：命令面的立即刷新与
    /// 轮询线程可能同时进入取数,无互斥时两者会在冷却检查与 429 冷却写入之间
    /// 各发一请求（限流桶风险）。
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

    /// 取数互斥守卫（阻塞）：同一平台同一时刻只有一个线程在取数。
    pub fn lock_fetch(&self, platform: Platform) -> std::sync::MutexGuard<'_, ()> {
        self.fetch_locks[Self::fetch_lock_index(platform)]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

}

/// 共享读取连接（命令线程查快照;WAL 一写多读,写连接归轮询线程）。
pub struct SubscriptionReader(pub Arc<Mutex<SubStore>>);

pub fn http_agent() -> Option<ureq::Agent> {
    Some(ureq::AgentBuilder::new().build())
}

/// 429 冷却闸（每平台一枚,嵌在各适配器里）：`Retry-After` 期内不打网络——
/// 限流桶（Claude 侧按 User-Agent 分档,见 claude.rs）最忌重试轰炸。
#[derive(Default)]
pub struct RateGate(std::sync::Mutex<Option<i64>>);

impl RateGate {
    /// 取闸位（poison 容忍：单次 panic 不应让整个订阅链路失去限流保护）。
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

/// 一轮取数一行：`via` = token / fallback / wake;`http` = 状态码或 `-`（未发请求）;
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

/// 兜底轮的零请求短路（仅 Claude）：桌面端采样比上次读数新且没涨 ⇒ 不必打网络
/// （桌面端记的是服务端口径,在线 / 网页用量也在内——正是兜底轮要覆盖的场景）。
/// 返回 Some = 零网络。token 驱动轮与唤醒轮不短路——用户要的就是那一刻
/// 的即时读数。
///
/// 「没涨」**不等于**「没变」：5h / 7d 都是滚动窗口,空闲期间旧用量不断过期,余量会自己
/// 涨回来。所以把样本里的**下降**正进快照（仍然零网络,细则见 `apply_flat_sample`）,
/// 否则球上的数会停在偏低的旧值,直到复工或手动刷新。
fn skip_by_desktop_probe(
    via: &str,
    platform: Platform,
    prev: Option<&SubscriptionSnapshot>,
    now: i64,
) -> Option<SubscriptionSnapshot> {
    if via != "fallback" || platform != Platform::Claude {
        return None;
    }
    let prev = prev?;
    match claude_desktop::probe_growth(prev, now) {
        claude_desktop::OnlineProbe::NoGrowth => Some(claude_desktop::apply_flat_sample(prev, now)),
        _ => None,
    }
}

/// 成功取到新读数后落一条标定样本,并重算系数。
/// 上一读数不是成功态 / 本地零代价（纯在线用量）→ 不落样本,但账目照常清零。
///
/// **只认两端都是 API 读数的读数对**：桌面端来源的读数是整数百分比,`fetched_at` 又是
/// 样本时刻（可能比轮时刻早十几分钟）,而代价是按 token 到达时刻累加的——两个时间窗对
/// 不齐,单条样本的错配可达区间的一半。桌面端那一路由 `bootstrap:ingest` 按**样本时刻**
/// 精确切,密度还更高（15 分钟一条）。
fn record_pair(
    store: &SubStore,
    platform: Platform,
    prev: Option<&SubscriptionSnapshot>,
    snap: &SubscriptionSnapshot,
    now: i64,
) {
    let acc = demand::take_account(platform, now);
    let (Some(prev), Some(t1)) = (prev, snap.fetched_at) else { return };
    let (Some(t0), true) = (prev.fetched_at, prev.status == FetchStatus::Ok) else { return };
    if acc.cost <= 0.0 || acc.since <= 0 {
        return;
    }
    // 两端任一来自桌面端采样 → 交给 bootstrap:ingest 那一路（账目照常清零）
    if prev.source != SnapshotSource::Api || snap.source != SnapshotSource::Api {
        return;
    }
    let used = |s: &SubscriptionSnapshot, kind: &str| {
        s.windows.iter().find(|w| w.kind == kind).map(|w| w.used_percent)
    };
    let (Some(u5_0), Some(u5_1)) = (used(prev, "5h"), used(snap, "5h")) else { return };
    let resets = |s: &SubscriptionSnapshot| {
        s.windows.iter().find(|w| w.kind == "5h").and_then(|w| w.resets_at)
    };
    let pair = calib::Pair {
        t0,
        t1,
        used5_0: u5_0,
        used5_1: u5_1,
        // 两端各自申报的窗尾:前移了就是这中间重置过,Δ 不是这段消耗涨出来的
        resets5_0: resets(prev),
        resets5_1: resets(snap),
        cost: acc.cost,
        // 在线路**给不出老化量**：`demand` 的账目只从上一次成功轮累加,没有「5 小时前
        // 那一段」的历史。填 0 = 不做正（两条回溯路有历史,会填真值）;这一路样本很少,
        // 影响可忽略。
        aged_cost: 0.0,
        unknown_cost: acc.unknown_cost,
    };
    let used7 = (used(prev, "7d").unwrap_or(0.0), used(snap, "7d").unwrap_or(0.0));
    if let Err(e) =
        store.insert_pair(platform, &pair, used7, &acc.breakdown_json, "online", &snap.plan_type)
    {
        crate::dev_log!("[subscription] {} pair insert failed: {}", platform.as_str(), e);
        return;
    }
    crate::dev_log!(
        "[subscription] {} pair: est {:.2}% vs actual {:.2}% over {}s (cost {:.1}{})",
        platform.as_str(),
        pair.cost * calib::scale(platform),
        pair.used5_1 - pair.used5_0,
        pair.t1 - pair.t0,
        pair.cost,
        if pair.unknown_cost > 0.0 { ", unknown models" } else { "" }
    );
    calib::refit_from_store(store, platform);
}

fn dead_snapshot(platform: Platform) -> SubscriptionSnapshot {
    SubscriptionSnapshot {
        platform,
        plan_type: "unknown".into(),
        windows: vec![],
        fetched_at: None,
        status: FetchStatus::AuthFailed,
        source: SnapshotSource::Api,
    }
}

fn idle_snapshot(platform: Platform) -> SubscriptionSnapshot {
    SubscriptionSnapshot {
        platform,
        plan_type: "unknown".into(),
        windows: vec![],
        fetched_at: None,
        status: FetchStatus::Idle,
        source: SnapshotSource::Api,
    }
}

fn transient_snapshot(platform: Platform) -> SubscriptionSnapshot {
    SubscriptionSnapshot {
        platform,
        plan_type: "unknown".into(),
        windows: vec![],
        fetched_at: None,
        status: FetchStatus::NetworkFailed,
        source: SnapshotSource::Api,
    }
}

fn rate_limited_snapshot(platform: Platform) -> SubscriptionSnapshot {
    SubscriptionSnapshot {
        platform,
        plan_type: "unknown".into(),
        windows: vec![],
        fetched_at: None,
        status: FetchStatus::RateLimited,
        source: SnapshotSource::Api,
    }
}

/// 文件 mtime（unix 秒;取不到 → None）——冷启动期间给 Claude 收割当闸门用。
///
/// collector.db 开着 WAL：首扫的写入往往先落在 `-wal` 上,主库文件的 mtime 要等
/// checkpoint 才动 ⇒ 两个文件一起看,取较新的那个,否则首扫写完了闸门还是不动。
fn file_mtime(db: &std::path::Path) -> Option<i64> {
    [db.to_path_buf(), db.with_extension("db-wal")]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok())
        .filter_map(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .max()
}

fn run(app: AppHandle, write_store: SubStore, adapters: Arc<Adapters>) {
    crate::dev_log!("[subscription] thread started");
    // 本地读数的收割 + 增量标定路径（零网络零凭据）：
    // - Claude 走桌面端 plan-usage-history.json + collector.db（bootstrap.rs）;
    // - Codex 走会话 rollout 里的 rate_limits,读数与代价同一行（codex_rollout.rs）。
    // collector.db 的路径取一次:数据根在运行期不变,每轮重取只是白跑。
    let collector_db = crate::data_root::current(&app).ok().map(|r| r.db_path());
    // 上次收割时看到的源文件 mtime（None = 还没看过;文件没动就不重新解析）。
    // Claude 那一路的闸门是一对：（桌面端历史文件 mtime, 冷启动期间的 collector.db mtime)
    // ——理由见下面的注释。
    let mut last_harvest_mtime: (Option<i64>, Option<i64>) = (None, None);
    let mut last_rollout_gen: Option<(i64, u64)> = None;
    let mut last_mtime: std::collections::HashMap<Platform, Option<i64>> =
        std::collections::HashMap::new();
    // wake 代际：wait 返回后代际有变 = 手动刷新/bind/间隔调整唤醒 → 全量一轮
    //（绕过各平台的应检时刻;初始 true = 启动先全量一轮）。代际未变的醒来
    //（`reschedule` / 虚假唤醒）只检到期平台。
    let mut woke = true;

    loop {
        // 收割桌面端采样 + 按样本时刻增量建标定样本（零网络）。
        // 放在取数之前：的兜底短路要用最新样本做读数正。
        // **不看绑定**——桌面端历史只滚动保留 14 天,绑定前错过的样本以后再也拿不回来,
        // 而收割本身只是本机文件读 + 幂等写。
        if let Some(db) = collector_db.as_ref() {
            let mtime = claude_desktop::history_mtime();
            // **冷启动次序**：这一路要两份数据——桌面端的读数历史**和** collector.db 里的本地轮
            // ——而闸门只看前者的 mtime,两者的到位时刻毫不相关。全新安装的第一轮里 collector
            // 首扫还没写盘,读数收割进去了却建不出样本,mtime 却已经锁存 ⇒ 要等桌面应用再写一次
            // 采样（约 15 分钟,且它得在运行）或下次启动才补得上。
            //
            // 所以在「这一路还没建出过任何样本」期间,把 collector.db 的 mtime 一并计入闸门：
            // 首扫一写盘就立刻重试。建出第一条样本之后第二格恒为 None,闸门退回只看历史文件。
            // （从不用 Claude Code 的机器会一直停在冷启动态、跟着 collector 写盘重试——那正是
            // 要一直等的情形,每次重试也只是一次本地文件解析。）
            let cold = write_store.latest_pair_t1(Platform::Claude, bootstrap::PAIR_SRC).is_none();
            let gate = (mtime, if cold { file_mtime(db) } else { None });
            if mtime.is_some() && gate != last_harvest_mtime {
                last_harvest_mtime = gate;
                bootstrap::ingest(&write_store, db, chrono::Utc::now().timestamp());
            }
        }
        // Codex 那一路（同样不看绑定,理由同上;它连 collector.db 都不需要——读数与
        // 代价都在 rollout 的同一行上）。闸门只 stat 不读内容。
        //
        // 它还会**零请求地推进 Codex 快照**：rollout 里的 rate_limits 与 usage 端点
        // 同源同精度,只是走本地文件到手。真改了就立刻广播,不等下面的取数轮
        // ——用贵模型时一个轮次能吃掉 5h 窗十几二十个点,那正是最不该显示旧数的时候。
        //
        // 闸门是 `（mtime, 总字节数)` 而**不是 mtime**：Windows 上正在被追加的 rollout
        // 文件 mtime 不动（见 `codex_rollout:rollout_files`）,只看 mtime 会让这条路在
        // 当前会话上一直不触发,显示只能靠 API 轮询顶着、落后数个百分点。
        {
            let gen = codex_rollout::generation();
            if gen.is_some() && gen != last_rollout_gen {
                last_rollout_gen = gen;
                let (_, _, snapshot_changed) =
                    codex_rollout::ingest(&write_store, chrono::Utc::now().timestamp());
                if snapshot_changed {
                    emit_changed(&app);
                }
            }
        }

        let bound = write_store.bound_platforms();
        idle::prune(&bound);
        demand::prune(&bound);
        let mut changed_any = false;
        if !bound.is_empty() {
            let base = poll_secs();
            for platform in &bound {
                // 每个平台开检时重取时刻：上一个平台的取数可能在网络上耗了数秒到
                // 数十秒,拿轮首的旧时刻去判到期 / 排下一轮会整体偏早。
                let now = chrono::Utc::now().timestamp();
                // 取数来源：
                // - wake:手动刷新 / 绑定 / 间隔变更的全量轮;
                // - token:本地 token 驱动（demand.rs）已到应检时刻;
                // - fallback:兜底轮（覆盖不产生本地 token 的在线 / 网页用量）。
                let by_token = demand::due_now(*platform, now);
                let by_fallback = idle::due(*platform, now);
                if !woke && !by_token && !by_fallback {
                    continue;
                }
                let via = if woke {
                    "wake"
                } else if by_token {
                    "token"
                } else {
                    "fallback"
                };

                // 凭据文件 mtime 探针:变化 → 清内存 token 缓存（判死自愈入口;
                // 无变化且未判死 → 走缓存/现读）。纳秒级 stat,无网络。
                let mtime = credentials::credential_mtime(*platform);
                let changed = last_mtime.get(platform) != Some(&mtime);
                if changed {
                    // 只清**该平台自己**的适配器：`ClaudeAdapter:invalidate` 顺带清
                    // 429 冷却闸,对两个适配器都调会让 Codex 凭据续期（CLI 每次刷新
                    // 都重写 auth.json）清掉 Claude 的限流退避。
                    match platform {
                        Platform::Codex => adapters.codex.invalidate(*platform),
                        Platform::Claude => adapters.claude.invalidate(*platform),
                    }
                    // **账号纪元**：换账号就是改这个文件,所以只在它真的动了的时候读一次,
                    // 把账号指纹记下来（Codex 的 auth.json 带 `tokens.account_id`;
                    // Claude 的凭据没有等价字段,`account_fp` 是 None,note_account 不动）。
                    if let Some(fp) =
                        credentials::read_credential(*platform).and_then(|c| c.account_fp)
                    {
                        write_store.note_account(*platform, &fp, now);
                    }
                }
                last_mtime.insert(*platform, mtime);

                // 判死态:仅当文件刚变化（用户重新登录/CLI 续期）才复活重试,
                // 否则**零网络**。但快照仍要落成 auth_failed
                // ——判死是「不试」,不是「UI 停在旧结论」。
                let dead = match platform {
                    Platform::Codex =>
                        matches!(adapters.codex.obtain_access(*platform), codex::Access::Dead),
                    Platform::Claude => adapters.claude.is_dead(*platform),
                };
                let prev = write_store.load_snapshot(*platform);
                // 快照
                let snap = if dead && !changed {
                    // 判死零网络,但桌面端采样是本地文件,照读（Claude 专属回落）
                    let mut note = "";
                    let snap = with_desktop_fallback(dead_snapshot(*platform), &mut note);
                    let note = if note.is_empty() { " skipped=dead" } else { " skipped=dead source=desktop" };
                    log_fetch(via, *platform, &adapters, &snap, note);
                    let _ = write_store.save_snapshot(&snap);
                    snap
                } else if let Some(quiet) = skip_by_desktop_probe(via, *platform, prev.as_ref(), now) {
                    // 兜底轮 + 桌面端采样证明上次读数之后没涨 → 零网络。
                    // 快照不是「原样保留」而是「把样本里的下降正进去」（滚动窗口的
                    // 余量恢复,见 apply_flat_sample）——真改了才落库,没改时 save 是等价
                    // 写入,下面的 changed_any 仍判不出变化,不会白广播。
                    let note = if prev.as_ref().is_some_and(|p| &quiet != p) {
                        " skipped=desktop-flat corrected=desktop-sample"
                    } else {
                        " skipped=desktop-flat"
                    };
                    log_fetch(via, *platform, &adapters, &quiet, note);
                    let _ = write_store.save_snapshot(&quiet);
                    quiet
                } else {
                    // 取数互斥：与命令面的立即刷新共用同一把平台锁,
                    // 防两处同时进入 fetch_one（冷却检查与 429 冷却写入之间无
                    // 原子性,各发一请求会加速触限）。
                    let _lease = adapters.lock_fetch(*platform);
                    let snap = fetch_one(&adapters, *platform, via);
                    drop(_lease);
                    // 响应顶层的 `account_id` 是最权威的一路——服务端把这次用量记在谁
                    // 头上,与读数同一个响应、零额外请求（见 codex:SEEN_ACCOUNT）。
                    if let Some(fp) = codex::take_seen_account() {
                        write_store.note_account(Platform::Codex, &fp, now);
                    }
                    let _ = write_store.save_snapshot(&snap);
                    snap
                };
                // 「拿到了**更新的**读数」——三条分支同一判据：判死轮也可能靠
                // 桌面端采样拿到新样本,那同样是一次真实读数。取「更新」而非「不同」:
                // 桌面端样本可能比上次 API 读数更旧,那不算进展。
                let advanced = match (snap.fetched_at, prev.as_ref().and_then(|p| p.fetched_at)) {
                    (Some(t1), Some(t0)) => t1 > t0,
                    (Some(_), None) => true,
                    _ => false,
                };
                // 取数结束的真实时刻（取数可能耗了数十秒;记账与排下一轮都按它算）
                let done_at = chrono::Utc::now().timestamp();
                // 有进展才清账并落一条标定样本（失败轮不清,下轮照样该取）
                if advanced {
                    record_pair(&write_store, *platform, prev.as_ref(), &snap, done_at);
                }
                // 尝试记进账目：最小间隔从**尝试**时刻起算,没进展则把应检时刻
                // 按退避推后（否则故障期间每个采集轮都会重试一次,见 demand.rs）。
                demand::note_attempt(*platform, done_at, advanced);
                // 兜底应检时刻按设置档顺延（待机不改变取数频次）
                idle::schedule_next(*platform, base, done_at);
                // 广播按**落库后真的变了**判（失败轮只推进 status、安静轮原样保留,
                // 见 store:save_snapshot）：无变化的轮广播出去,前端每轮都要白跑一次
                // 查询与重渲染。
                changed_any |= write_store.load_snapshot(*platform) != prev;
            }
            if changed_any {
                emit_changed(&app);
            }
        }

        let now = chrono::Utc::now().timestamp();
        // 待机判据 = 安静起点距今满 STANDBY_QUIET_SECS（idle.rs）:每次醒来重算一次,翻转即广播
        if idle::evaluate(now) {
            idle::emit_idle(&app);
            crate::dev_log!("[subscription] standby toggled");
        }

        // 可中断睡眠（wake 提前返回）：睡到最近的应检时刻与最近的待机判定时刻里
        // 较早的那个;无绑定时仍按基础档空转睡眠（零网络）。
        // ⚠ 代际读取与 wait 必须**同一把锁贯穿**：分两次 lock 会在解锁间隙丢 wake 通知
        // （notify 无等待者即失效,唤醒最长被吞一个兜底档）——读代际、算时长、进等待三步
        // 之间不得释放 WAKE.0。
        let guard = WAKE.0.lock().unwrap_or_else(|e| e.into_inner());
        let gen_before = *guard;
        // 收割节律参与封顶（零网络,见 HARVEST_SECS）：兜底档再长也不至于让
        // 桌面端样本在库外压半小时。无绑定时同样照睡 HARVEST_SECS——收割不看绑定。
        let wait = idle::next_wait_secs(now, poll_secs()).min(HARVEST_SECS);
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
/// 绑定前线程空转睡眠,**零网络**。
pub fn spawn(app: AppHandle) -> Result<(), String> {
    let root = crate::data_root::current(&app)?;
    std::fs::create_dir_all(&root.root).map_err(|e| e.to_string())?;
    let write_store = SubStore::open(&root.subscriptions_db_path())?;
    let reader = Arc::new(Mutex::new(SubStore::open(&root.subscriptions_db_path())?));

    let adapters = Arc::new(Adapters {
        codex: codex::CodexAdapter::new(),
        claude: claude::ClaudeAdapter::new(),
        fetch_locks: [Mutex::new(()), Mutex::new(())],
    });

    // 价目索引装载（取数路径零 IO;库里的行由 SubStore:open 的幂等 upsert 保证是最新
    // 出厂种子 + 更早版本留下的历史生效段）。**必须排在重算之前**——重算要按库里的价目,
    // 而不是编译期种子那份兜底。
    price::load_from(&write_store);

    // 价格数据集升订号后的**就地重算**（价格变更的落点）：按 breakdown 里的原始 token,
    // **每个模型按它在该区间 t1 时刻生效的价目**把存量行的 cost 重算一遍。订号没变时
    // 只是一次索引查询。必须排在标定装载之前——否则这一轮会拿旧尺子量出来的 cost 先拟合一次。
    for platform in [Platform::Codex, Platform::Claude] {
        if let Err(e) = write_store.recompute_stale_costs(platform) {
            crate::dev_log!("[subscription] {} cost recompute failed: {e}", platform.as_str());
        }
    }
    // 标定系数启动装载（排除存疑行 + 按当前套餐筛,口径见 store:pairs_for_fit;
    // 空库 = 出厂预设）
    for platform in [Platform::Codex, Platform::Claude] {
        calib::refit_from_store(&write_store, platform);
    }

    app.manage(SubscriptionReader(reader));

    std::thread::Builder::new()
        .name("subscription".into())
        .spawn(move || run(app, write_store, adapters))
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------- 命令面 ----------

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
            source: SnapshotSource::Api,
        });
    }
    wake();
    emit_changed(&app);
    Ok(())
}

/// 手动立即刷新（右键菜单/设置页按钮）。手动刷新 = 用户注意到额度 → 先退出待机,
/// 再全量取数;注意在前,唤醒轮的无变化只计第 1 轮安静。
///
/// **最小间隔 `MANUAL_REFRESH_MIN_GAP_SECS`**：被挡下的那次不推进代际（= 不触发全量轮,
/// 两个平台各省一次请求）,但退待机与广播照发,前端的刷新三态动画正常落地。只挡手动
/// 这一路——bind / unbind / 改档的 `wake` 不是连点面,不受此限。
#[tauri::command]
pub fn refresh_subscriptions_now(app: AppHandle) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    nudge_standby(&app);
    let now = chrono::Utc::now().timestamp();
    let last = LAST_MANUAL_REFRESH.load(Ordering::SeqCst);
    if now - last >= MANUAL_REFRESH_MIN_GAP_SECS {
        LAST_MANUAL_REFRESH.store(now, Ordering::SeqCst);
        wake();
    } else {
        crate::dev_log!(
            "[subscription] manual refresh throttled ({}s since last, min {}s)",
            now - last,
            MANUAL_REFRESH_MIN_GAP_SECS
        );
    }
    emit_changed(&app);
    Ok(())
}

/// 取数策略（设置页 Subscriptions tab）：预计消耗达到 `threshold_pct` 就取一次读数;
/// `tighten_when_low` = 5h 剩余 ≤ `demand:LOW_REMAINING_PCT` 时阈值减半。持久化在前端
/// designPrefs,本命令只改运行时值（窗口装载时恢复,与 set_subscription_poll_secs 同款）。
#[tauri::command]
pub fn set_subscription_fetch_policy(threshold_pct: f64, tighten_when_low: bool) -> Result<(), String> {
    // 幂等早退：装载恢复 + 设置页直调会重复下发同一值
    if (demand::threshold_pct() - threshold_pct).abs() < 1e-9
        && demand::tighten_when_low() == tighten_when_low
    {
        return Ok(());
    }
    let (pct, tighten) = demand::set_policy(threshold_pct, tighten_when_low);
    crate::dev_log!("[subscription] fetch policy: threshold={pct}% tighten_when_low={tighten}");
    Ok(())
}

/// 估算器诊断（设置页状态行;只读,不触发任何网络）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct EstimatorState {
    pub platform: Platform,
    /// 是否已按校准（样本数达门槛;false = 仍以出厂预设权重为主）。
    pub calibrated: bool,
    /// 参与拟合的有效样本数。
    pub pairs: u32,
    /// 距上次读数的预计消耗（百分点）。
    pub est_pct_since_fetch: f64,
    /// 当前归一化系数（**百分点 / 美元当量**）：1 美元的官方 API 当量吃掉多少配额。
    /// 取倒数就是「1% 配额 ≈ 多少美元」——Insights 价格面板用它把「相当于多少钱」
    /// 与「还剩多少额度」接上。未装载过样本时 = 出厂预设。
    pub scale: f64,
    /// 已收割留存的桌面端采样条数（仅 Claude 有源;0 = 本机没有桌面端采样文件）。
    /// 桌面端自己只保约 14 天,这个数会越过那条线继续涨——它就是「样本密度」。
    pub desktop_samples: i64,
    /// 「本机解释不了的消耗」证据（多机 / 网页 / 手机 App 的信号,语义
    /// [`bootstrap:ForeignEvidence`];无源的平台恒为全零）。
    pub foreign: bootstrap::ForeignEvidence,
}

#[tauri::command]
pub fn get_subscription_estimator(
    state: tauri::State<'_, SubscriptionReader>,
) -> Result<Vec<EstimatorState>, String> {
    let store = state.0.lock().unwrap_or_else(|e| e.into_inner());
    Ok([Platform::Codex, Platform::Claude]
        .into_iter()
        .map(|p| {
            let pairs = calib::sample_count(p);
            EstimatorState {
                platform: p,
                calibrated: pairs >= calib::CALIBRATED_PAIRS,
                pairs,
                est_pct_since_fetch: demand::estimated_pct(p),
                scale: calib::scale(p),
                desktop_samples: store.sample_count(p),
                foreign: bootstrap::evidence(p),
            }
        })
        .collect())
}

/// 设置**兜底**取数间隔（秒;设置页 Subscriptions tab 5/10/15/30 分钟下拉）。
/// 常规取数由本地 token 驱动,此值只决定「本地无 token 时多久兜底取一轮」。
/// 存 prefs 侧的持久化由前端 designPrefs 管理（subscriptionPollSecs 键）,
/// 此处只改运行时值——重启后前端初查时再调用本命令恢复。
#[tauri::command]
pub fn set_subscription_poll_secs(secs: u64) -> Result<(), String> {
    // 幂等早退：设置页直调 + 装载恢复 effect 会重复下发同一值,
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
