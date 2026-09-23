//! 订阅额度归一化数据模型。
//!
//! 两平台原始响应字段不同（Codex: used_percent + unix 秒;Claude: utilization +
//! ISO 8601），适配器各自转换为这里的统一模型后落库/出命令面——前端只消费
//! 归一化形状。

use serde::{Deserialize, Serialize};

/// 平台标识（订阅侧平台集合与 collector 六源无关，独立枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Codex,
    Claude,
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Codex => "codex",
            Platform::Claude => "claude",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "codex" => Some(Platform::Codex),
            "claude" => Some(Platform::Claude),
            _ => None,
        }
    }
    /// collector 采集源 id → 其用量计入的订阅平台（待机退出的本地活动信号用;
    /// 其余源不对应订阅,返回 None）。
    pub fn of_collector_source(source_id: &str) -> Option<Self> {
        match source_id {
            "codex" => Some(Platform::Codex),
            "claude-code" => Some(Platform::Claude),
            _ => None,
        }
    }
    /// 上一条的逆：该平台的用量记在 collector 的哪个 `agent_key` 名下。
    /// 分模型用量查询按它去 collector.db 取数。
    pub fn collector_source(&self) -> &'static str {
        match self {
            Platform::Codex => "codex",
            Platform::Claude => "claude-code",
        }
    }
}

/// 额度窗口种类（kind 语义跨平台对齐：5h 滚动 / 7d 滚动 / 附加窗口）。
/// `PartialEq`:主轮询据此判「落库后读数真的变了吗」,只有真变了才广播
/// `subscription:changed`（两端都来自同一份 JSON 往返,浮点按位可比）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// "5h" | "7d" | "7d_opus"（附加窗口原样透传,前端按需展示）。
    pub kind: String,
    /// 已用百分比 0-100（「剩余 = 100 − used」换算在前端做,口径单侧）。
    pub used_percent: f64,
    /// 重置时间（unix 秒;平台未提供时为 None）。
    pub resets_at: Option<i64>,
}

/// 快照状态分级：
/// - ok: 正常;
/// - plan_inactive: 订阅过期/降级（凭据仍有效,权益没了——多信号收敛判定,
///   见各适配器;续费后下一轮自动恢复 ok,用户零操作）;
/// - auth_failed: 凭据失效（refresh token 被撤销/过期,需重新登录 agent CLI;
///   Codex 订阅过期主路径落在这一态——token 随订阅失效）;
/// - rate_limited: 平台限流（429）——**按 `Retry-After` 冷却期内零网络**,
///   静默保留旧数据;Claude usage 端点的限流桶按 User-Agent 分档,详见 claude.rs;
/// - network_failed: 网络失败（静默保留旧数据）;
/// - parse_failed: 响应结构不认识（端点改版信号,字段容错后仍不完整）;
/// - idle: 未绑定/凭据文件不存在。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchStatus {
    Ok,
    PlanInactive,
    AuthFailed,
    RateLimited,
    NetworkFailed,
    ParseFailed,
    Idle,
}

impl FetchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FetchStatus::Ok => "ok",
            FetchStatus::PlanInactive => "plan_inactive",
            FetchStatus::AuthFailed => "auth_failed",
            FetchStatus::RateLimited => "rate_limited",
            FetchStatus::NetworkFailed => "network_failed",
            FetchStatus::ParseFailed => "parse_failed",
            FetchStatus::Idle => "idle",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "ok" => FetchStatus::Ok,
            "plan_inactive" => FetchStatus::PlanInactive,
            "auth_failed" => FetchStatus::AuthFailed,
            "rate_limited" => FetchStatus::RateLimited,
            "network_failed" => FetchStatus::NetworkFailed,
            "parse_failed" => FetchStatus::ParseFailed,
            "idle" => FetchStatus::Idle,
            _ => return None,
        })
    }
}

/// 读数来源（标定筛选与诊断用）：
/// - `api` = 平台 usage 端点读数,`fetched_at` = 请求时刻,与本地代价的累计区间对得上;
/// - `desktop` = Claude 桌面端 `plan-usage-history.json` 采样,`fetched_at` = 样本时刻
///与「这段时间花了多少」对应的不是同一段时间;
/// - `rollout` = Codex 会话 rollout 里 `token_count` 事件带的 `rate_limits`,
///   `fetched_at` = 那次 API 调用的时刻。**它和 `api` 是同一个服务端数字**,
///   只是走本地文件到手,不花一个请求。
///
/// 三者的百分比精度相同（两个平台给的都是整数）,分开标是为了**时刻语义**：
/// `api` 的时刻就是「现在」,另外两个是「那一刻」。
///
/// **在线标定样本只认两端都是 `api` 的读数对**（`record_pair`）:另外两路各自按**读数
/// 自己的时刻**精确切割（[`super:bootstrap`] / [`super:codex_rollout`]）——两个窗口
/// 同源、偏移归零,而且密度比取数轮高得多。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotSource {
    #[default]
    Api,
    Desktop,
    Rollout,
}

