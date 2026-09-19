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

use super::model::{FetchStatus, Platform, QuotaWindow, SnapshotSource, SubscriptionSnapshot};

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

/// 候选文件里最新的 mtime（unix 秒;无文件 → None）。收割前的廉价闸门：
/// 桌面端 15 分钟才写一次,mtime 没动就不必重新解析 50KB JSON。
pub fn history_mtime() -> Option<i64> {
    history_paths()
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok())
        .filter_map(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .max()
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

/// 全部样本（按时间升序;标定冷启动用,见 bootstrap.rs）。同样按组织过滤。
pub fn all_samples() -> Vec<(i64, f64, f64)> {
    let org = credential_org();
    let mut out: Vec<(i64, f64, f64)> = history_paths()
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .find_map(|body| all_samples_of(&body, org.as_deref()))
        .unwrap_or_default();
    out.sort_by_key(|(t, ..)| *t);
    out
}

/// 单个文件里的全部样本（组织匹配规则同 `latest_sample`：有匹配就只要匹配的）。
fn all_samples_of(body: &str, org: Option<&str>) -> Option<Vec<(i64, f64, f64)>> {
    let v: Value = serde_json::from_str(body).ok()?;
    let samples = v.get("samples")?.as_array()?;
    let all: Vec<(i64, f64, f64, Option<String>)> = samples
        .iter()
        .filter_map(|s| {
            let t = s.get("t")?.as_i64()?;
            let u = s.get("u")?;
            Some((
                t / 1000,
                u.get("fh")?.as_f64()?,
                u.get("sd")?.as_f64()?,
                s.get("org").and_then(|x| x.as_str()).map(String::from),
            ))
        })
        .collect();
    let pick = |want: Option<&str>| -> Vec<(i64, f64, f64)> {
        all.iter()
            .filter(|(_, _, _, o)| want.map_or(true, |w| o.as_deref() == Some(w)))
            .map(|(t, fh, sd, _)| (*t, *fh, *sd))
            .collect()
    };
    let matched = org.map(|o| pick(Some(o))).unwrap_or_default();
    Some(if matched.is_empty() { pick(None) } else { matched })
}

/// 全局最新样本（不论新旧;`snapshot` 与在线用量探测共用）。
fn latest_any() -> Option<(i64, f64, f64)> {
    let org = credential_org();
    history_paths()
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .find_map(|body| latest_sample(&body, org.as_deref()))
}

/// 由最新样本构造快照（样本过旧 / 文件不可读 → None）。
pub fn snapshot(now: i64) -> Option<SubscriptionSnapshot> {
    let (t, fh, sd) = latest_any()?;
    build(t, fh, sd, now)
}

/// 桌面端采样与上次读数的比对容差（百分点）：桌面端记的是整数百分比,
/// 与 API 的小数值最多差一个取整位,不足此值不算涨。
const ROUND_TOL: f64 = 1.0;

/// 在线用量探测结果（兜底轮是否值得打 API;语义见 `probe_growth`）。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum OnlineProbe {
    /// 桌面端有比上次读数更新的样本,且没涨 → 不必打网络。
    NoGrowth,
    /// 桌面端样本显示涨了（本地 token 之外的用量,即在线 / 网页）→ 该取一轮。
    Growth,
    /// 没有可用样本 / 样本不比上次读数新 → 证明不了什么,按常规取数。
    Unknown,
}

/// 用桌面端采样探测「上次读数之后有没有涨」（零网络）。
/// 只在主路径凭据可用的兜底轮调用——凭据失效时走的是 `fallback`,本就读同一份样本。
pub fn probe_growth(prev: &SubscriptionSnapshot, now: i64) -> OnlineProbe {
    let Some(fetched_at) = prev.fetched_at else { return OnlineProbe::Unknown };
    if prev.status != FetchStatus::Ok {
        return OnlineProbe::Unknown;
    }
    let Some((t, fh, sd)) = latest_any() else { return OnlineProbe::Unknown };
    // 样本必须比上次读数新,否则「没涨」只是因为它还停在更早的时刻;
    // 过旧的样本（桌面端没在跑）同样不作数
    if t <= fetched_at || now - t > MAX_SAMPLE_AGE_SECS {
        return OnlineProbe::Unknown;
    }
    let used = |kind: &str| prev.windows.iter().find(|w| w.kind == kind).map(|w| w.used_percent);
    let grew = [("5h", fh), ("7d", sd)]
        .iter()
        .any(|(kind, sample)| used(kind).is_none_or(|prev| *sample > prev + ROUND_TOL));
    if grew { OnlineProbe::Growth } else { OnlineProbe::NoGrowth }
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
        // 待机 / 刷新完成判据都按「没有新数据」处理
        fetched_at: Some(t.min(now)),
        status: FetchStatus::Ok,
        source: SnapshotSource::Desktop,
    })
}

