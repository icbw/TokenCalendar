//! Claude 桌面端额度采样（Claude 平台的零凭据补充数据源）。
//!
//! 背景：主路径读 `~/.claude/.credentials.json`,只有 CLI /
//! VS Code 扩展运行时才续期该文件;**Claude 桌面端**（含其 Code 标签页拉起的
//! claude.exe）自持登录态,不回写这份文件——只用桌面端的用户,文件内 token 过期后
//! 悬浮球永远停在 auth_failed。
//!
//! 桌面端自己每 ~15 分钟把订阅用量采样写进 `plan-usage-history.json`：
//! `{"version":2,"samples":[{"t":<unix ms>,"org":"<uuid>","u":{"fh":<5h %>,"sd":<7d %>}}]}`
//! （整数百分比,无 resets_at,无 opus/sonnet 分窗）。本模块只读这个文件的最新样本,
//! **不碰桌面端的加密 token 缓存**（config.json `oauth:tokenCache*`）——凭据红线不变。
//!
//! 取舍：仅当主路径拿不到数（auth_failed / 无凭据文件）时回落到这里;样本超过
//! `MAX_SAMPLE_AGE_SECS` 视为桌面端没在跑,不回落（旧读数冒充 ok 比显示失败更误导）。
//!
//! 路径：MSIX 安装（商店 / 新版安装器）的 Roaming 被虚拟化到
//! `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude`;传统安装在
//! `%APPDATA%\Claude`。两处都探,取 mtime 较新者。

use std::path::PathBuf;

use serde_json::Value;

use super::model::{FetchStatus, Platform, QuotaWindow, SubscriptionSnapshot};

const HISTORY_FILE: &str = "plan-usage-history.json";

/// 可回落样本的最大年龄（秒）：桌面端 15 分钟一采,留足睡眠唤醒 / 采样抖动余量。
pub const MAX_SAMPLE_AGE_SECS: i64 = 3600;

/// 候选文件（存在者;mtime 新者在前）。
fn history_paths() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![];
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("Claude"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        if let Ok(entries) = std::fs::read_dir(PathBuf::from(local).join("Packages")) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with("Claude_") {
                    dirs.push(e.path().join("LocalCache").join("Roaming").join("Claude"));
                }
            }
        }
    }
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = dirs
        .into_iter()
        .map(|d| d.join(HISTORY_FILE))
        .filter_map(|p| {
            let m = std::fs::metadata(&p).ok()?.modified().ok()?;
            Some((p, m))
        })
        .collect();
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files.into_iter().map(|(p, _)| p).collect()
}

/// 本机是否有桌面端采样文件（设置页发现 / 绑定前置校验用）。
pub fn is_present() -> bool {
    !history_paths().is_empty()
}

/// 最新样本（unix 秒, 5h %, 7d %）。`org` 给定且有匹配样本时只取该组织的样本
/// ——多组织账号切换过,避免拿另一个组织的额度冒充;无匹配则取全局最新。
fn latest_sample(body: &str, org: Option<&str>) -> Option<(i64, f64, f64)> {
    let v: Value = serde_json::from_str(body).ok()?;
    let samples = v.get("samples")?.as_array()?;
    let parse = |s: &Value| -> Option<(i64, f64, f64, Option<String>)> {
        let t = s.get("t")?.as_i64()?;
        let u = s.get("u")?;
        let fh = u.get("fh")?.as_f64()?;
        let sd = u.get("sd")?.as_f64()?;
        let o = s.get("org").and_then(|x| x.as_str()).map(String::from);
        Some((t / 1000, fh, sd, o))
    };
    let all: Vec<_> = samples.iter().filter_map(parse).collect();
    let pick = |want: Option<&str>| {
        all.iter()
            .filter(|(_, _, _, o)| want.map_or(true, |w| o.as_deref() == Some(w)))
            .max_by_key(|(t, ..)| *t)
            .map(|(t, fh, sd, _)| (*t, *fh, *sd))
    };
    org.and_then(|o| pick(Some(o))).or_else(|| pick(None))
}

