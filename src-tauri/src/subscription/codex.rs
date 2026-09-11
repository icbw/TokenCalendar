//! Codex（ChatGPT 订阅）额度适配器。
//!
//! 端点：GET chatgpt.com/backend-api/wham/usage,
//! Bearer = ~/.codex/auth.json tokens.access_token,可选 ChatGPT-Account-Id。
//! 刷新走 credentials:refresh_access（内存化,不回写文件）。

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use super::credentials::{self, MemoryToken, RawCredential, RefreshOutcome};
use super::model::{FetchStatus, Platform, QuotaWindow, SubscriptionSnapshot};

pub struct CodexAdapter {
    /// 内存 token 缓存（key = 平台;轮询线程单写单读,Mutex 仅为编译器安心）。
    tokens: Mutex<HashMap<Platform, MemoryToken>>,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self { tokens: Mutex::new(HashMap::new()) }
    }
}

/// 取当前可用 token（缓存有效则用缓存;否则现读文件;文件 token 过期且可刷新
/// 则静默刷新一轮）。返回 None = 无可用凭据（文件缺失/解析失败/已判死）。
pub enum Access {
    Ok(RawCredential),
    /// 凭据判死（refresh 永久失败）——本周期跳过网络。
    Dead,
    /// 无凭据（文件不存在/不可解析）。
    None,
    /// 瞬态错误（网络/服务端）——放弃,不降级状态。
    Transient,
}

impl CodexAdapter {
    /// 统一准入：从内存缓存或凭据文件取得可用 access token。
    /// 死态复活条件 = 凭据文件 mtime 变化（mod.rs 轮询里清缓存）。
    pub fn obtain_access(&self, platform: Platform) -> Access {
        let mut cache = self.tokens.lock().unwrap();
        if let Some(tok) = cache.get(&platform) {
            if tok.dead_since.is_some() {
                return Access::Dead;
            }
        }
        let Some(cred) = credentials::read_credential(platform) else {
            return Access::None;
        };
        if !cred.access_expired {
            return Access::Ok(cred);
        }
        // 文件内 token 已过期 → 静默刷新一轮（仅当有 refresh token）
        let Some(rt) = cred.refresh_token.clone() else {
            return Access::Ok(cred); // 无 refresh 可用,拿旧 token 碰一次 401 也算合理尝试
        };
        match credentials::refresh_access(platform, &rt) {
            RefreshOutcome::Renewed(new_access) => {
                cache.insert(
                    platform,
                    MemoryToken { access_token: new_access, dead_since: None },
                );
                Access::Ok(RawCredential {
                    access_token: cache.get(&platform).unwrap().access_token.clone(),
                    refresh_token: Some(rt),
                    access_expired: false,
                    account_hint: cred.account_hint,
                })
            }
            RefreshOutcome::Dead => {
                cache.insert(
                    platform,
                    MemoryToken {
                        access_token: String::new(),
                        dead_since: Some(chrono::Utc::now().timestamp()),
                    },
                );
                Access::Dead
            }
            RefreshOutcome::Transient | RefreshOutcome::NoRefresh => Access::Transient,
        }
    }

    /// 内存 token 失效（401 后被动刷新用）与判死记录。
    pub fn note_dead(&self, platform: Platform) {
        let mut cache = self.tokens.lock().unwrap();
        cache.insert(
            platform,
            MemoryToken {
                access_token: String::new(),
                dead_since: Some(chrono::Utc::now().timestamp()),
            },
        );
    }

    /// 凭据文件变化 → 清内存缓存（下轮重新现读）。
    pub fn invalidate(&self, platform: Platform) {
        self.tokens.lock().unwrap().remove(&platform);
    }
}

/// 拉取 Codex 订阅快照（一轮:取 token → GET usage → 解析 → 归一化）。
pub fn fetch(adapter: &CodexAdapter, cred: RawCredential) -> SubscriptionSnapshot {
    let now = chrono::Utc::now().timestamp();
    let agent = match super::http_agent() {
        Some(a) => a,
        None => {
            return snapshot_error(Platform::Codex, &cred, FetchStatus::NetworkFailed, now);
        }
    };

    let account_id = account_id_from_jwt(&cred.access_token);
    let mut req = agent
        .get("https://chatgpt.com/backend-api/wham/usage")
        .set("Authorization", &format!("Bearer {}", cred.access_token))
        .set("Accept", "application/json");
    if let Some(acc) = &account_id {
        req = req.set("ChatGPT-Account-Id", acc);
    }

    let resp = req.timeout(std::time::Duration::from_secs(15)).call();
    let body = match resp {
        Ok(r) => r.into_string().unwrap_or_default(),
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            // usage 401/403:被动刷新一轮分型（refresh 失败 → 判死）。刷新成功
            // 也不在重试（避免递归/突发请求）——记 auth_failed,下轮
            // obtain_access 自动用新内存 token 自然重试。
            return match passive_refresh(adapter, Platform::Codex, &cred) {
                PassiveResult::Dead => {
                    snapshot_error(Platform::Codex, &cred, FetchStatus::AuthFailed, now)
                }
                PassiveResult::Transient => {
                    snapshot_error(Platform::Codex, &cred, FetchStatus::NetworkFailed, now)
                }
            };
        }
        Err(ureq::Error::Status(code, _)) if code == 404 => {
            // 端点没了（改版）——按 parse_failed 记,UI 显示旧数据
            return snapshot_error(Platform::Codex, &cred, FetchStatus::ParseFailed, now);
        }
        Err(ureq::Error::Status(402, _)) => {
            // 支付/权益类（少见于 GET usage,防御性归 plan_inactive）
            return snapshot_error(Platform::Codex, &cred, FetchStatus::PlanInactive, now);
        }
        Err(ureq::Error::Status(_, _)) => {
            return snapshot_error(Platform::Codex, &cred, FetchStatus::NetworkFailed, now);
        }
        Err(_) => {
            return snapshot_error(Platform::Codex, &cred, FetchStatus::NetworkFailed, now);
        }
    };

    parse_usage(Platform::Codex, &cred, &body, now)
}

