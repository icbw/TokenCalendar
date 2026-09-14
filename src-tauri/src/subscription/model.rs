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
}

/// 额度窗口种类（kind 语义跨平台对齐：5h 滚动 / 7d 滚动 / 附加窗口）。
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// 单平台订阅快照（归一化,落库/命令面同形状）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionSnapshot {
    pub platform: Platform,
    /// 套餐名（"plus"/"pro"/"max"/"free"…;获取失败为 "unknown"）。
    pub plan_type: String,
    pub windows: Vec<QuotaWindow>,
    /// 最近一次成功获取。
    pub fetched_at: Option<i64>,
    /// 状态。
    pub status: FetchStatus,
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