/// 兜底轮「没涨」时的读数正。
///
/// `probe_growth` 判 `NoGrowth` 的前提就是手上有一条**比上次读数更新**的样本,只是
/// 没涨。但 5h / 7d 都是**滚动窗口**：离开期间旧用量不断过期,「没涨」里其实还藏着
/// 「掉了」——旧版把这条样本整个丢掉、快照原样保留,于是球上的余量停在偏低的旧值,
/// 要等复工那一笔 token 或手动刷新才纠正,而此刻明明已经握着更新的样本。
///
/// 这里只做**下**（样本低出一个取整位以上才改;涨的那一路走 `Growth` 正常取数,
/// ±1 以内当整数取整噪声不动），并且：
/// - `resets_at` 保留——桌面端不提供窗尾,旧值仍是当前这个窗口的窗尾;真过期了
///   前端自己会把时刻显示成横杠（`clockAt` 对已过时刻返回 null）;
/// - `fetched_at` **不推进** ⇒ 仍判「没有进展」,不清取数账目、不落标定样本、
///   退避与触发一概不受影响,改的只是显示用的读数;
/// - 真改了就把来源标成 `desktop`——正值是**整数**百分比,不能当作在线标定样本的
///   端点（见 `SnapshotSource`）。
pub fn apply_flat_sample(prev: &SubscriptionSnapshot, now: i64) -> SubscriptionSnapshot {
    match latest_any() {
        Some(sample) => apply_sample(prev, sample, now),
        None => prev.clone(),
    }
}

/// 纯逻辑（单测直接覆盖）：把一条样本的**下降**进快照,语义见 `apply_flat_sample`。
fn apply_sample(
    prev: &SubscriptionSnapshot,
    (t, fh, sd): (i64, f64, f64),
    now: i64,
) -> SubscriptionSnapshot {
    let mut out = prev.clone();
    if now - t > MAX_SAMPLE_AGE_SECS {
        return out; // 桌面端没在跑,旧样本不作数
    }
    let mut changed = false;
    for w in out.windows.iter_mut() {
        let sample = match w.kind.as_str() {
            "5h" => fh,
            "7d" => sd,
            _ => continue,
        };
        if sample < w.used_percent - ROUND_TOL {
            w.used_percent = sample;
            changed = true;
        }
    }
    if changed {
        // 正值是**整数**百分比 ⇒ 不能再当在线标定样本的端点（见 SnapshotSource）
        out.source = SnapshotSource::Desktop;
    }
    out
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

    fn ok_snapshot(fetched: i64, used_5h: f64, used_7d: f64) -> SubscriptionSnapshot {
        SubscriptionSnapshot {
            platform: Platform::Claude,
            plan_type: "max".into(),
            windows: vec![
                QuotaWindow { kind: "5h".into(), used_percent: used_5h, resets_at: None },
                QuotaWindow { kind: "7d".into(), used_percent: used_7d, resets_at: None },
            ],
            fetched_at: Some(fetched),
            status: FetchStatus::Ok,
            source: SnapshotSource::Api,
        }
    }

    #[test]
    fn probe_needs_a_sample_newer_than_the_last_reading() {
        // 无 fetched_at / 非 ok 的上次读数证明不了什么
        let mut snap = ok_snapshot(1_000, 10.0, 5.0);
        snap.fetched_at = None;
        assert_eq!(probe_growth(&snap, 2_000), OnlineProbe::Unknown);
        let mut snap = ok_snapshot(1_000, 10.0, 5.0);
        snap.status = FetchStatus::RateLimited;
        assert_eq!(probe_growth(&snap, 2_000), OnlineProbe::Unknown);
    }

    fn used(s: &SubscriptionSnapshot, kind: &str) -> f64 {
        s.windows.iter().find(|w| w.kind == kind).unwrap().used_percent
    }

    /// 只下修、不上修,且永不推进 fetched_at（本轮仍算「没有进展」:不清账目、不落样本）。
    #[test]
    fn flat_sample_only_corrects_downwards() {
        let prev = ok_snapshot(1_000, 30.0, 20.0);
        // 5h 滚动过期 ⇒ 余量恢复,样本比上次读数低 → 修进去
        let out = apply_sample(&prev, (1_500, 22.0, 20.0), 1_600);
        assert_eq!(used(&out, "5h"), 22.0, "下降修进快照");
        assert_eq!(used(&out, "7d"), 20.0, "没动的窗口不改");
        assert_eq!(out.fetched_at, prev.fetched_at, "fetched_at 永不推进");
        assert_eq!(out.source, SnapshotSource::Desktop, "整数读数 → 标成桌面端来源");
        assert_eq!(out.windows[0].resets_at, prev.windows[0].resets_at, "窗尾保留");
    }

    #[test]
    fn flat_sample_ignores_rounding_noise_and_growth() {
        let prev = ok_snapshot(1_000, 30.0, 20.0);
        // 整数取整最多差一个位 ⇒ ±1 以内不动（也不该把来源改掉）
        let noise = apply_sample(&prev, (1_500, 29.2, 19.5), 1_600);
        assert_eq!(noise, prev, "取整噪声不触发任何改动");
        // 涨的那一路走 Growth 正常取数,这里不负责
        let grew = apply_sample(&prev, (1_500, 44.0, 20.0), 1_600);
        assert_eq!(grew, prev, "上修不在本函数职责内");
    }

    #[test]
    fn flat_sample_ignores_stale_samples() {
        let prev = ok_snapshot(1_000, 30.0, 20.0);
        let now = 1_500 + MAX_SAMPLE_AGE_SECS + 1;
        assert_eq!(apply_sample(&prev, (1_500, 5.0, 2.0), now), prev, "桌面端没在跑,旧样本不作数");
    }

    #[test]
    fn fallback_leaves_other_statuses_alone() {
        let snap = SubscriptionSnapshot {
            platform: Platform::Claude,
            plan_type: "max".into(),
            windows: vec![],
            fetched_at: None,
            status: FetchStatus::RateLimited,
            source: SnapshotSource::Api,
        };
        assert_eq!(fallback(snap, 0).status, FetchStatus::RateLimited);
    }
}