impl SnapshotSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SnapshotSource::Api => "api",
            SnapshotSource::Desktop => "desktop",
            SnapshotSource::Rollout => "rollout",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "api" => Some(SnapshotSource::Api),
            "desktop" => Some(SnapshotSource::Desktop),
            "rollout" => Some(SnapshotSource::Rollout),
            _ => None,
        }
    }
}

/// 单平台订阅快照（归一化,落库/命令面同形状;`PartialEq` 见 `QuotaWindow`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionSnapshot {
    pub platform: Platform,
    /// 套餐名（"plus"/"pro"/"max"/"free"…;获取失败为 "unknown"）。
    pub plan_type: String,
    pub windows: Vec<QuotaWindow>,
    /// 最近一次成功获取。
    pub fetched_at: Option<i64>,
    pub status: FetchStatus,
    /// 读数来源（语义见 [`SnapshotSource`];失败轮沿用库内上一条的来源）。
    #[serde(default)]
    pub source: SnapshotSource,
}

/// Codex 滚动窗尾的容差（秒）：未开始的窗口，窗尾 = 读数时刻 + 窗口长度。真库 api 读数偏
/// −1〜+9 秒；rollout 读数偏 −23〜−6 秒（读数取调用开始时刻）。取 120，留足余量。
const ROLLING_RESET_TOLERANCE_SECS: i64 = 120;

/// 窗口是否**已开始计时**（与「已用是否为 0」是两件事）。
///
/// 读数是整数百分比，窗口开始后用量不到 1% 时 `used_percent` 仍是 0，所以不能拿 0 当作
/// 「没开始」（[调查]（../../../)）。
/// 各平台在服务端各有一个信号：
/// - Claude：没开始时 `resets_at = null`，开始后才给窗尾；
/// - Codex：没开始时给**滚动窗尾**（读数时刻 + 窗口长度，每次取数都往后漂），开始后窗尾固定。
///
/// 两个平台都要求窗尾仍在未来：桌面端零请求路会保留上一份快照的窗尾，那个窗口可能已经过期。
/// 桌面端读数本身不带窗尾，在这里自然落到 `used > 0` 这条判据上。
pub fn window_started(platform: Platform, w: &QuotaWindow, fetched_at: Option<i64>, now: i64) -> bool {
    if w.used_percent > 0.0 {
        return true;
    }
    let Some(reset) = w.resets_at.filter(|&r| r > now) else {
        return false;
    };
    match platform {
        Platform::Claude => true,
        Platform::Codex => {
            let len = match w.kind.as_str() {
                "5h" => 5 * 3600,
                "7d" => 7 * 86_400,
                // 未知窗口没有长度可比；窗尾在未来就当作已开始（与 Claude 同判）
                _ => return true,
            };
            fetched_at.is_some_and(|t| reset - t < len - ROLLING_RESET_TOLERANCE_SECS)
        }
    }
}

/// 前端读口的快照形状：快照原样展开，另附**派生的**「已开始的窗口」列表。
/// 只在命令面现算，不落库——`resets_at` 与读数时刻都在库里，随时能重新算出来。
#[derive(Debug, Clone, Serialize)]
pub struct FrontendSnapshot {
    #[serde(flatten)]
    pub snap: SubscriptionSnapshot,
    /// 已开始计时的窗口 kind（见 [`window_started`]）。
    pub started_windows: Vec<String>,
}

impl FrontendSnapshot {
    pub fn from_snapshot(snap: SubscriptionSnapshot, now: i64) -> Self {
        let started_windows = snap
            .windows
            .iter()
            .filter(|w| window_started(snap.platform, w, snap.fetched_at, now))
            .map(|w| w.kind.clone())
            .collect();
        Self { snap, started_windows }
    }
}

/// 归一化读数序列的一行（落库形状见 `store` 的 `quota_reading` 建表注释）。
///
/// 一行 = **一个窗口在某一时刻的一次读数**。与 `SubscriptionSnapshot` 的区别是维度：
/// 快照是「此刻两个窗口各是多少」,这里是「某个窗口一路走来是多少」——前者每平台
/// 一行覆盖式写入,后者只增不删。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaReading {
    /// 读数时刻（unix 秒;语义随 `source` 不同,见 [`SnapshotSource`]）。
    pub t: i64,
    /// 窗口种类,原样透传适配器给的 kind："5h" / "7d" / "7d_opus"。
    pub kind: String,
    pub used_percent: f64,
    /// 窗尾（unix 秒;`None` = **该来源不提供**,不是「没有窗尾」）。
    pub resets_at: Option<i64>,
    /// 该读数当时的套餐（源给不出 → 空串）。
    pub plan_type: String,
    pub source: SnapshotSource,
}