/// 凭据文件里的组织 uuid（只读非敏感字段;文件缺失 / 过期都不影响）。
fn credential_org() -> Option<String> {
    let path = super::credentials::credential_path(Platform::Claude)?;
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.get("organizationUuid")?.as_str().map(String::from)
}

/// 由最新样本构造快照（样本过旧 / 文件不可读 → None）。
pub fn snapshot(now: i64) -> Option<SubscriptionSnapshot> {
    let org = credential_org();
    let (t, fh, sd) = history_paths()
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .find_map(|body| latest_sample(&body, org.as_deref()))?;
    build(t, fh, sd, now)
}

fn build(t: i64, fh: f64, sd: f64, now: i64) -> Option<SubscriptionSnapshot> {
    if now - t > MAX_SAMPLE_AGE_SECS {
        return None;
    }
    // 套餐名仍只能取凭据侧 subscriptionType（过期 token 不影响该字段）
    let plan = super::credentials::read_credential(Platform::Claude)
        .and_then(|c| c.plan_hint)
        .unwrap_or_else(|| "unknown".into());
    Some(SubscriptionSnapshot {
        platform: Platform::Claude,
        plan_type: plan,
        windows: vec![
            QuotaWindow { kind: "5h".into(), used_percent: fh, resets_at: None },
            QuotaWindow { kind: "7d".into(), used_percent: sd, resets_at: None },
        ],
        // 取样本时刻而非时刻：同一样本重复读出时 fetched_at 不推进,
        // 待机 / boost / 刷新完成判据都按「没有新数据」处理
        fetched_at: Some(t.min(now)),
        status: FetchStatus::Ok,
    })
}

/// 主路径结果的回落：仅 Claude 的 auth_failed / idle（无凭据）才换成桌面端样本。
pub fn fallback(snap: SubscriptionSnapshot, now: i64) -> SubscriptionSnapshot {
    if snap.platform != Platform::Claude
        || !matches!(snap.status, FetchStatus::AuthFailed | FetchStatus::Idle)
    {
        return snap;
    }
    snapshot(now).unwrap_or(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"{"version":2,"samples":[
        {"t":1000000,"org":"a","u":{"fh":10,"sd":3}},
        {"t":3000000,"org":"b","u":{"fh":40,"sd":9}},
        {"t":2000000,"org":"a","u":{"fh":20,"sd":4}},
        {"t":4000000,"org":"a","u":{"fh":"bad"}}
    ]}"#;

    #[test]
    fn latest_sample_prefers_matching_org_and_skips_malformed() {
        assert_eq!(latest_sample(BODY, Some("a")), Some((2000, 20.0, 4.0)));
        assert_eq!(latest_sample(BODY, None), Some((3000, 40.0, 9.0)));
        assert_eq!(latest_sample(BODY, Some("zzz")), Some((3000, 40.0, 9.0)), "无匹配组织回落全局最新");
        assert_eq!(latest_sample("{}", None), None);
    }

    #[test]
    fn stale_sample_is_not_used() {
        assert!(build(1000, 5.0, 1.0, 1000 + MAX_SAMPLE_AGE_SECS).is_some());
        assert!(build(1000, 5.0, 1.0, 1001 + MAX_SAMPLE_AGE_SECS).is_none());
        let s = build(1000, 5.0, 1.0, 1200).unwrap();
        assert_eq!(s.fetched_at, Some(1000));
        assert_eq!(s.windows.len(), 2);
        assert_eq!(s.status, FetchStatus::Ok);
    }

    #[test]
    fn fallback_leaves_other_statuses_alone() {
        let snap = SubscriptionSnapshot {
            platform: Platform::Claude,
            plan_type: "max".into(),
            windows: vec![],
            fetched_at: None,
            status: FetchStatus::RateLimited,
        };
        assert_eq!(fallback(snap, 0).status, FetchStatus::RateLimited);
    }
}
