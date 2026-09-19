//! Claude（Pro/Max 订阅）额度适配器。
//!
//! 端点：GET api.anthropic.com/api/oauth/usage,
//! Headers 四件套（Authorization/Accept/Content-Type/anthropic-beta:
//! oauth-——曾从可选转必填,必须带上）。
//!
//! **User-Agent 决定限流桶**：
//! 同一枚 token,带 `claude-code/<version>` 能穿到鉴权层（正规 401/200）,
//! 不带（ureq/curl 默认 UA）持续 429 rate_limit_error——**必须显式带上**,
//! 否则无论怎么重试都是「Network error」（真实症状：读数全 —）。
//!
//! 凭据 = ~/.claude/.credentials.json claudeAiOauth.*,**只读、不刷新**：
//! Claude 的 refresh token **一次性**（用过即轮换）,刷新了不写回会把 Claude
//! Code 自己的登录态用废（它下次刷新 invalid_grant 被迫重登）;而写回又违反
//! 「凭据文件只读」红线。故 token 过期即记 auth_failed,待用户跑一次 CLI
//! 重写文件（mtime 自愈）——自助刷新/自持登录态另立项。

use std::collections::HashMap;
use std::sync::Mutex;

use super::credentials::{self, MemoryToken, RawCredential};
use super::model::{FetchStatus, Platform, QuotaWindow, SnapshotSource, SubscriptionSnapshot};
use super::RateGate;

/// 上报的 CLI 版本：UA 只要求形态正确 + 版本够新。
/// 取值参照本机装着的扩展 `anthropic.claude-code-2.1.267` / npm latest 2.1.269。
const CLAUDE_CODE_VERSION: &str = "2.1.269";

/// usage 端点 UA（命中宽松限流桶）。
fn ua_usage() -> String {
    format!("claude-code/{CLAUDE_CODE_VERSION}")
}

/// OAuth 刷新端点 UA（走的是另一套桶：`claude-cli/... （external, cli)`）。
pub(super) fn ua_cli() -> String {
    format!("claude-cli/{CLAUDE_CODE_VERSION} (external, cli)")
}

/// 取凭据结果（无刷新环节——见模块头）。
pub enum Access {
    Ok(RawCredential),
    /// 凭据判死（文件内 token 已过期,或 usage 返回 401/403）——本周期零网络,
    /// 待凭据文件 mtime 变化（用户跑 CLI 重写）复活。
    Dead,
    /// 无凭据（文件不存在/不可解析）。
    None,
}

/// 内存 token 缓存（与 codex 同构但独立实例——平台间不串缓存）。
pub struct ClaudeAdapter {
    tokens: Mutex<HashMap<Platform, MemoryToken>>,
    /// 429 冷却闸（`Retry-After` 期内不打网络）。
    gate: RateGate,
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self { tokens: Mutex::new(HashMap::new()), gate: RateGate::default() }
    }

    /// 冷却剩余秒数（None = 未处于冷却）。
    pub fn cooldown_remaining(&self) -> Option<i64> {
        self.gate.remaining(chrono::Utc::now().timestamp())
    }

    /// 取内存 token 缓存（poison 容忍,审计 P3-：持锁线程 panic 后缓存仍可用,
    /// 不让轮询线程与命令面连锁停摆）。
    fn tokens_slot(&self) -> std::sync::MutexGuard<'_, HashMap<Platform, MemoryToken>> {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 取当前可用凭据（只读文件;token 过期不刷新,直接判死——见模块头）。
    pub fn obtain_access(&self, platform: Platform) -> Access {
        if self.is_dead(platform) {
            return Access::Dead;
        }
        match credentials::read_credential(platform) {
            None => Access::None,
            Some(cred) if cred.access_expired => {
                self.note_dead(platform);
                Access::Dead
            }
            Some(cred) => Access::Ok(cred),
        }
    }

    /// 凭据文件变化 → 清内存缓存（下轮重新现读;判死自愈入口）。
    /// 冷却也一并清掉：文件都换了（用户重登/CLI 续期）,旧限流判定不再适用。
    /// **只认自己平台的变化**：gate 是 Claude 自己的 429 退避,别的平台换凭据
    /// 不该把它清掉。
    pub fn invalidate(&self, platform: Platform) {
        if platform != Platform::Claude {
            return;
        }
        self.tokens_slot().remove(&platform);
        self.gate.clear();
    }

    pub fn is_dead(&self, platform: Platform) -> bool {
        self.tokens_slot()
            .get(&platform)
            .map(|t| t.dead_since.is_some())
            .unwrap_or(false)
    }

    fn note_dead(&self, platform: Platform) {
        self.tokens_slot().insert(
            platform,
            MemoryToken {
                access_token: String::new(),
                dead_since: Some(chrono::Utc::now().timestamp()),
            },
        );
    }
}