/// 日级汇总的一行（**纯派生**,可从 `quota_reading` 完全重建）。
///
/// 日界按**本地日期**切,与热力图 / collector 的 `YYYY-MM-DD` 同口径。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaDay {
    pub day: String,
    pub kind: String,
    /// 当日读数条数（**按时刻去重之后**）。
    pub n: i64,
    pub t_first: i64,
    pub t_last: i64,
    pub used_first: f64,
    pub used_last: f64,
    pub used_max: f64,
    pub used_min: f64,
    /// 当日观测到的**额度消耗**：相邻读数正向差之和（跨零点那一笔算进后一天）。
    /// 滚动窗口里消耗与过期同时发生 ⇒ 这是**下界**,不是账单。
    pub gain_pct: f64,
    /// 当日观测到的**窗口回收**：相邻读数负向差之和（绝对值）。
    pub drop_pct: f64,
    /// 当日跨过窗口重置的次数。窗尾要前移得**比时间本身还快**才算——空窗时服务端
    /// 报的是 `now + 窗长`,它跟着时间漂,不是重置。两端有一端不提供窗尾（回填的存量
    /// 行 / Claude 桌面端）就判不出来,恒为 0。
    pub resets: i64,
    /// 当天第一条读数与它前面那条之间的间隔（秒;前面没有读数则 0）。
    ///
    /// 空档之后的第一笔涨幅按定义整笔记在后一天,这个数就是让消费方看得
    /// 「这笔涨幅是跨多久攒出来的」。多长算太久由消费方定。
    pub carry_secs: i64,
}

/// 凭据发现项（设置页扫描列表用;**永不包含 token 值,只含掩码**）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialInfo {
    pub platform: Platform,
    pub present: bool,
    /// 结构是否可解析（present 且 parseable 才可绑定）。
    pub parseable: bool,
    /// 账号掩码（如 JWT payload 取 id / 邮箱前 3 位 + ***;无则 None）。
    pub account_hint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(kind: &str, used: f64, resets_at: Option<i64>) -> QuotaWindow {
        QuotaWindow { kind: kind.into(), used_percent: used, resets_at }
    }

    /// 2026-09-23 真库：21:02:54 那次 Claude API 读数，5h 已用 0、窗尾 01:49:59。
    const T: i64 = 1_790_190_174;

    #[test]
    fn claude_zero_with_reset_is_started() {
        assert!(window_started(Platform::Claude, &w("5h", 0.0, Some(1_790_207_399)), Some(T), T));
        assert!(window_started(Platform::Claude, &w("7d", 0.0, Some(1_790_780_399)), Some(T), T));
        assert!(window_started(Platform::Claude, &w("7d_fable", 0.0, Some(1_790_780_400)), Some(T), T));
    }

    #[test]
    fn claude_zero_without_reset_is_not_started() {
        assert!(!window_started(Platform::Claude, &w("5h", 0.0, None), Some(T), T));
    }

    #[test]
    fn expired_reset_is_not_started() {
        // 桌面端零请求路保留下来的旧窗尾，已经过期
        assert!(!window_started(Platform::Claude, &w("5h", 0.0, Some(T - 1)), Some(T - 9_000), T));
        assert!(!window_started(Platform::Codex, &w("5h", 0.0, Some(T - 1)), Some(T - 9_000), T));
    }

    #[test]
    fn used_above_zero_is_always_started() {
        // 桌面端读数不带窗尾；Claude scoped 窗口也见过有用量、窗尾为 null
        assert!(window_started(Platform::Claude, &w("5h", 3.0, None), Some(T), T));
        assert!(window_started(Platform::Codex, &w("5h", 1.0, Some(T + 18_000)), Some(T), T));
    }

    #[test]
    fn codex_rolling_reset_is_not_started() {
        // 真库极值：api 5h −1〜+9 秒、rollout 5h 最低 17,977、rollout 7d 604,785
        for d in [17_977, 17_999, 18_009] {
            assert!(!window_started(Platform::Codex, &w("5h", 0.0, Some(T + d)), Some(T), T), "{d}");
        }
        assert!(!window_started(Platform::Codex, &w("7d", 0.0, Some(T + 604_785)), Some(T), T));
    }

    #[test]
    fn codex_fixed_reset_is_started() {
        // 窗口开始了 10 分钟：窗尾固定，距读数时刻不足一个窗口长
        assert!(window_started(Platform::Codex, &w("5h", 0.0, Some(T + 18_000 - 600)), Some(T), T));
        // 快照不是刚取的也照样判：比的是读数时刻，不是现在
        assert!(window_started(Platform::Codex, &w("5h", 0.0, Some(T + 17_400)), Some(T), T + 300));
    }

    #[test]
    fn codex_without_fetched_at_is_not_started() {
        assert!(!window_started(Platform::Codex, &w("5h", 0.0, Some(T + 9_000)), None, T));
    }

    #[test]
    fn frontend_snapshot_lists_started_kinds_and_flattens() {
        let snap = SubscriptionSnapshot {
            platform: Platform::Claude,
            plan_type: "max".into(),
            windows: vec![w("5h", 0.0, None), w("7d", 0.0, Some(T + 86_400))],
            fetched_at: Some(T),
            status: FetchStatus::Ok,
            source: SnapshotSource::Api,
        };
        let f = FrontendSnapshot::from_snapshot(snap, T);
        assert_eq!(f.started_windows, vec!["7d".to_string()]);
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["platform"], "claude");
        assert_eq!(v["windows"][1]["kind"], "7d");
        assert_eq!(v["started_windows"][0], "7d");
    }
}
