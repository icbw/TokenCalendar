//! 待机（悬浮球减淡）与兜底取数的应检时刻。
//!
//! 本模块装着**两件互不相干的事**,状态分开存：若混在同一张每平台表里,纯视觉开关会改
//! 网络行为（关掉减淡 ⇒ 清表 ⇒ `due` 的「无记录即应检」让每次醒来两个平台都取一轮）。
//!
//! - **待机（视觉）**：一个全局态。用户离开 agent 开发工作时降低常驻贴边条的视觉干扰;
//!   回到工作或注意到它立刻退出。
//!   判据 = **安静起点距今 ≥ `STANDBY_QUIET_SECS`**（与 demand 的「复工即取」共用常量）。
//!   安静起点 = 最近一次「用户注意 / 本地 agent 活动」,**全局一个数**：任何 agent 在用
//!   = 用户在工作（`note_local_tokens` 对所有采集源都调 `note_attention`,包括不对应订阅
//!   的源）。初值取进程启动时刻,免得刚打开就减淡。
//!   退出只翻转视觉态,**不排取数**——取数时机单一源在 demand.rs（复工那一笔 token 会
//!   照常排一轮;手动刷新走 `refresh_subscriptions_now` 的全量轮）。
//!   只看本地痕迹意味着**纯在线 / 网页用量期间悬浮球会减淡**（兜底取数照常跑,读数不会停）。
//!
//! - **兜底调度（网络）**：每平台一个应检时刻 = 上一轮取数时刻 + 设置的兜底间隔。
//!   待机**不改变取数频次**,两者唯一的交集是主线程睡眠时长要同时照顾「下一次应检」
//!   与「下一次待机判定」。
//!
//! 开关持久化在 designPrefs（orbIdleEnabled,默认开），只影响待机,不影响调度。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde::Serialize;

use super::model::Platform;

/// 进入待机所需的安静时长 = demand 的「离开」判据（单一源,勿再定义第二个）。
pub use super::demand::QUIET_SECS as STANDBY_QUIET_SECS;

// ---------- 待机（全局视觉态） ----------

/// 待机状态机（纯逻辑,单测直接覆盖;全局实例见 `standby`）。
#[derive(Debug)]
struct Standby {
    /// 开关（designPrefs.orbIdleEnabled 的运行时镜像）。
    enabled: bool,
    on: bool,
    /// 安静起点（最近一次用户注意 / 本地 agent 活动;初值 = 进程启动时刻）。
    attended_at: i64,
}

impl Standby {
    /// 用户注意 / 本地 agent 活动：推进安静起点并退出待机。
    /// 返回 true = 确有翻转（需广播 `subscription:idle`）。
    fn note_attention(&mut self, now: i64) -> bool {
        self.attended_at = now;
        std::mem::replace(&mut self.on, false)
    }

    /// 重算待机态。返回 true = 有翻转（需广播）。
    fn evaluate(&mut self, now: i64) -> bool {
        let want = self.enabled && now - self.attended_at >= STANDBY_QUIET_SECS;
        std::mem::replace(&mut self.on, want) != want
    }

    /// 下一次待机判定的时刻（已待机 / 开关关掉 → 不必再判）。
    fn next_check(&self, now: i64) -> Option<i64> {
        if !self.enabled || self.on {
            return None;
        }
        Some(self.attended_at + STANDBY_QUIET_SECS).filter(|at| *at > now)
    }

    /// 下发开关。返回 true = 值真的变了（调用方据此决定是否重算睡眠）。
    fn set_enabled(&mut self, enabled: bool) -> bool {
        if self.enabled == enabled {
            return false;
        }
        self.enabled = enabled;
        if !enabled {
            self.on = false;
        }
        true
    }
}

static STANDBY: LazyLock<Mutex<Standby>> = LazyLock::new(|| {
    Mutex::new(Standby {
        enabled: true,
        on: false,
        attended_at: chrono::Utc::now().timestamp(),
    })
});

/// 取待机状态机（poison 容忍）。
fn standby() -> std::sync::MutexGuard<'static, Standby> {
    STANDBY.lock().unwrap_or_else(|e| e.into_inner())
}

/// 用户注意 / 本地 agent 活动（语义见模块头）。返回 true = 有翻转（需广播）。
/// **不排取数**——取数时机单一源在 demand.rs。
pub fn note_attention(now: i64) -> bool {
    standby().note_attention(now)
}

