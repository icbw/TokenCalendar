//! Claude（Pro/Max 订阅）额度适配器。
//!
//! 端点：GET api.anthropic.com/api/oauth/usage,
//! Headers 四件套（Authorization/Accept/Content-Type/anthropic-beta:
//! oauth-——曾从可选转必填,必须带上）。
//! 凭据 = ~/.claude/.credentials.json claudeAiOauth.*。
//! 刷新走 credentials:refresh_access（内存化,不回写文件）。

use std::collections::HashMap;
use std::sync::Mutex;

use super::credentials::{self, MemoryToken, RawCredential};
use super::model::{FetchStatus, Platform, QuotaWindow, SubscriptionSnapshot};

/// 内存 token 缓存（与 codex 同构但独立实例——平台间不串缓存）。
pub struct ClaudeAdapter {
    tokens: Mutex<HashMap<Platform, MemoryToken>>,
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self { tokens: Mutex::new(HashMap::new()) }
    }

    /// 凭据文件变化 → 清内存缓存（下轮重新现读;判死自愈入口）。
    pub fn invalidate(&self, platform: Platform) {
        self.tokens.lock().unwrap().remove(&platform);
    }

    pub fn is_dead(&self, platform: Platform) -> bool {
        self.tokens
            .lock()
            .unwrap()
            .get(&platform)
            .map(|t| t.dead_since.is_some())
            .unwrap_or(false)
    }

    fn note_dead(&self, platform: Platform) {
        self.tokens.lock().unwrap().insert(
            platform,
            MemoryToken {
                access_token: String::new(),
                dead_since: Some(chrono::Utc::now().timestamp()),
            },
        );
    }
}

/// 拉取 Claude 订阅快照（一轮:usage 请求 + 401/403 分型 + 解析）。
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
        .timeout(std::time::Duration::from_secs(15))
        .call();

    match resp {
        Ok(r) => {
            let body = r.into_string().unwrap_or_default();
            parse_usage(&cred, &body, now)
        }
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            // 401/403 两可:token 失效或 订阅撤销（预案）——被动刷新一轮分型:
            // refresh Dead → auth_failed（凭据死了,须重登 CLI）;
            // Renewed → 下一轮用新 token 自然分型（新 token 仍 403 且未过期
            // → 权益没了的信号,但一轮内不重复请求,记 auth_failed）。
            let Some(rt) = cred.refresh_token.clone() else {
                adapter.note_dead(Platform::Claude);
                return error_snapshot(FetchStatus::AuthFailed);
            };
            match credentials::refresh_access(Platform::Claude, &rt) {
                credentials::RefreshOutcome::Renewed(access) => {
                    adapter.tokens.lock().unwrap().insert(
                        Platform::Claude,
                        MemoryToken { access_token: access, dead_since: None },
                    );
                    error_snapshot(FetchStatus::AuthFailed)
                }
                credentials::RefreshOutcome::Dead => {
                    adapter.note_dead(Platform::Claude);
                    error_snapshot(FetchStatus::AuthFailed)
                }
                _ => error_snapshot(FetchStatus::NetworkFailed),
            }
        }
        Err(ureq::Error::Status(404, _)) => error_snapshot(FetchStatus::ParseFailed),
        Err(ureq::Error::Status(_, _)) => error_snapshot(FetchStatus::NetworkFailed),
        Err(_) => error_snapshot(FetchStatus::NetworkFailed),
    }
}

/// 解析 /api/oauth/usage（窗口字段缺省容错;完全无窗口 → plan_inactive 信号）。
pub fn parse_usage(_cred: &super::credentials::RawCredential, body: &str, now: i64) -> SubscriptionSnapshot {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return error_snapshot(FetchStatus::ParseFailed);
    };

    let mut windows = vec![];
    for (key, kind) in [
        ("five_hour", "5h"),
        ("seven_day", "7d"),
        ("seven_day_opus", "7d_opus"),
        ("seven_day_omelette", "7d_opus"),
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

    SubscriptionSnapshot {
        platform: Platform::Claude,
        // usage 端点不返回 plan 名;subscriptionType 在凭据侧（前端并显）
        plan_type: "claude".into(),
        windows,
        fetched_at: Some(now),
        status: FetchStatus::Ok,
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
    }
}
