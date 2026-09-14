//! 凭据文件读取与 token 静默刷新（核心安全层）。
//!
//! 红线：
//! - 凭据文件**只读不写**——刷新得到的新 token 只存内存,不回写 agent 自己的
//!   凭据文件（规避与 agent CLI 的 refresh rotation 互踩）;
//! - 优先依赖「agent CLI 在用 → 文件内 token 自鲜」,仅当 accessToken 判定过期
//!   时才用文件内 refreshToken 静默换新;
//! - token/refresh_token 不得进入任何日志/错误信息/落库路径。
//!
//! 刷新失败分级：
//! - 400/401/403 + invalid_grant 类 = **永久失效**（订阅过期撤销 refresh token,
//!   Codex 形态 invalid_refresh_token）→ Dead,判死后本周期零网络;
//! - 超时/5xx/DNS = 瞬态 → 不降级凭据状态,下轮照常。

use serde_json::Value;

use super::model::Platform;

/// 过期判定的提前量（秒）：见 read_credential 的 Claude 分支。
const EXPIRY_GRACE_SECS: i64 = 60;

/// 内存态 token（轮询线程持有,进程退出即消失;永不落盘）。
/// **不 derive Debug/Clone 打印面**——含明文 access token,防意外日志泄露;
/// Clone 仅供线程内缓存更新使用。
#[derive(Clone)]
pub struct MemoryToken {
    pub access_token: String,
    /// 判死时间（unix 秒）——Dead 态下跳过该平台一切网络请求,直到凭据文件
    /// mtime 变化（用户重新登录 CLI 重写文件）才复活重读。
    pub dead_since: Option<i64>,
}

impl std::fmt::Debug for MemoryToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryToken")
            .field("access_token", &"<redacted>")
            .field("dead_since", &self.dead_since)
            .finish()
    }
}

/// 凭据文件路径（`~` 经 collector:home_dir 同款解析）。
pub fn credential_path(platform: Platform) -> Option<std::path::PathBuf> {
    let home = crate::collector::home_dir()?;
    match platform {
        Platform::Codex => Some(home.join(".codex").join("auth.json")),
        Platform::Claude => Some(home.join(".claude").join(".credentials.json")),
    }
}

/// 凭据文件 mtime（unix 秒;文件不存在返回 None）——轮询线程的「文件自愈」探针。
pub fn credential_mtime(platform: Platform) -> Option<i64> {
    let path = credential_path(platform)?;
    let meta = std::fs::metadata(path).ok()?;
    meta.modified().ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

/// 展开的凭据（内存中转,Drop 即丢;**不 derive Debug——防 dbg!/{:?} 泄 token**）。
pub struct RawCredential {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// access token 是否已过期（据文件内 expires/时间戳;拿不到视为未过期,
    /// 交给 401 被动刷新兜底——宁可少刷不可多刷）。
    pub access_expired: bool,
    /// 账号掩码（设置页发现列表用;来自 id_token/JWT payload,不含原始 token）。
    pub account_hint: Option<String>,
    /// 套餐名（小写,如 "max"/"pro"/"plus"）——仅供**凭据侧**能拿到的情况填:
    /// Claude 的 usage 端点不返回 plan,只能从凭据 `subscriptionType` 取;
    /// Codex 的 plan 在 usage 响应里（plan_type）,此处留 None。
    pub plan_hint: Option<String>,
}

/// JWT payload 提取（不验签——本工具只读自显;失败返回 None,调用方降级）。
pub fn jwt_claim_json(token: &str) -> Option<Value> {
    let payload_b64 = token.split('.').nth(1)?;
    let payload = base64_lite_decode::b64_decode_json(payload_b64).ok()?;
    Some(payload)
}

/// 便捷单 claim 读取。
pub fn jwt_claim(token: &str, claim: &str) -> Option<Value> {
    jwt_claim_json(token)?.get(claim).cloned()
}

/// 极简 base64url → bytes → UTF-8 → serde_json（无新依赖,手写解码 ~30 行）。
mod base64_lite_decode {
    use serde_json::Value;

    pub fn b64_decode_json(input: &str) -> Result<Value, String> {
        let mut buf = Vec::with_capacity(input.len() * 3 / 4);
        let bytes: Vec<u8> = input
            .trim_end_matches('=')
            .bytes()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
        let mut acc: u32 = 0;
        let mut bits = 0u32;
        for b in bytes {
            let v = match b {
                b'A'..=b'Z' => (b - b'A') as u32,
                b'a'..=b'z' => (b - b'a' + 26) as u32,
                b'0'..=b'9' => (b - b'0' + 52) as u32,
                b'-' | b'+' => 62,
                b'_' | b'/' => 63,
                _ => return Err("bad base64 char".into()),
            };
            acc = (acc << 6) | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                buf.push(((acc >> bits) & 0xFF) as u8);
            }
        }
        let s = String::from_utf8(buf).map_err(|e| e.to_string())?;
        serde_json::from_str(&s).map_err(|e| e.to_string())
    }
}