/// 重算待机态。返回 true = 有翻转（需广播 `subscription:idle`）。
pub fn evaluate(now: i64) -> bool {
    standby().evaluate(now)
}

// ---------- 兜底取数调度（每平台一个应检时刻） ----------

/// 每平台的下次应检时刻（unix 秒）。表里只有这一个数——兜底间隔本身的单一源是
/// `super:POLL_SECS`,不必每平台再存一份镜像。
static DUES: LazyLock<Mutex<HashMap<Platform, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 取应检时刻表（poison 容忍）。
fn dues() -> std::sync::MutexGuard<'static, HashMap<Platform, i64>> {
    DUES.lock().unwrap_or_else(|e| e.into_inner())
}

/// 该平台是否应检（兜底节奏）。无记录 → 检：刚绑定还没取过第一轮的平台
/// 立刻取一次（绑定命令本身也会 wake,这里是双保险）。
pub fn due(platform: Platform, now: i64) -> bool {
    dues().get(&platform).map_or(true, |at| now >= *at)
}

/// 一轮取数之后：按兜底间隔顺延应检时刻（基础档变更即时生效）。
pub fn schedule_next(platform: Platform, base: u64, now: i64) {
    dues().insert(platform, now + base as i64);
}

/// 把该平台的应检时刻提前到 `due`（本地 token 驱动入口,见 demand.rs）。
/// 返回 true = 确有提前（调用方唤醒主轮询重算睡眠）。无记录时直接落 `due`
/// ——绑定后首个 token 也要能立刻取数。
pub fn pull_forward(platform: Platform, due: i64) -> bool {
    let mut g = dues();
    match g.get_mut(&platform) {
        Some(at) if due < *at => {
            *at = due;
            true
        }
        Some(_) => false,
        None => {
            g.insert(platform, due);
            true
        }
    }
}

/// 清掉未绑定平台的记录（解绑即停;待机开关与此无关）。
pub fn prune(bound: &[Platform]) {
    dues().retain(|p, _| bound.contains(p));
}

