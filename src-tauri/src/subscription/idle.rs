//! 待机监控（订阅主轮询的自适应退档）。
//!
//! 定位：boost 的反向补充——读数长期无变化时放慢主
//! 轮询,静置期降低网络与视觉占用：
//! - 每平台独立档位,从设置的基础间隔起步;成功轮询且读数与上次一致 → 翻倍
//!   退一档（5m → 10m → 20m → 30m 封顶 `MAX_IDLE_SECS`）;
//! - 任何读数变化（套餐/状态/used）→ 立即回基础档;
//! - 失败轮（限流/网络/凭据失效,fetched_at 不推进）不评变化、保档——
//!   「没测出来」不是「没变化」;
//! - 变化判据**排除 resets_at**：Codex 的滚动窗口尾每次取数都后漂（假时刻,
//!   见 OrbWindow windowIdle 注）,计入判据待机永不成立;used 带 0.01% 容差
//!   比较,亚档抖动不算变化;
//! - 档位退过基础档 = 待机：广播 `subscription:idle`,悬浮球整体减淡 50%
//!   （前端合并 boost 态——高频监控中的平台不变暗）。
//!
//! 档位只存内存（重启回基础档重评）;开关持久化在 designPrefs（orbIdleEnabled,
//! 默认开——退档是收敛行为,与 boost「提频需显式授权」的口径相反）。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, atomic::{AtomicBool, Ordering}};

use serde::Serialize;

use super::model::{FetchStatus, Platform, SubscriptionSnapshot};

/// 待机封顶间隔（最长退到 30 分钟;= 主轮询间隔上限,基础档即为
/// 30 分钟时永不待机——已在最低频）。
pub const MAX_IDLE_SECS: u64 = 1800;

static ENABLED: AtomicBool = AtomicBool::new(true);

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// 读数内容指纹（变化判据;fetched_at/resets_at 刻意排除,见模块注释）。
/// used 比较带容差——上游百分比的亚档抖动（浮点/采样噪声）不算变化。
#[derive(Debug)]
struct Content {
    plan: String,
    status: FetchStatus,
    /// （kind, used_percent)。
    windows: Vec<(String, f64)>,
}

/// used 比较容差（百分点;差值低于此视为同一读数）。
const CONTENT_EPS: f64 = 0.01;

impl PartialEq for Content {
    fn eq(&self, other: &Self) -> bool {
        self.plan == other.plan
            && self.status == other.status
            && self.windows.len() == other.windows.len()
            && self
                .windows
                .iter()
                .zip(other.windows.iter())
                .all(|(a, b)| a.0 == b.0 && (a.1 - b.1).abs() < CONTENT_EPS)
    }
}

impl Content {
    fn of(snap: &SubscriptionSnapshot) -> Self {
        Content {
            plan: snap.plan_type.clone(),
            status: snap.status,
            windows: snap
                .windows
                .iter()
                .map(|w| (w.kind.clone(), w.used_percent))
                .collect(),
        }
    }
}

/// 单平台退档状态机（纯逻辑,不含 IO——单测直接覆盖）。
#[derive(Debug)]
struct Track {
    /// 基础档（设置页轮询间隔;变更即回档重评）。
    base: u64,
    /// 当前档位（≥ base,退档时翻倍增长）。
    tier: u64,
    /// 下次应检时刻（unix 秒）。
    next_due: i64,
    /// 上次成功读数指纹（None = 尚无基线）。
    last: Option<Content>,
    /// 是否处于待机（tier > base）。
    standby: bool,
}

impl Track {
    fn new(base: u64, now: i64) -> Self {
        Track {
            base,
            tier: base,
            next_due: now + base as i64,
            last: None,
            standby: false,
        }
    }

    /// 退一档：翻倍并封顶（5m → 10m → 20m → 30m;已达封顶则原地不动）。
    fn step(&mut self) {
        self.tier = self.tier.saturating_mul(2).min(MAX_IDLE_SECS);
    }

    /// 一轮观测。返回待机旗标是否翻转（翻转 = 需要广播 subscription:idle）。
    /// next_due 一律按**评估后的档位**顺延（退档/回档当轮即生效,不吃一拍延迟）。
    fn observe(&mut self, snap: &SubscriptionSnapshot, base: u64, now: i64) -> bool {
        if self.base != base {
            // 基础档变了（设置页调整）:档位回新基础档重新评估
            self.base = base;
            self.tier = base;
            self.next_due = now + self.tier as i64;
        }
        if snap.fetched_at.is_none() {
            // 失败轮:保档不评（限流冷却/网络失败轮没有新读数）,应检时刻顺延
            self.next_due = now + self.tier as i64;
            return false;
        }
        let content = Content::of(snap);
        let changed = self.last.as_ref().map_or(true, |prev| *prev != content);
        self.last = Some(content);
        let was = self.standby;
        if changed {
            self.tier = self.base;
        } else {
            self.step();
        }
        self.standby = self.tier > self.base;
        self.next_due = now + self.tier as i64;
        self.standby != was
    }
}

