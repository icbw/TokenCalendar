//! 待机监控（订阅主轮询的自适应退档）。
//!
//! 定位：待机只为一件事——
//! 用户离开 agent 开发工作时,尽量降低常驻贴边条的视觉干扰;用户注意到它或回到
//! 工作时,立刻退出。
//!
//! 进入（每平台独立判定,前端再取「全部已绑定平台待机且 无 boost」）:
//! - 连续 `STANDBY_MIN_ROUNDS`（3）轮**成功**取数读数无变化,**且**距上次变化
//!   / 上次注意 ≥ `STANDBY_MIN_QUIET_SECS`（10 分钟）——两条同时满足。
//!   轮数防「唤醒轮扎堆」（刷新/boost 退出/绑定的全量轮几分钟内凑满轮数）,
//!   时长下限防「睡眠唤醒后一轮定论」（时间够了但只有一次读数）;
//! - 失败轮（限流/网络/凭据失效,fetched_at 不推进）不计轮数、不评变化——
//!   「没测出来」不是「安静」;
//! - 变化判据**排除 resets_at**：Codex 的滚动窗口尾每次取数都后漂（假时刻,
//!   见 OrbWindow windowIdle 注）,计入判据待机永不成立;used 带 0.01% 容差
//!   比较,亚档抖动不算变化。
//!
//! 放慢：进入待机后才开始翻倍退档（5m → 10m → 20m → 30m 封顶 `MAX_IDLE_SECS`）。
//!
//! 退出（任一即可,退出 = 清零安静计数 + 回基础档）:
//! - 读数变化（主轮询观测）;
//! - 本地 agent 新活动（采集线程,`note_attention`;对应订阅平台原在待机时立即补取）;
//! - 用户注意（手动刷新 / 展开 / 切换平台,命令面 `note_subscription_attention`
//!   与 `refresh_subscriptions_now`）。
//!
//! 翻转广播 `subscription:idle`,悬浮球整体减淡 50%。档位只存内存（重启回基础档
//! 重评）;开关持久化在 designPrefs（orbIdleEnabled,默认开——退档是收敛行为,
//! 与 boost「提频需显式授权」的口径相反）。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, atomic::{AtomicBool, Ordering}};

use serde::Serialize;

use super::model::{FetchStatus, Platform, SubscriptionSnapshot};

/// 待机封顶间隔（最长退到 30 分钟;= 主轮询间隔上限）。
pub const MAX_IDLE_SECS: u64 = 1800;

/// 进入待机的最少连续无变化成功轮数。
pub const STANDBY_MIN_ROUNDS: u32 = 3;

/// 进入待机的最短安静时长。
pub const STANDBY_MIN_QUIET_SECS: i64 = 600;

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

/// 单平台待机状态机（纯逻辑,不含 IO——单测直接覆盖）。
#[derive(Debug)]
struct Track {
    /// 基础档（设置页轮询间隔;变更即回档重评）。
    base: u64,
    /// 当前档位（≥ base;只在待机中翻倍增长）。
    tier: u64,
    /// 下次应检时刻（unix 秒）。
    next_due: i64,
    /// 上次成功读数指纹（None = 尚无基线）。
    last: Option<Content>,
    /// 连续无变化的成功轮数（变化 / 注意清零;失败轮不计）。
    quiet_rounds: u32,
    /// 安静起点（unix 秒;上次读数变化 / 注意 / 建轨时刻）。
    quiet_since: i64,
    /// 是否处于待机（轮数与时长同时达标后置位,退出条件任一命中复位）。
    standby: bool,
}

impl Track {
    fn new(base: u64, now: i64) -> Self {
        Track {
            base,
            tier: base,
            next_due: now + base as i64,
            last: None,
            quiet_rounds: 0,
            quiet_since: now,
            standby: false,
        }
    }

    /// 退一档：翻倍并封顶（5m → 10m → 20m → 30m;已达封顶则原地不动）。
    fn step(&mut self) {
        self.tier = self.tier.saturating_mul(2).min(MAX_IDLE_SECS);
    }

    /// 安静计数清零 + 回基础档 + 退出待机（读数变化与注意共用）。
    fn wake_up(&mut self, now: i64) {
        self.quiet_rounds = 0;
        self.quiet_since = now;
        self.tier = self.base;
        self.standby = false;
    }