/// 从 access_token（JWT）取 ChatGPT-Account-Id claim（失败 None → 不带可选头）。
fn account_id_from_jwt(access_token: &str) -> Option<String> {
    let v = credentials::jwt_claim_json(access_token)?;
    // 常见 claim 形态:直接 "chatgpt_account_id",或嵌套 auth claim（容错探测,
    // 拿不到就不带——该头本就可选）
    if let Some(id) = v.get("chatgpt_account_id").and_then(|x| x.as_str()) {
        return Some(id.to_string());
    }
    v.get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(|x| x.as_str())
        .map(String::from)
}

/// 解析 wham/usage 响应（字段缺省容错——逆向接口,结构可能漂移）。
pub fn parse_usage(
    platform: Platform,
    cred: &RawCredential,
    body: &str,
    now: i64,
) -> SubscriptionSnapshot {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return snapshot_error(platform, cred, FetchStatus::ParseFailed, now),
    };

    // plan_type:缺省 "unknown"（不判 parse 失败——字段降级容忍)
    let plan_type = v
        .get("plan_type")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();

    // rate_limit.primary_window / secondary_window
    let mut windows = vec![];
    let rate = v.get("rate_limit");
    let primary = rate.and_then(|r| r.get("primary_window"));
    let secondary = rate.and_then(|r| r.get("secondary_window"));
    let mut structural_ok = primary.is_some() || secondary.is_some();

    for (node, kind) in [(primary, "5h"), (secondary, "7d")] {
        if let Some(w) = node {
            let used = w.get("used_percent").and_then(|x| x.as_f64());
            let reset = w.get("reset_at").and_then(|x| x.as_i64());
            match used {
                Some(u) => windows.push(QuotaWindow {
                    kind: kind.into(),
                    used_percent: u,
                    resets_at: reset,
                }),
                None => structural_ok = false,
            }
        }
    }

    // 多信号收敛判 plan_inactive（预案）:
    //  plan_type 报 free/unknown 且没有任何窗口 → 大概率无付费权益
    //  完全没有 rate_limit 结构 → 权益缺失
    let plan_inactive = matches!(plan_type.as_str(), "free" | "unknown") && windows.is_empty();
    if !structural_ok || plan_inactive {
        return snapshot_error(platform, cred, FetchStatus::PlanInactive, now);
    }

    SubscriptionSnapshot {
        platform,
        plan_type,
        windows,
        fetched_at: Some(now),
        status: FetchStatus::Ok,
    }
}

/// 被动刷新结果。
pub enum PassiveResult {
    Dead,
    Transient,
}

/// usage 401/403 后的被动刷新（一轮,失败即判死——有界,无循环）。
pub fn passive_refresh(
    adapter: &CodexAdapter,
    platform: Platform,
    cred: &RawCredential,
) -> PassiveResult {
    let Some(rt) = cred.refresh_token.clone() else {
        adapter.note_dead(platform);
        return PassiveResult::Dead;
    };
    match credentials::refresh_access(platform, &rt) {
        RefreshOutcome::Renewed(access) => {
            adapter.tokens.lock().unwrap().insert(
                platform,
                MemoryToken { access_token: access, dead_since: None },
            );
            // 刷新成功:按瞬态处理（不覆盖 auth_failed 语义）,下轮自愈
            PassiveResult::Transient
        }
        RefreshOutcome::Dead => {
            adapter.note_dead(platform);
            PassiveResult::Dead
        }
        RefreshOutcome::Transient | RefreshOutcome::NoRefresh => PassiveResult::Transient,
    }
}

/// 失败快照构造（保留 plan_type=unknown;**不覆盖已有 fetched_at**——None 表示
/// 未成功,前端沿用库内上一条成功快照的时间戳展示）。
fn snapshot_error(
    platform: Platform,
    _cred: &RawCredential,
    status: FetchStatus,
    now: i64,
) -> SubscriptionSnapshot {
    let _ = now;
    SubscriptionSnapshot {
        platform,
        plan_type: "unknown".into(),
        windows: vec![],
        fetched_at: None,
        status,
    }
}
