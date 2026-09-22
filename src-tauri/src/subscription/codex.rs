//! Codex（ChatGPT 订阅）额度适配器。
//!
//! 端点（逆向,未公开）：GET chatgpt.com/backend-api/wham/usage,
//! Bearer = ~/.codex/auth.json tokens.access_token,可选 ChatGPT-Account-Id。
//! 刷新走 credentials:refresh_access（内存化,不回写文件）。

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use super::credentials::{self, MemoryToken, RawCredential, RefreshOutcome};
use super::model::{FetchStatus, Platform, QuotaWindow, SnapshotSource, SubscriptionSnapshot};
use super::RateGate;

pub struct CodexAdapter {
    /// 内存 token 缓存（key = 平台;轮询线程与命令面等多个取数方共享）。
    tokens: Mutex<HashMap<Platform, MemoryToken>>,
    /// 429 冷却闸（`Retry-After` 期内不打网络）。
    gate: RateGate,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self { tokens: Mutex::new(HashMap::new()), gate: RateGate::default() }
    }

    /// 冷却剩余秒数（None = 未处于冷却）。
    pub fn cooldown_remaining(&self) -> Option<i64> {
        self.gate.remaining(chrono::Utc::now().timestamp())
    }

    /// 取内存 token 缓存（poison 容忍：持锁线程 panic 后缓存仍可用,
    /// 不让轮询线程与命令面连锁停摆）。
    fn tokens_slot(&self) -> std::sync::MutexGuard<'_, HashMap<Platform, MemoryToken>> {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 取凭据结果（见 `CodexAdapter:obtain_access`）。
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
    /// 统一准入：从内存缓存或凭据文件取得可用 access token;文件 token 过期且可刷新则静默刷新一轮。
    /// 死态复活条件 = 凭据文件 mtime 变化（mod.rs 轮询里清缓存）。
    /// 锁只覆盖内存缓存读写——**网络刷新在锁外做**：持锁刷新会把共享同一 Adapters 的
    /// 其他取数方阻塞最长 15s。
    pub fn obtain_access(&self, platform: Platform) -> Access {
        {
            let cache = self.tokens_slot();
            if let Some(tok) = cache.get(&platform) {
                if tok.dead_since.is_some() {
                    return Access::Dead;
                }
            }
        } // 锁在此释放:后续文件读取与网络刷新不得持锁
        let Some(cred) = credentials::read_credential(platform) else {
            return Access::None;
        };
        // 注:Codex 凭据文件无显式过期字段,read_credential 恒给 access_expired
        // = false——本分支当前不可达,为未来显式过期判定预留（届时并发刷新
        // 语义需随之下调,见下方写回保护）。
        if !cred.access_expired {
            return Access::Ok(cred);
        }
        // 文件内 token 已过期 → 静默刷新一轮（仅当有 refresh token）
        let Some(rt) = cred.refresh_token.clone() else {
            return Access::Ok(cred); // 无 refresh 可用,拿旧 token 碰一次 401 也算合理尝试
        };
        let outcome = credentials::refresh_access(platform, &rt);
        let mut cache = self.tokens_slot();
        match outcome {
            RefreshOutcome::Renewed(new_access) => {
                // 并发写回保护:别的线程可能已经刷成有效 token——保留先到者,
                // 不让后写者覆盖（两枚合法 token 互踩,后刷新者可能已失效）。
                if let Some(tok) = cache.get(&platform) {
                    if tok.dead_since.is_none() && !tok.access_token.is_empty() {
                        let access = tok.access_token.clone();
                        return Access::Ok(RawCredential {
                            access_token: access,
                            refresh_token: Some(rt),
                            access_expired: false,
                            account_hint: cred.account_hint,
                            plan_hint: None,
                            plan_tier: None,
                            // 刷新换的是同一个账号的 token,指纹照搬
                            account_fp: cred.account_fp.clone(),
                        });
                    }
                }
                cache.insert(
                    platform,
                    MemoryToken { access_token: new_access.clone(), dead_since: None },
                );
                Access::Ok(RawCredential {
                    access_token: new_access,
                    refresh_token: Some(rt),
                    access_expired: false,
                    account_hint: cred.account_hint,
                    // Codex 的 plan 在 usage 响应里（plan_type），凭据侧不提供
                    plan_hint: None,
                    plan_tier: None,
                    account_fp: cred.account_fp.clone(),
                })
            }
            RefreshOutcome::Dead => {
                // 同上:另一线程刚刷成功时不得把它判死。
                if let Some(tok) = cache.get(&platform) {
                    if tok.dead_since.is_none() && !tok.access_token.is_empty() {
                        let access = tok.access_token.clone();
                        return Access::Ok(RawCredential {
                            access_token: access,
                            refresh_token: Some(rt),
                            access_expired: false,
                            account_hint: cred.account_hint,
                            plan_hint: None,
                            plan_tier: None,
                            // 刷新换的是同一个账号的 token,指纹照搬
                            account_fp: cred.account_fp.clone(),
                        });
                    }
                }
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
        let mut cache = self.tokens_slot();
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
        self.tokens_slot().remove(&platform);
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
    super::note_http(&resp);
    let body = match resp {
        Ok(r) => {
            adapter.gate.clear();
            r.into_string().unwrap_or_default()
        }
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
        Err(ureq::Error::Status(429, r)) => {
            // 限流：按 Retry-After 冷却（期内零网络,静默保留旧数据）
            let secs = r
                .header("retry-after")
                .and_then(|v| v.trim().parse::<i64>().ok())
                .unwrap_or(300)
                .clamp(60, 1800);
            adapter.gate.arm(now, secs);
            return snapshot_error(Platform::Codex, &cred, FetchStatus::RateLimited, now);
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

/// **响应里看到的账号指纹**（`wham/usage` 顶层的 `account_id`）。
///
/// 这是最权威的一路——它就是**服务端把这次用量记在谁头上**,与读数在同一个响应里,
/// 零额外请求。但 `parse_usage` 是纯解析、拿不到 store,所以先放在这个格子里,
/// 由轮询线程落库时取走（与 `cost:set_plan` 同一套办法）。
static SEEN_ACCOUNT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 取走上一次解析看到的账号指纹（取完即清;没看到 → None）。
pub fn take_seen_account() -> Option<String> {
    SEEN_ACCOUNT.lock().unwrap_or_else(|e| e.into_inner()).take()
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

    // 账号指纹：响应顶层就带 `account_id`（与 `user_id` / `email` 并列）——**不存原值**,
    // 只把哈希放进格子,由轮询线程取走（见 SEEN_ACCOUNT）。
    if let Some(id) = v.get("account_id").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
        *SEEN_ACCOUNT.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(super::credentials::fingerprint(id));
    }

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

    // 多信号收敛判 plan_inactive:
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
        source: SnapshotSource::Api,
    }
}

/// 被动刷新结果。刷新成功也归 `Transient`：新 token 已进内存缓存,不重试。
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
            adapter.tokens_slot().insert(
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
        source: SnapshotSource::Api,
    }
}