    /// 一轮观测。返回待机旗标是否翻转（翻转 = 需要广播 subscription:idle）。
    /// next_due 一律按**评估后的档位**顺延（退档/回档当轮即生效,不吃一拍延迟）。
    fn observe(&mut self, snap: &SubscriptionSnapshot, base: u64, now: i64) -> bool {
        if self.base != base {
            // 基础档变了（设置页调整）:档位回新基础档,待机中的后续无变化轮再退档
            self.base = base;
            self.tier = base;
        }
        if snap.fetched_at.is_none() {
            // 失败轮:不计轮数、不评变化（限流冷却/网络失败轮没有新读数）,应检时刻顺延
            self.next_due = now + self.tier as i64;
            return false;
        }
        let content = Content::of(snap);
        let changed = self.last.as_ref().map_or(true, |prev| *prev != content);
        self.last = Some(content);
        let was = self.standby;
        if changed {
            self.wake_up(now);
        } else {
            self.quiet_rounds = self.quiet_rounds.saturating_add(1);
            if !self.standby
                && self.quiet_rounds >= STANDBY_MIN_ROUNDS
                && now - self.quiet_since >= STANDBY_MIN_QUIET_SECS
            {
                self.standby = true;
            }
            if self.standby {
                // 进入待机当轮即开始放慢
                self.step();
            }
        }
        self.next_due = now + self.tier as i64;
        self.standby != was
    }

    /// 用户注意 / 本地 agent 活动：清零安静计数、退出待机、回基础档;应检时刻
    /// 最迟拉回到 now + base（深档时一觉可能还剩 30 分钟）。`fetch_now` = 立即应检。
    /// 读数基线保留——下一轮无变化照常计为第 1 轮安静。
    /// 返回 （待机翻转, 应检时刻提前)。
    fn attend(&mut self, now: i64, fetch_now: bool) -> (bool, bool) {
        let was = self.standby;
        self.wake_up(now);
        let due = if fetch_now { now } else { now + self.base as i64 };
        let pulled = due < self.next_due;
        if pulled {
            self.next_due = due;
        }
        (was, pulled)
    }
}

/// 档位表（写 = 主轮询 observe / 注意入口,读 = 命令面/调度计算）。
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

/// 注意结果：`flipped` = 有平台退出待机（需广播 subscription:idle）;
/// `rescheduled` = 有平台应检时刻提前（需唤醒主轮询重算睡眠,见 `super:reschedule`）。
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Attention {
    pub flipped: bool,
    pub rescheduled: bool,
}

/// 用户注意 / 本地 agent 活动：**全部**平台清零安静计数并退出待机（待机是全局
/// 视觉态,任何 agent 在用 = 用户在工作）。`fetch_now` = 该订阅平台原在待机时
/// 立即补取一轮（本地活动映射到的平台;原本不在待机的保持基础档节奏,不额外发请求）。
pub fn note_attention(now: i64, fetch_now: Option<Platform>) -> Attention {
    let mut out = Attention::default();
    if !is_enabled() {
        return out;
    }
    for (platform, t) in tracks().iter_mut() {
        let fetch = fetch_now == Some(*platform) && t.standby;
        let (was, pulled) = t.attend(now, fetch);
        out.flipped |= was;
        out.rescheduled |= pulled;
    }
    out
}

/// 清掉未绑定平台的轨道（解绑后复绑不得拿旧指纹误判「无变化」计入安静）。
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

/// 单平台待机态（前端 orb 窗口消费;idle = 该平台已进入待机）。
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

