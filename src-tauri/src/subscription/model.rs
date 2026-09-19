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

/// 读数来源：
/// - `api` = 平台 usage 端点读数,`fetched_at` = 请求时刻,与本地代价的累计区间对得上
///   （差不到一秒）;
/// - `desktop` = Claude 桌面端 `plan-usage-history.json` 采样,`fetched_at` = 样本时刻
///与「这段时间花了多少」对应的不是同一段时间;
/// - `rollout` = Codex 会话 rollout 里 `token_count` 事件带的 `rate_limits`,
///   `fetched_at` = 那次 API 调用的时刻。**它和 `api` 是同一个服务端数字**——
///   Codex 在每次响应里回的限流状态,只是走本地文件到手,不花一个请求。
///
/// 三者的百分比精度其实相同,分开标不是为了
/// 精度,是为了**时刻语义**：`api` 的时刻就是「现在」,另外两个是「那一刻」。
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
    /// 状态。
    pub status: FetchStatus,
    /// 读数来源（语义见 [`SnapshotSource`];失败轮沿用库内上一条的来源）。
    #[serde(default)]
    pub source: SnapshotSource,
}

/// 归一化读数序列的一行（-4;落库形状见 `store` 的 `quota_reading` 建表注释）。
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

/// 日级汇总的一行（-4;**纯派生**,可从 `quota_reading` 完全重建）。
///
/// 日界按**本地日期**切,与热力图 / collector 的 `YYYY-MM-DD` 同口径。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaDay {
    /// 本地日期 `YYYY-MM-DD`。
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
    /// 凭据文件是否存在。
    pub present: bool,
    /// 结构是否可解析（present 且 parseable 才可绑定）。
    pub parseable: bool,
    /// 账号掩码（如 JWT payload 取 id / 邮箱前 3 位 + ***;无则 None）。
    pub account_hint: Option<String>,
}