/// 主线程睡眠时长：最近的应检时刻与最近的待机判定时刻取小;都没有 → 兜底档。
pub fn next_wait_secs(now: i64, fallback: u64) -> u64 {
    let soonest_due = dues().values().min().copied();
    let at = match (soonest_due, standby().next_check(now)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    at.map_or(fallback, |at| (at - now).max(1) as u64)
}

pub fn emit_idle(app: &tauri::AppHandle) {
    use tauri::Emitter;
    let _ = app.emit("subscription:idle", true);
}

// ---------- 命令面 ----------

/// 单平台待机态（前端 orb 窗口消费）。待机是全局视觉态,两条记录的 `idle` 恒相同
/// ——形状按平台给是为了前端「全部已绑定平台都待机才减淡」的判据不必特判。
#[derive(Debug, Clone, Serialize)]
pub struct PlatformIdleState {
    pub platform: Platform,
    pub idle: bool,
}

/// 当前待机态（前端 orb 窗口初查口;事件 `subscription:idle` 翻转后重查）。
#[tauri::command]
pub fn get_subscription_idle() -> Result<Vec<PlatformIdleState>, String> {
    let idle = standby().on;
    Ok([Platform::Codex, Platform::Claude]
        .into_iter()
        .map(|platform| PlatformIdleState { platform, idle })
        .collect())
}

/// 下发待机开关（持久化由前端 designPrefs 承担,orb 窗口装载时再调本命令恢复）。
/// 关闭即清待机态。**不 wake**：待机是视觉态,不改变取数频次,wake 会推进代际 =
/// 两个平台各多发一次请求;只用 `reschedule` 让主轮询重算睡眠（下一次待机判定时刻变了）。
#[tauri::command]
pub fn set_subscription_idle_enabled(enabled: bool) -> Result<(), String> {
    // 幂等早退：设置页直调 + orb 桥接跟随会重复下发同一值。
    if !standby().set_enabled(enabled) {
        return Ok(());
    }
    super::reschedule();
    Ok(())
}

/// 用户注意到悬浮球（展开 / 切换平台;手动刷新走 `refresh_subscriptions_now`
/// 同一入口）：退出待机,不额外取数。
#[tauri::command]
pub fn note_subscription_attention(app: tauri::AppHandle) -> Result<(), String> {
    super::nudge_standby(&app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 待机状态机用局部实例测（全局那份归运行时;测试间不互相踩）。
    fn sb(now: i64) -> Standby {
        Standby { enabled: true, on: false, attended_at: now }
    }

    /// 应检时刻表是全局的,每个用例自己起个干净的记录。
    fn fresh(platform: Platform, base: u64, now: i64) {
        dues().insert(platform, now + base as i64);
    }

    #[test]
    fn standby_enters_after_silence_and_exits_on_attention() {
        let mut s = sb(1_000);
        assert!(!s.evaluate(1_000 + STANDBY_QUIET_SECS - 1), "静默未满不待机");
        assert!(s.evaluate(1_000 + STANDBY_QUIET_SECS), "静默满 10 分钟 → 进待机（翻转）");
        assert!(s.on);
        assert!(!s.evaluate(1_000 + STANDBY_QUIET_SECS * 2), "已待机:重算不再翻转");
        assert!(s.note_attention(1_000 + STANDBY_QUIET_SECS * 2), "用户注意 → 退出待机");
        assert!(!s.on);
        assert!(!s.note_attention(1_000 + STANDBY_QUIET_SECS * 2 + 1), "已经亮着:不算翻转");
    }

    #[test]
    fn disabled_switch_never_reports_standby() {
        let mut s = sb(1_000);
        s.enabled = false;
        assert!(!s.evaluate(1_000 + STANDBY_QUIET_SECS * 10), "开关关掉不进待机");
        assert_eq!(s.next_check(1_000), None);
    }

    #[test]
    fn disabling_clears_standby_and_re_enabling_judges_afresh() {
        let mut s = sb(1_000);
        s.evaluate(1_000 + STANDBY_QUIET_SECS);
        assert!(s.on);
        assert!(s.set_enabled(false));
        assert!(!s.on, "关掉即恢复全亮");
        assert!(!s.set_enabled(false), "同值下发不算变化");
        // 安静起点照常保留:重新打开时按「已经安静多久」立刻判定,不从零重计
        assert!(s.set_enabled(true));
        assert!(s.evaluate(1_000 + STANDBY_QUIET_SECS * 2), "重新打开 → 立刻回到待机");
    }

    #[test]
    fn standby_check_time_is_the_quiet_deadline() {
        let s = sb(1_000);
        assert_eq!(s.next_check(1_000), Some(1_000 + STANDBY_QUIET_SECS));
        assert_eq!(s.next_check(1_000 + STANDBY_QUIET_SECS), None, "判定时刻已到 / 已过 → 不再排");
    }

    #[test]
    fn fallback_due_follows_the_configured_interval() {
        let p = Platform::Claude;
        let now = 100_000;
        fresh(p, 1800, now);
        assert!(!due(p, now + 1799));
        assert!(due(p, now + 1800));
        schedule_next(p, 900, now + 1800);
        assert!(!due(p, now + 1800 + 899), "改档即时生效");
        assert!(due(p, now + 1800 + 900));
        prune(&[]);
    }

    #[test]
    fn pull_forward_only_moves_earlier() {
        let p = Platform::Claude;
        let now = 100_000;
        fresh(p, 1800, now);
        assert!(pull_forward(p, now + 60), "提前 → 生效");
        assert!(!pull_forward(p, now + 600), "更晚的排程不会推后已定的应检时刻");
        assert!(due(p, now + 60));
        prune(&[]);
    }

    #[test]
    fn sleep_wakes_for_the_standby_check_before_the_fallback() {
        let p = Platform::Codex;
        let now = chrono::Utc::now().timestamp();
        fresh(p, 1800, now);
        // 兜底还有 1800 秒,但待机判定最晚在安静满 10 分钟时就要做
        let wait = next_wait_secs(now, 1800);
        assert!(wait <= STANDBY_QUIET_SECS as u64, "睡眠被待机判定时刻截短:{wait}");
        prune(&[]);
    }

    /// 待机开关是**视觉**开关：关掉它不许动兜底调度表,否则每次醒来两个平台都被判成
    /// 应检,纯视觉开关改了网络行为。
    #[test]
    fn the_standby_switch_leaves_the_fallback_schedule_alone() {
        let p = Platform::Codex;
        let now = 200_000;
        fresh(p, 1800, now);
        let mut s = sb(now);
        s.set_enabled(false);
        prune(&[p]);
        assert!(!due(p, now + 1799), "开关关掉,应检时刻照旧");
        assert!(due(p, now + 1800));
        prune(&[]);
    }
}