/// 档位表（轮询线程单写多读:写 = 主轮询 observe,读 = 命令面/调度计算）。
static TRACKS: LazyLock<Mutex<HashMap<Platform, Track>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 取档位表（poison 容忍,审计 P3-）：持锁线程 panic 后表内容仍可用,
/// 不让主轮询/命令面连锁停摆。
fn tracks() -> std::sync::MutexGuard<'static, HashMap<Platform, Track>> {
    TRACKS.lock().unwrap_or_else(|e| e.into_inner())
}

/// 该平台是否应检（有轨道看 next_due;无轨道/未启用 → 检）。
pub fn due(platform: Platform, now: i64) -> bool {
    if !is_enabled() {
        return true;
    }
    tracks().get(&platform).map_or(true, |t| now >= t.next_due)
}

/// 主轮询线程一轮观测（成功/失败快照都进;语义见 `Track:observe`）。
/// 返回该平台待机旗标是否翻转。
pub fn observe(platform: Platform, snap: &SubscriptionSnapshot, base_secs: u64, now: i64) -> bool {
    if !is_enabled() {
        // 关闭即摘档:该平台回基础档恒频（曾待机 = 翻转,需广播复位）
        return tracks().remove(&platform).map_or(false, |t| t.standby);
    }
    tracks()
        .entry(platform)
        .or_insert_with(|| Track::new(base_secs, now))
        .observe(snap, base_secs, now)
}

/// 清掉未绑定平台的轨道（解绑后复绑不得拿旧指纹误判「无变化」直接退档）。
pub fn prune(bound: &[Platform]) {
    let mut g = tracks();
    if !is_enabled() {
        g.clear();
        return;
    }
    g.retain(|p, _| bound.contains(p));
}

/// 主线程睡眠时长:所有平台最近的应检时刻;无轨道（未启用/未绑定）→ 基础档。
pub fn next_wait_secs(now: i64, fallback: u64) -> u64 {
    if !is_enabled() {
        return fallback;
    }
    let g = tracks();
    g.values()
        .map(|t| (t.next_due - now).max(1) as u64)
        .min()
        .unwrap_or(fallback)
}

pub fn emit_idle(app: &tauri::AppHandle) {
    use tauri::Emitter;
    let _ = app.emit("subscription:idle", true);
}

// ---------- 命令面（snake_case,契约风格） ----------

/// 单平台待机态（前端 orb 窗口消费;idle = 该平台当前退过基础档）。
#[derive(Debug, Clone, Serialize)]
pub struct PlatformIdleState {
    pub platform: Platform,
    pub idle: bool,
    /// 当前档位（秒;未待机时 = 基础档）。
    pub interval_secs: u64,
}

/// 当前待机态（前端 orb 窗口初查口;事件 `subscription:idle` 翻转后重查）。
#[tauri::command]
pub fn get_subscription_idle() -> Result<Vec<PlatformIdleState>, String> {
    let g = tracks();
    Ok([Platform::Codex, Platform::Claude]
        .into_iter()
        .map(|p| match g.get(&p) {
            Some(t) => PlatformIdleState {
                platform: p,
                idle: t.standby,
                interval_secs: t.tier,
            },
            None => PlatformIdleState {
                platform: p,
                idle: false,
                interval_secs: super::poll_secs(),
            },
        })
        .collect())
}

