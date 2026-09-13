//! 订阅额度模块：与 collector 平行的独立数据链路。
//!
//! - 数据源：各 agent CLI 的**本机凭据文件**（只读）+ 平台 usage 端点（逆向）;
//! - 存储：`<数据根>/subscriptions.db`（快照 + 绑定开关;凭据永不落库）;
//! - 轮询：独立 daemon 线程（对齐 collector 模式）——默认 5 分钟,
//!   绑定前零网络,凭据判死后零网络（mtime 自愈探针复活）;
//! - 红线：凭据文件只读不写,刷新 token 只存内存;单平台失败不拖垮整体。

pub mod claude;
pub mod codex;
pub mod credentials;
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
    let mut gen = WAKE.0.lock().unwrap();
    *gen += 1;
    WAKE.1.notify_all();
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
    /// 剩余冷却秒数（None = 未冷却）。
    pub fn remaining(&self, now: i64) -> Option<i64> {
        self.0
            .lock()
            .unwrap()
            .map(|until| until - now)
            .filter(|rest| *rest > 0)
    }

    /// 触发限流：从 now 起冷却 secs 秒（多次触发取更晚者）。
    pub fn arm(&self, now: i64, secs: i64) {
        let until = now + secs;
        let mut g = self.0.lock().unwrap();
        if g.map(|cur| until > cur).unwrap_or(true) {
            *g = Some(until);
        }
    }

    pub fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }
}

// ---------- 采集编排 ----------

fn fetch_one(adapters: &Adapters, platform: Platform) -> SubscriptionSnapshot {
    // 冷却期内不碰网络（限流退避,静默保留旧数据）
    let cooling = match platform {
        Platform::Codex => adapters.codex.cooldown_remaining(),
        Platform::Claude => adapters.claude.cooldown_remaining(),
    };
    if cooling.is_some() {
        return rate_limited_snapshot(platform);
    }

    match platform {
        Platform::Codex => match adapters.codex.obtain_access(platform) {
            codex::Access::Ok(cred) => codex::fetch(&adapters.codex, cred),
            codex::Access::Dead => dead_snapshot(platform),
            codex::Access::None => idle_snapshot(platform),
            codex::Access::Transient => transient_snapshot(platform),
        },
        Platform::Claude => match adapters.claude.obtain_access(platform) {
            claude::Access::Ok(cred) => claude::fetch(&adapters.claude, cred),
            claude::Access::Dead => dead_snapshot(platform),
            claude::Access::None => idle_snapshot(platform),
        },
    }
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

    loop {
        let bound = write_store.bound_platforms();
        if !bound.is_empty() {
            for platform in bound {
                // 凭据文件 mtime 探针:变化 → 清内存 token 缓存（判死自愈入口;
                // 无变化且未判死 → 走缓存/现读）。纳秒级 stat,无网络。
                let mtime = credentials::credential_mtime(platform);
                let changed = last_mtime.get(&platform) != Some(&mtime);
                if changed {
                    adapters.codex.invalidate(platform);
                    adapters.claude.invalidate(platform);
                }
                last_mtime.insert(platform, mtime);

                // 判死态:仅当文件刚变化（用户重新登录/CLI 续期）才复活重试,
                // 否则**零网络**（预案）。但快照仍要落成 auth_failed
                // ——判死是「不试」,不是「UI 停在旧结论」。
                let dead = match platform {
                    Platform::Codex =>
                        matches!(adapters.codex.obtain_access(platform), codex::Access::Dead),
                    Platform::Claude => adapters.claude.is_dead(platform),
                };
                let snap = if dead && !changed {
                    dead_snapshot(platform)
                } else {
                    fetch_one(&adapters, platform)
                };
                let _ = write_store.save_snapshot(&snap);
            }
            emit_changed(&app);
        }

        // 可中断睡眠（wake 提前返回;无绑定时同样睡眠——零网络）
        let gen = WAKE.0.lock().unwrap();
        let (guard, timeout) = WAKE
            .1
            .wait_timeout(gen, Duration::from_secs(poll_secs()))
            .unwrap();
        let _ = (guard, timeout);
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
    let reader = SubStore::open(&root.subscriptions_db_path())?;

    let adapters = Arc::new(Adapters {
        codex: codex::CodexAdapter::new(),
        claude: claude::ClaudeAdapter::new(),
        dead_mtime: Mutex::new(std::collections::HashMap::new()),
    });

    app.manage(SubscriptionReader(Arc::new(Mutex::new(reader))));
    app.manage(SubscriptionAdapters(adapters.clone()));

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
    credentials::read_credential(platform)
        .ok_or_else(|| "credential file not found or unparseable".to_string())?;
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

/// 手动立即刷新（右键菜单/设置页按钮）。
#[tauri::command]
pub fn refresh_subscriptions_now(app: AppHandle) -> Result<(), String> {
    wake();
    emit_changed(&app);
    Ok(())
}

/// 设置轮询间隔（秒;设置页 Subscriptions tab 5/10/15/30 分钟下拉）。
/// 存 prefs 侧的持久化由前端 designPrefs 管理（subscriptionPollSecs 键）,
/// 此处只改运行时值——重启后前端初查时再调用本命令恢复。
#[tauri::command]
pub fn set_subscription_poll_secs(secs: u64) -> Result<(), String> {
    set_poll_secs(secs);
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