/// 用户注意到悬浮球（展开 / 切换平台;手动刷新走 `refresh_subscriptions_now`
/// 同一入口）：退出待机,不额外取数——只把深档的应检时刻拉回基础档节奏。
#[tauri::command]
pub fn note_subscription_attention(app: tauri::AppHandle) -> Result<(), String> {
    super::nudge_standby(&app, None);
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

    fn failed() -> SubscriptionSnapshot {
        SubscriptionSnapshot {
            platform: Platform::Codex,
            plan_type: "unknown".into(),
            windows: vec![],
            fetched_at: None,
            status: FetchStatus::NetworkFailed,
        }
    }

    /// 基础档 5m:t=0 建基线,之后每 5 分钟一轮无变化。返回进入待机前的轨道
    /// （t=900 第 3 轮 = 轮数 3 且安静 15 分钟 → 进入待机）。
    fn standby_track() -> Track {
        let mut t = Track::new(300, 0);
        t.observe(&snap(0, 5.0), 300, 0);
        t.observe(&snap(300, 5.0), 300, 300);
        t.observe(&snap(600, 5.0), 300, 600);
        assert!(t.observe(&snap(900, 5.0), 300, 900), "第 3 轮 + 15 分钟 → 进入待机(翻转)");
        t
    }

    #[test]
    fn standby_needs_three_rounds_then_ladder_caps() {
        let mut t = Track::new(300, 0);
        assert!(!t.observe(&snap(0, 5.0), 300, 0), "首轮建基线");
        assert!(!t.observe(&snap(300, 5.0), 300, 300), "第 1 轮安静:不待机");
        assert_eq!(t.tier, 300, "待机前不放慢");
        assert!(!t.observe(&snap(600, 5.0), 300, 600), "第 2 轮(安静 10 分钟,轮数不够)");
        assert_eq!(t.tier, 300);
        assert!(t.observe(&snap(900, 5.0), 300, 900), "第 3 轮 → 待机");
        assert_eq!(t.tier, 600, "进入待机当轮即放慢");
        assert!(!t.observe(&snap(1500, 5.0), 300, 1500), "10m → 20m(已待机,无翻转)");
        assert_eq!(t.tier, 1200);
        assert!(!t.observe(&snap(2700, 5.0), 300, 2700), "20m → 30m 封顶");
        assert_eq!(t.tier, MAX_IDLE_SECS);
        assert!(t.standby);
    }

    #[test]
    fn rounds_alone_are_not_enough() {
        // 唤醒轮扎堆:3 轮无变化挤在 3 分钟内 → 时长下限不满足,不待机
        let mut t = Track::new(300, 0);
        t.observe(&snap(0, 5.0), 300, 0);
        t.observe(&snap(60, 5.0), 300, 60);
        t.observe(&snap(120, 5.0), 300, 120);
        assert!(!t.observe(&snap(180, 5.0), 300, 180), "3 轮但只安静 3 分钟");
        assert!(!t.standby);
        assert_eq!(t.tier, 300);
        // 时长补满后的下一轮无变化 → 待机（轮数早已达标）
        assert!(t.observe(&snap(600, 5.0), 300, 600));
    }

    #[test]
    fn quiet_time_alone_is_not_enough() {
        // 睡眠唤醒:距基线 1 小时,但只有 1 轮成功读数 → 不待机
        let mut t = Track::new(300, 0);
        t.observe(&snap(0, 5.0), 300, 0);
        assert!(!t.observe(&snap(3600, 5.0), 300, 3600), "安静 1 小时但只 1 轮");
        assert!(!t.observe(&snap(3900, 5.0), 300, 3900), "2 轮");
        assert!(t.observe(&snap(4200, 5.0), 300, 4200), "3 轮 → 待机");
    }

    #[test]
    fn change_exits_and_resets_counters() {
        let mut t = standby_track();
        assert!(t.observe(&snap(1500, 18.0), 300, 1500), "+13% 变化 → 退出待机(翻转)");
        assert_eq!(t.tier, 300);
        assert!(!t.standby);
        assert_eq!(t.quiet_rounds, 0);
        // 计数从变化时刻重来:再 2 轮不够
        t.observe(&snap(1800, 18.0), 300, 1800);
        assert!(!t.observe(&snap(2100, 18.0), 300, 2100));
        assert!(t.observe(&snap(2400, 18.0), 300, 2400), "变化后第 3 轮 + 15 分钟 → 再次待机");
    }

    #[test]
    fn failed_rounds_do_not_count_as_quiet() {
        let mut t = Track::new(300, 0);
        t.observe(&snap(0, 5.0), 300, 0);
        t.observe(&snap(300, 5.0), 300, 300); // 第 1 轮
        assert!(!t.observe(&failed(), 300, 600), "失败轮不评");
        assert!(!t.observe(&failed(), 300, 900));
        assert_eq!(t.quiet_rounds, 1, "失败轮不计轮数");
        assert!(!t.observe(&snap(1200, 5.0), 300, 1200), "第 2 轮(安静 20 分钟也不够轮数)");
        assert!(t.observe(&snap(1500, 5.0), 300, 1500), "第 3 轮 → 待机");
    }

    #[test]
    fn failed_round_holds_standby_tier_and_baseline() {
        let mut t = standby_track(); // 待机 10m
        assert!(!t.observe(&failed(), 300, 1500), "失败轮:保档不评");
        assert_eq!(t.tier, 600);
        assert!(!t.observe(&snap(2100, 5.0), 300, 2100), "基线未作废:仍无变化 → 退档");
        assert_eq!(t.tier, 1200);
    }

    #[test]
    fn attention_exits_standby_and_pulls_due() {
        let mut t = standby_track();
        t.observe(&snap(1500, 5.0), 300, 1500); // 20m 档,next_due = 2700
        assert_eq!(t.next_due, 2700);
        assert_eq!(t.attend(1600, false), (true, true), "退出待机 + 应检拉回基础档");
        assert!(!t.standby);
        assert_eq!(t.tier, 300);
        assert_eq!(t.next_due, 1900);
        assert_eq!(t.attend(1650, false), (false, false), "已在基础档:不翻转、不提前");
        assert_eq!(t.next_due, 1900);
        // 基线保留:下一轮无变化计第 1 轮,轮数与时长都从最近一次注意(1650)重算
        assert!(!t.observe(&snap(1950, 5.0), 300, 1950));
        assert!(!t.observe(&snap(2050, 5.0), 300, 2050));
        assert!(!t.observe(&snap(2150, 5.0), 300, 2150), "3 轮但距注意只 500 秒");
        assert!(t.observe(&snap(2250, 5.0), 300, 2250), "时长补满 10 分钟 → 待机");
    }

    #[test]
    fn attention_fetch_now_is_immediately_due() {
        let mut t = standby_track();
        assert_eq!(t.attend(1000, true), (true, true));
        assert_eq!(t.next_due, 1000, "立即应检");
    }

    #[test]
    fn resets_at_drift_is_not_a_change() {
        // Codex 滚动窗口尾每次取数后漂(假时刻)——不得计入变化判据
        let mut t = Track::new(300, 0);
        let mut a = snap(0, 5.0);
        a.windows[0].resets_at = Some(1000);
        t.observe(&a, 300, 0);
        for i in 1..=3 {
            let mut b = snap(300 * i, 5.0);
            b.windows[0].resets_at = Some(1000 + 3600 * i);
            t.observe(&b, 300, 300 * i);
        }
        assert!(t.standby, "仅 resets_at 漂移 → 仍算安静");
    }

    #[test]
    fn sub_percent_jitter_is_not_a_change() {
        let mut t = Track::new(300, 0);
        t.observe(&snap(0, 5.004), 300, 0);
        t.observe(&snap(300, 5.008), 300, 300);
        t.observe(&snap(600, 5.002), 300, 600);
        t.observe(&snap(900, 5.006), 300, 900);
        assert!(t.standby, "<0.01% 抖动 → 仍算安静");
    }

    #[test]
    fn plan_change_exits() {
        let mut t = standby_track();
        let mut upgraded = snap(1500, 5.0);
        upgraded.plan_type = "max".into();
        assert!(t.observe(&upgraded, 300, 1500), "套餐变化 → 退出待机(翻转)");
        assert_eq!(t.tier, 300);
    }

    #[test]
    fn base_change_resets_tier() {
        let mut t = standby_track(); // 待机 10m
        // 基础档改 10m + 数据变化 → 档位回新基础档,退出待机
        assert!(t.observe(&snap(1500, 18.0), 600, 1500));
        assert_eq!(t.tier, 600);
        assert!(!t.standby);
    }

    #[test]
    fn slowest_base_can_still_enter_standby() {
        // 基础档 30m(已在最低频):旧版永不待机;新版 3 轮 + 时长达标照常减淡
        let mut t = Track::new(1800, 0);
        t.observe(&snap(0, 5.0), 1800, 0);
        t.observe(&snap(1800, 5.0), 1800, 1800);
        t.observe(&snap(3600, 5.0), 1800, 3600);
        assert!(t.observe(&snap(5400, 5.0), 1800, 5400));
        assert_eq!(t.tier, MAX_IDLE_SECS);
    }

    #[test]
    fn static_surface_due_wait_attention_and_disable() {
        // 静态表面(due/next_wait/注意/开关)——其余单测走纯 Track,不碰静态表
        assert!(due(Platform::Codex, 0), "无轨道 = 应检");
        assert_eq!(next_wait_secs(0, 300), 300, "无轨道 → 基础档");
        for i in 0..=3 {
            observe(Platform::Codex, &snap(300 * i, 5.0), 300, 300 * i);
        } // t=900 进入待机,10m 档,next_due = 1500
        assert!(!due(Platform::Codex, 1000), "未到期不检");
        assert!(due(Platform::Codex, 1500), "到期必检");
        assert_eq!(next_wait_secs(900, 300), 600);
        assert_eq!(next_wait_secs(1501, 300), 1, "逾期 → 至少醒 1s");
        assert!(get_subscription_idle()
            .unwrap()
            .iter()
            .any(|s| s.platform == Platform::Codex && s.idle));

        // 本地活动映射到 Codex:退出待机 + 立即应检
        let a = note_attention(1000, Some(Platform::Codex));
        assert_eq!(a, Attention { flipped: true, rescheduled: true });
        assert!(due(Platform::Codex, 1000));
        assert!(get_subscription_idle().unwrap().iter().all(|s| !s.idle));
        assert_eq!(
            note_attention(1001, Some(Platform::Codex)),
            Attention::default(),
            "已退出待机:不再立即补取,不翻转"
        );

        let _ = set_subscription_idle_enabled(false); // 关闭:清表 + 回基础档
        assert!(due(Platform::Codex, 0));
        assert_eq!(next_wait_secs(0, 300), 300);
        assert_eq!(note_attention(0, None), Attention::default(), "关闭时注意为空操作");
        assert!(get_subscription_idle().unwrap().iter().all(|s| !s.idle));
        let _ = set_subscription_idle_enabled(true); // 收尾恢复默认(开)
    }
}