/// 读凭据文件并展开（现读,不缓存文件内容本身）。
pub fn read_credential(platform: Platform) -> Option<RawCredential> {
    let path = credential_path(platform)?;
    let raw = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let now = chrono::Utc::now().timestamp();

    match platform {
        Platform::Codex => {
            let tokens = v.get("tokens")?;
            let access = tokens.get("access_token")?.as_str()?.to_string();
            let refresh = tokens.get("refresh_token")?.as_str().map(String::from);
            // Codex auth.json 无显式 expires 字段——last_refresh 仅作参考,
            // 过期判定交给 401 被动刷新（宁少勿多）
            let _ = v.get("last_refresh");
            let hint = jwt_claim(&access, "email")
                .and_then(|e| e.as_str().map(String::from))
                .or_else(|| {
                    jwt_claim(&access, "sub").and_then(|s| s.as_str().map(String::from))
                })
                .map(mask_hint);
            Some(RawCredential {
                access_token: access,
                refresh_token: refresh,
                access_expired: false,
                account_hint: hint,
                plan_hint: None,
            })
        }
        Platform::Claude => {
            let oauth = v.get("claudeAiOauth")?;
            let access = oauth.get("accessToken")?.as_str()?.to_string();
            let refresh = oauth.get("refreshToken")?.as_str().map(String::from);
            let expires_ms = oauth.get("expiresAt").and_then(|x| x.as_i64());
            // 提前 60s 判定过期：token 只剩几十秒时发请求多半白跑一轮
            // （且那轮 401 会把平台判死,不如直接判死等 CLI 续期）
            let access_expired = expires_ms
                .map(|ms| ms / 1000 <= now + EXPIRY_GRACE_SECS)
                .unwrap_or(false);
            let hint = oauth
                .get("subscriptionType")
                .and_then(|x| x.as_str())
                .map(|s| format!("subscription:{s}"));
            Some(RawCredential {
                access_token: access,
                refresh_token: refresh,
                access_expired,
                account_hint: hint,
                plan_hint: plan_from_oauth(oauth),
            })
        }
    }
}

/// 套餐名（凭据侧唯一来源——usage 端点不返回 plan）：
/// 主取 `subscriptionType`（"max"/"pro"）;缺失时从 `rateLimitTier`
/// （如 `default_claude_max_5x`）里挑已知套餐词兜底。返回小写。
fn plan_from_oauth(oauth: &Value) -> Option<String> {
    if let Some(s) = oauth.get("subscriptionType").and_then(|x| x.as_str()) {
        let t = s.trim().to_ascii_lowercase();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let tier = oauth
        .get("rateLimitTier")
        .and_then(|x| x.as_str())?
        .to_ascii_lowercase();
    ["enterprise", "team", "max", "pro", "free"]
        .iter()
        .find(|p| tier.contains(**p))
        .map(|p| (*p).to_string())
}

/// 账号掩码：保留前 3 字符 + ***（不足以 1 字符;绝不透出完整标识）。
fn mask_hint(s: String) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(3).collect();
    format!("{head}***")
}