/// 下发待机开关（持久化由前端 designPrefs 承担,orb 窗口装载时再调本命令恢复
/// ——与 set_subscription_poll_secs 同款）。关闭后立即唤醒主轮询:正在睡的
/// 长档位马上回基础档节奏。
#[tauri::command]
pub fn set_subscription_idle_enabled(enabled: bool) -> Result<(), String> {
    // 幂等早退（审计 P2-）：设置页直调 + orb 桥接跟随会重复下发同一值,
    // 不早退则每次多唤醒主轮询一整轮（两平台各多发一次请求）。
    if is_enabled() == enabled {
        return Ok(());
    }
    ENABLED.store(enabled, Ordering::SeqCst);
    if !enabled {
        tracks().clear();
    }
    super::wake();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription::model::{FetchStatus, QuotaWindow};

    fn snap(fetched: i64, used_5h: f64) -> SubscriptionSnapshot {
        SubscriptionSnapshot {
            platform: Platform::Codex,
            plan_type: "pro".into(),
            windows: vec![QuotaWindow {
                kind: "5h".into(),
                used_percent: used_5h,
                resets_at: None,
            }],
            fetched_at: Some(fetched),
            status: FetchStatus::Ok,
        }
    }

    #[test]
    fn unchanged_steps_ladder_and_caps() {
        let mut t = Track::new(300, 0);
        assert!(!t.observe(&snap(100, 5.0), 300, 0), "首轮建基线,档位 = 基础档");
        assert_eq!(t.tier, 300);
        assert!(t.observe(&snap(400, 5.0), 300, 400), "5m → 10m,首次进入待机(翻转)");
        assert_eq!(t.tier, 600);
        assert!(!t.observe(&snap(1000, 5.0), 300, 1000), "10m → 20m(已待机,无翻转)");
        assert_eq!(t.tier, 1200);
        assert!(!t.observe(&snap(2200, 5.0), 300, 2200), "20m → 30m 封顶");
        assert_eq!(t.tier, MAX_IDLE_SECS);
        assert!(t.standby);
    }

    #[test]
    fn change_restores_base_and_flips() {
        let mut t = Track::new(300, 0);
        t.observe(&snap(100, 5.0), 300, 0);
        t.observe(&snap(400, 5.0), 300, 400); // 待机 10m
        assert!(t.observe(&snap(1000, 18.0), 300, 1000), "+13% 变化 → 回基础档(翻转)");
        assert_eq!(t.tier, 300);
        assert!(!t.standby);
    }

    #[test]
    fn failed_round_holds_tier_and_baseline() {
        let mut t = Track::new(300, 0);
        t.observe(&snap(100, 5.0), 300, 0);
        t.observe(&snap(400, 5.0), 300, 400); // 待机 10m
        let failed = SubscriptionSnapshot {
            platform: Platform::Codex,
            plan_type: "unknown".into(),
            windows: vec![],
            fetched_at: None,
            status: FetchStatus::NetworkFailed,
        };
        assert!(!t.observe(&failed, 300, 1000), "失败轮:保档不评");
        assert_eq!(t.tier, 600);
        assert!(!t.observe(&snap(1300, 5.0), 300, 1300), "基线未作废:仍无变化 → 退档");
        assert_eq!(t.tier, 1200);
    }

    #[test]
    fn resets_at_drift_is_not_a_change() {
        // Codex 滚动窗口尾每次取数后漂(假时刻)——不得计入变化判据
        let mut t = Track::new(300, 0);
        let mut a = snap(100, 5.0);
        a.windows[0].resets_at = Some(1000);
        let mut b = snap(400, 5.0);
        b.windows[0].resets_at = Some(3700);
        t.observe(&a, 300, 0);
        t.observe(&b, 300, 400); // 仅 resets_at 漂移 → 无变化退档
        assert_eq!(t.tier, 600);
    }

    #[test]
    fn sub_percent_jitter_is_not_a_change() {
        let mut t = Track::new(300, 0);
        t.observe(&snap(100, 5.004), 300, 0);
        t.observe(&snap(400, 5.008), 300, 400); // <0.01% 抖动 → 无变化退档
        assert_eq!(t.tier, 600);
    }

    #[test]
    fn plan_change_restores() {
        let mut t = Track::new(300, 0);
        t.observe(&snap(100, 5.0), 300, 0);
        t.observe(&snap(400, 5.0), 300, 400); // 待机
        let mut upgraded = snap(1000, 5.0);
        upgraded.plan_type = "max".into();
        assert!(t.observe(&upgraded, 300, 1000), "套餐变化 → 回基础档(翻转)");
        assert_eq!(t.tier, 300);
    }

    #[test]
    fn base_change_resets_tier() {
        let mut t = Track::new(300, 0);
        t.observe(&snap(100, 5.0), 300, 0);
        t.observe(&snap(400, 5.0), 300, 400); // 待机 10m
        // 基础档改 10m + 数据变化 → 档位回新基础档,退出待机
        assert!(t.observe(&snap(1000, 18.0), 600, 1000));
        assert_eq!(t.tier, 600);
        assert!(!t.standby);
    }

    #[test]
    fn static_surface_due_wait_and_disable() {
        // 静态表面(due/next_wait/开关)——其余单测走纯 Track,不碰静态表
        assert!(due(Platform::Codex, 0), "无轨道 = 应检");
        assert_eq!(next_wait_secs(0, 300), 300, "无轨道 → 基础档");
        observe(Platform::Codex, &snap(100, 5.0), 300, 0);
        observe(Platform::Codex, &snap(400, 5.0), 300, 400); // 10m 待机,next_due=1000
        assert!(!due(Platform::Codex, 500), "未到期不检");
        assert!(due(Platform::Codex, 1000), "到期必检");
        assert_eq!(next_wait_secs(400, 300), 600);
        assert_eq!(next_wait_secs(1001, 300), 1, "逾期 → 至少醒 1s");

        let _ = set_subscription_idle_enabled(false); // 关闭:清表 + 回基础档
        assert!(due(Platform::Codex, 0));
        assert_eq!(next_wait_secs(0, 300), 300);
        assert!(get_subscription_idle().unwrap().iter().all(|s| !s.idle));

        // 重开 → 退档 → 待机态可见;再关 → 全部复位
        let _ = set_subscription_idle_enabled(true);
        observe(Platform::Codex, &snap(100, 5.0), 300, 0);
        observe(Platform::Codex, &snap(400, 5.0), 300, 400);
        assert!(get_subscription_idle()
            .unwrap()
            .iter()
            .any(|s| s.platform == Platform::Codex && s.idle));
        let _ = set_subscription_idle_enabled(false);
        assert!(get_subscription_idle().unwrap().iter().all(|s| !s.idle));
        let _ = set_subscription_idle_enabled(true); // 收尾恢复默认(开)
    }
}