/// 429 响应的冷却秒数：优先 `Retry-After`（钳到 [60, 1800]）,缺省 300。
fn retry_after_secs(r: &ureq::Response) -> i64 {
    r.header("retry-after")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(300)
        .clamp(60, 1800)
}

/// 拉取 Claude 订阅快照（一轮:usage 请求 + 状态分型 + 解析）。
pub fn fetch(adapter: &ClaudeAdapter, cred: RawCredential) -> SubscriptionSnapshot {
    let now = chrono::Utc::now().timestamp();
    let Some(agent) = super::http_agent() else {
        return error_snapshot(FetchStatus::NetworkFailed);
    };

    let resp = agent
        .get("https://api.anthropic.com/api/oauth/usage")
        .set("Authorization", &format!("Bearer {}", cred.access_token))
        .set("Accept", "application/json")
        .set("Content-Type", "application/json")
        .set("anthropic-beta", "oauth-2025-04-20")
        // 限流桶分档的开关（缺它必 429,见模块头）
        .set("User-Agent", &ua_usage())
        .timeout(std::time::Duration::from_secs(15))
        .call();
    super::note_http(&resp);

    match resp {
        Ok(r) => {
            adapter.gate.clear();
            let body = r.into_string().unwrap_or_default();
            parse_usage(&cred, &body, now)
        }
        Err(ureq::Error::Status(429, r)) => {
            // 限流：按 Retry-After 冷却（期内零网络,静默保留旧数据）
            adapter.gate.arm(now, retry_after_secs(&r));
            error_snapshot(FetchStatus::RateLimited)
        }
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            // 401/403 两可:token 失效或 订阅撤销（预案）——**不刷新**
            // （一次性 refresh token,见模块头）:判死,待 CLI 重写凭据文件复活。
            adapter.note_dead(Platform::Claude);
            error_snapshot(FetchStatus::AuthFailed)
        }
        Err(ureq::Error::Status(404, _)) => error_snapshot(FetchStatus::ParseFailed),
        Err(ureq::Error::Status(_, _)) => error_snapshot(FetchStatus::NetworkFailed),
        Err(_) => error_snapshot(FetchStatus::NetworkFailed),
    }
}

/// 解析 /api/oauth/usage（窗口字段缺省容错;完全无窗口 → plan_inactive 信号）。
pub fn parse_usage(cred: &super::credentials::RawCredential, body: &str, now: i64) -> SubscriptionSnapshot {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return error_snapshot(FetchStatus::ParseFailed);
    };

    let mut windows = vec![];
    // 只取语义明确的三个窗口：omelette / cowork / 各种代号窗口（nimbus_quill 等）
    // 是活动型/实验型限额,归到 "7d_opus" 属错标,一律不取。
    // 注：新响应另带 `limits[]`（session / weekly_all / weekly_scoped）结构化视图,
    // 目前与上述三个窗口同源,故仍按旧键解析（端点改版时再迁）。
    for (key, kind) in [
        ("five_hour", "5h"),
        ("seven_day", "7d"),
        ("seven_day_opus", "7d_opus"),
        ("seven_day_sonnet", "7d_sonnet"),
    ] {
        let Some(node) = v.get(key) else { continue };
        let Some(util) = node.get("utilization").and_then(|x| x.as_f64()) else {
            continue;
        };
        let resets_at = node
            .get("resets_at")
            .and_then(|x| x.as_str())
            .and_then(rfc3339_to_unix);
        windows.push(QuotaWindow { kind: kind.into(), used_percent: util, resets_at });
    }

    if windows.is_empty() {
        // token 有效但零窗口:企业 spend 形状（首期不支持）或权益缺失
        return error_snapshot(FetchStatus::PlanInactive);
    }

    // 限额档（`rateLimitTier`）只喂出厂预设的倍率表——`subscriptionType` 分不出
    // Max 5x / 20x,而两档配额差 4 倍。它**不进库**,见 `RawCredential:plan_tier`。
    if let Some(tier) = cred.plan_tier.as_deref() {
        super::cost::set_plan_tier(Platform::Claude, tier);
    }

    SubscriptionSnapshot {
        platform: Platform::Claude,
        // usage 端点**不返回** plan 名 → 取凭据侧 subscriptionType（"max"/"pro"）,
        // 前端按「平台名 + 套餐名」拼成「Claude Max」;取不到才回落 unknown
        // （前端见到 unknown 就只显示平台名）。
        plan_type: cred.plan_hint.clone().unwrap_or_else(|| "unknown".into()),
        windows,
        fetched_at: Some(now),
        status: FetchStatus::Ok,
        source: SnapshotSource::Api,
    }
}

fn rfc3339_to_unix(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

fn error_snapshot(status: FetchStatus) -> SubscriptionSnapshot {
    SubscriptionSnapshot {
        platform: Platform::Claude,
        plan_type: "unknown".into(),
        windows: vec![],
        fetched_at: None,
        status,
        source: SnapshotSource::Api,
    }
}