/// 刷新失败分类。
pub enum RefreshOutcome {
    /// 拿到新 access（只存内存）。
    Renewed(String),
    /// 永久失效（invalid_grant 族）→ Dead。
    Dead,
    /// 瞬态失败（网络/5xx）→ 不降级。
    Transient,
    /// 无 refresh token 可用（调用方按瞬态处理;枚举完整保留分型空间）。
    #[allow(dead_code)]
    NoRefresh,
}

/// 静默刷新（不回写文件）。各平台端点/请求形状不同,分派之。
pub fn refresh_access(platform: Platform, refresh_token: &str) -> RefreshOutcome {
    match platform {
        Platform::Codex => refresh_codex(refresh_token),
        Platform::Claude => refresh_claude(refresh_token),
    }
}

/// 永久失效判定：HTTP 4xx 且 body 含 OAuth 标准错误码（invalid_grant/
/// invalid_refresh_token/unauthorized_client）或裸 401。
fn is_permanent_refresh_failure(status: u16, body: &str) -> bool {
    if status == 401 {
        return true;
    }
    if (400..500).contains(&status) {
        let lowered = body.to_ascii_lowercase();
        return lowered.contains("invalid_grant")
            || lowered.contains("invalid_refresh_token")
            || lowered.contains("unauthorized_client")
            || lowered.contains("invalid_client");
    }
    false
}

fn refresh_codex(refresh_token: &str) -> RefreshOutcome {
    // client_id 为 Codex CLI 公开标识
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", "app_EMoamEEZ73f0CkXaXp7hrann"),
        ("refresh_token", refresh_token),
    ];
    let agent = match super::http_agent() {
        Some(a) => a,
        None => return RefreshOutcome::Transient,
    };
    let resp = agent
        .post("https://auth.openai.com/oauth/token")
        .timeout(std::time::Duration::from_secs(15))
        .send_form(&form);
    handle_refresh_response(resp)
}

fn refresh_claude(refresh_token: &str) -> RefreshOutcome {
    // Claude Code 公开 client_id + 固定 scope。
    // 端点/UA 均按校准：只有 platform.claude.com 有效
    // （console.anthropic.com 已 404）,且限流桶认 `claude-cli/<v> （external, cli)`
    // ——换别的 UA 直接 429（与 usage 端点同族机制,见 claude.rs 模块头）。
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
        "refresh_token": refresh_token,
        "scope": "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
    });
    let agent = match super::http_agent() {
        Some(a) => a,
        None => return RefreshOutcome::Transient,
    };
    let resp = agent
        .post("https://platform.claude.com/v1/oauth/token")
        .set("User-Agent", &super::claude::ua_cli())
        .timeout(std::time::Duration::from_secs(15))
        .send_string(&body.to_string());
    handle_refresh_response(resp)
}

fn handle_refresh_response(
    resp: Result<ureq::Response, ureq::Error>,
) -> RefreshOutcome {
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            return if is_permanent_refresh_failure(code, &body) {
                RefreshOutcome::Dead
            } else {
                RefreshOutcome::Transient
            };
        }
        Err(_) => return RefreshOutcome::Transient, // 传输层错误（超时/DNS）
    };
    let body = match resp.into_string() {
        Ok(b) => b,
        Err(_) => return RefreshOutcome::Transient,
    };
    let v: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return RefreshOutcome::Transient,
    };
    match v.get("access_token").and_then(|x| x.as_str()) {
        Some(t) => RefreshOutcome::Renewed(t.to_string()),
        None => RefreshOutcome::Transient,
    }
}
