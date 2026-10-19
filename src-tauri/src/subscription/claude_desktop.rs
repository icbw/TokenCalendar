//! Claude 桌面端额度采样（Claude 平台的零凭据补充数据源）。
//!
//! 主路径读 `~/.claude/.credentials.json`,只有 CLI / VS Code 扩展运行时才续期该文件;
//! **Claude 桌面端**（含其 Code 标签页拉起的 claude.exe）自持登录态,不回写这份文件——
//! 只用桌面端的用户,文件内 token 过期后悬浮球会一直停在 auth_failed。
//!
//! 桌面端取用量时顺带把订阅用量采样写进 `plan-usage-history.json`：
//! `{"version":2,"samples":[{"t":<unix ms>,"org":"<uuid>","u":{"fh":<5h %>,"sd":<7d %>}}]}`
//! （整数百分比,无 resets_at,无 opus/sonnet 分窗）。本模块只读这个文件的最新样本,
//! **不碰桌面端的加密 token 缓存**（config.json `oauth:tokenCache*`）——凭据红线不变。
//!
//! **采样节律**：不是无条件每 15 分钟。
//! 启动时取一次;之后后台定时器按「距最近一次**交互**多久」分档——30 分钟内每 5 分钟,
//! 之后每 15 分钟。**交互** = 点对话底部的上下文窗口按钮（用量弹窗）或右键托盘图标
//! （config.json `planUsageLastTrayOpenAt` 记最近一次）。每个 tick 还要过两道闸：
//!  服务端配置 `pollRequiresTrayOpenWithinHours` 生效且距最近交互超过该小时数 → 暂停
//! （日志 `background poll paused: tray not opened recently`）;
//!  系统锁屏 / 空闲 >= 10 分钟 → 跳过（回来后立刻补一条）。写样本另有 4.5 分钟去重。
//! 所以**只要用户近期在桌面端操作过,这份文件就是活的**（≤ 5〜15 分钟滞后）;长时间没交互
//! 才会停在上次启动那一条。弹窗里的实时数（每次回复后在 claude.exe 进程内存里更新）不落盘。
//!
//! **暂停闸可以精确算出来**：最近交互时刻在 config.json
//! `planUsageLastTrayOpenAt`;小时数在 `fcache`（GrowthBook 开关缓存：8 字节头 + gzip JSON）
//! 特性 `1294982044` 的 `pollRequiresTrayOpenWithinHours`（本机 24）。暂停时刻 =
//! 最近交互 + 小时数;闸门生效而从没交互过 = 一直暂停（与桌面端同口径）。
//! 两份文件任一读不到就退回只看样本年龄,不猜。样本文件保留期 30 天,`u` 里还可能有
//! so / sn / oa / cw / om / op / xu 等键（本模块只用 fh / sd）。
//!
//! 取舍：仅当主路径拿不到数（auth_failed / 无凭据文件）时回落到这里。样本「新鲜」的判据
//! 见 `sample_fresh`：超过 `MAX_SAMPLE_AGE_SECS` 一律不作数（桌面端没在跑 / 锁屏）;
//! 采样**已暂停**时只认 `PAUSED_SAMPLE_GRACE_SECS` 内的样本（启动时那一条）,过了就判过期,
//! 不再干等满 1 小时。不新鲜就不回落（旧读数冒充 ok 比显示失败更误导）,前端按
//! `auth_failed` 提示用户去点一下上下文窗口按钮恢复。主循环每次看一眼都用
//! `stale_candidate` 检查,已过期的本地读数当场落成 `auth_failed`,不等下一次取数轮。
//!
//! 路径：MSIX 安装（商店 / 新版安装器）的 Roaming 被虚拟化到
//! `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude`;桌面端 2.42304 起又把它
//! 外迁到 `%LOCALAPPDATA%\Claude-Data\<包族名小写>`,旧目录只留 `.user-data-*` 标记
//! （规则见 `data_dirs_in`）;传统安装在 `%APPDATA%\Claude`。都探,取 mtime 较新者。
//! 暂停闸的 config.json / fcache 与采样文件同目录,跟着一起走。

use std::path::PathBuf;

use serde_json::Value;

use super::model::{FetchStatus, Platform, QuotaWindow, SnapshotSource, SubscriptionSnapshot};

const HISTORY_FILE: &str = "plan-usage-history.json";

/// 可回落样本的最大年龄（秒）：交互后最慢 15 分钟一采,留足睡眠唤醒 / 采样抖动余量。
pub const MAX_SAMPLE_AGE_SECS: i64 = 3600;

/// 采样已暂停时样本还能用多久（秒）：桌面端正常节律最慢 15 分钟一条,暂停后只剩启动那一条,
/// 过了这个时长它就不可能再代表当前用量。
pub const PAUSED_SAMPLE_GRACE_SECS: i64 = 900;

/// 桌面端 config.json 里最近一次交互（unix 毫秒）的键。
const CONFIG_FILE: &str = "config.json";
const LAST_INTERACTION_KEY: &str = "planUsageLastTrayOpenAt";
/// 桌面端 GrowthBook 开关缓存与暂停闸特性。特性键是名字的哈希,桌面端改版可能变——
/// 变了就读成「闸门关」或读不到,最坏退回旧的 1 小时判据。
const FEATURE_CACHE_FILE: &str = "fcache";
const POLL_GATE_FEATURE: &str = "1294982044";
const POLL_GATE_FIELD: &str = "pollRequiresTrayOpenWithinHours";

/// MSIX 包族名前缀（商店版 / 新版安装器各一个包族）。
const PACKAGE_PREFIXES: [&str; 2] = ["Claude_", "AnthropicPBC.Claude_"];
/// MSIX 数据外迁的目标根：`%LOCALAPPDATA%\Claude-Data\<包族名小写>`。
const MOVED_DATA_ROOT: &str = "Claude-Data";

/// 外迁状态标记（桌面端写在包内旧目录里;`leaving` 在它的上一级）。
fn marker(dir: &std::path::Path, state: &str, family: &str) -> bool {
    dir.join(format!(".user-data-{state}.{family}")).exists()
}

/// 桌面端数据目录候选（纯文件系统判断;单测用临时目录覆盖）。
///
/// 桌面端 2.42304 起把 MSIX 包内的 Roaming（`Packages\<包族>\LocalCache\Roaming\Claude`）
/// 外迁到 `%LOCALAPPDATA%\Claude-Data\<包族名小写>`（`--user-data-dir` 启动）,包内旧目录
/// 只留标记：`left` = 已外迁,以外迁目录为准;`back` / `blocked` = 搬回 / 没搬成,以包内为准
/// （外迁目录里可能是过期副本）;`leaving`（在旧目录的上一级）= 搬运中,两边都算。
/// 没有任何标记 = 旧版桌面端,两边都算。最后统一按采样文件 mtime 取新,两边都在也不会选错。
fn data_dirs_in(local: Option<&std::path::Path>, appdata: Option<&std::path::Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![];
    if let Some(appdata) = appdata {
        dirs.push(appdata.join("Claude")); // 传统（Squirrel）安装
    }
    let Some(local) = local else { return dirs };
    let moved_root = local.join(MOVED_DATA_ROOT);
    // 包内旧目录明确说「数据在这儿」的包族（它们的外迁目录不作数）
    let mut moved_back: Vec<String> = vec![];
    if let Ok(entries) = std::fs::read_dir(local.join("Packages")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !PACKAGE_PREFIXES.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let family = name.to_lowercase();
            let roaming = e.path().join("LocalCache").join("Roaming");
            let inside = roaming.join("Claude");
            let left = marker(&inside, "left", &family);
            let back = marker(&inside, "back", &family) || marker(&inside, "blocked", &family);
            let leaving = marker(&roaming, "leaving", &family);
            if back {
                moved_back.push(family);
                dirs.push(inside);
            } else if !left || leaving {
                dirs.push(inside);
            }
        }
    }
    // 外迁目录：只认包族名形状的子目录（与桌面端 `[a-z0-9.-]+_[a-z0-9]+` 同口径）
    if let Ok(entries) = std::fs::read_dir(&moved_root) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let family_like = name.split_once('_').is_some_and(|(a, b)| {
                !a.is_empty()
                    && !b.is_empty()
                    && a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
                    && b.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            });
            if family_like && !moved_back.contains(&name) {
                dirs.push(e.path());
            }
        }
    }
    dirs
}

/// 候选文件（存在者;mtime 新者在前）。
fn history_paths() -> Vec<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let dirs = data_dirs_in(local.as_deref(), appdata.as_deref());
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

/// 桌面端后台采样的暂停闸（零网络,只读两份本机文件）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SamplerGate {
    /// 最近一次交互（unix 秒）;None = 从没交互过。
    pub last_interaction: Option<i64>,
    /// 交互后多少小时内允许后台采样;None = 读不到（未知）,`Some（0)` = 闸门没开。
    pub window_hours: Option<f64>,
}

impl SamplerGate {
    /// 后台采样从何时起暂停（unix 秒）;None = 不会暂停或闸门未知。
    pub fn paused_at(&self) -> Option<i64> {
        let hours = self.window_hours?;
        if hours <= 0.0 {
            return None;
        }
        // 闸门生效而从没交互过：桌面端同样直接判暂停
        Some(self.last_interaction.map_or(i64::MIN, |t| t + (hours * 3600.0) as i64))
    }

    pub fn is_paused(&self, now: i64) -> bool {
        self.paused_at().is_some_and(|p| now >= p)
    }
}

/// 读暂停闸（与 mtime 最新的那份采样文件同目录）。
pub fn sampler_gate() -> SamplerGate {
    let Some(dir) = history_paths().into_iter().next().and_then(|p| p.parent().map(PathBuf::from)) else {
        return SamplerGate { last_interaction: None, window_hours: None };
    };
    SamplerGate {
        last_interaction: std::fs::read_to_string(dir.join(CONFIG_FILE))
            .ok()
            .and_then(|body| last_interaction_of(&body)),
        window_hours: poll_gate_hours(&dir.join(FEATURE_CACHE_FILE)),
    }
}

fn last_interaction_of(config_body: &str) -> Option<i64> {
    let v: Value = serde_json::from_str(config_body).ok()?;
    v.get(LAST_INTERACTION_KEY)?.as_f64().map(|ms| (ms / 1000.0) as i64)
}

/// 开关缓存里的暂停小时数,按 （路径, mtime) 记住上次结果——文件 40KB、解压后 160KB,
/// 主循环 30 秒看一眼,不值得每次都解。
fn poll_gate_hours(path: &std::path::Path) -> Option<f64> {
    use std::sync::Mutex;
    type Slot = Option<(PathBuf, std::time::SystemTime, Option<f64>)>;
    static CACHE: Mutex<Slot> = Mutex::new(None);
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let mut slot = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((p, m, hours)) = slot.as_ref() {
        if p == path && *m == mtime {
            return *hours;
        }
    }
    let hours = std::fs::read(path).ok().and_then(|raw| poll_gate_hours_of(&raw));
    *slot = Some((path.to_path_buf(), mtime, hours));
    hours
}

/// 纯解析（单测直接覆盖）：头部之后找 gzip 魔数解压,取特性值。解得开但没有这个特性
/// = 闸门没下发 = `Some（0)`（桌面端缺省 0,不暂停）;解不开 = None（未知）。
fn poll_gate_hours_of(raw: &[u8]) -> Option<f64> {
    use std::io::Read;
    let head = &raw[..raw.len().min(64)];
    let start = head.windows(3).position(|w| w == [0x1f, 0x8b, 0x08])?;
    let mut body = String::new();
    flate2::read::GzDecoder::new(&raw[start..]).read_to_string(&mut body).ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    let hours = v
        .get("features")?
        .as_object()?
        .get(POLL_GATE_FEATURE)
        .and_then(|f| f.get("value"))
        .and_then(|val| val.get(POLL_GATE_FIELD))
        .and_then(|h| h.as_f64())
        .unwrap_or(0.0);
    Some(hours.max(0.0))
}

/// 样本在 `now` 时是否还代表当前用量（判据见模块头「取舍」）。
pub fn sample_fresh(t: i64, now: i64, gate: &SamplerGate) -> bool {
    let age = now - t;
    if age > MAX_SAMPLE_AGE_SECS {
        return false;
    }
    !gate.is_paused(now) || age <= PAUSED_SAMPLE_GRACE_SECS
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
    build(t, fh, sd, now, &sampler_gate())
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
    // 不新鲜的样本（桌面端没在跑 / 采样已暂停）同样不作数
    if t <= fetched_at || !sample_fresh(t, now, &sampler_gate()) {
        return OnlineProbe::Unknown;
    }
    let used = |kind: &str| prev.windows.iter().find(|w| w.kind == kind).map(|w| w.used_percent);
    let grew = [("5h", fh), ("7d", sd)]
        .iter()
        .any(|(kind, sample)| used(kind).is_none_or(|prev| *sample > prev + ROUND_TOL));
    if grew { OnlineProbe::Growth } else { OnlineProbe::NoGrowth }
}

fn build(t: i64, fh: f64, sd: f64, now: i64, gate: &SamplerGate) -> Option<SubscriptionSnapshot> {
    if !sample_fresh(t, now, gate) {
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
/// 「掉了」;若丢掉这条样本,球上的余量会停在偏低的旧值,直到复工那一笔 token 或手动刷新。
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
        Some(sample) => apply_sample(prev, sample, now, &sampler_gate()),
        None => prev.clone(),
    }
}

/// 纯逻辑（单测直接覆盖）：把一条样本的**下降**进快照,语义见 `apply_flat_sample`。
fn apply_sample(
    prev: &SubscriptionSnapshot,
    (t, fh, sd): (i64, f64, f64),
    now: i64,
    gate: &SamplerGate,
) -> SubscriptionSnapshot {
    let mut out = prev.clone();
    if !sample_fresh(t, now, gate) {
        return out; // 桌面端没在跑 / 采样已暂停,旧样本不作数
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

/// 零请求推进：桌面端采样文件有了**更新的**样本,而当前快照本来就靠本地读数撑着
/// （凭据失效 / 无凭据 / 上一条也是桌面端样本）→ 返回应落库的新快照。
///
/// 与 `fallback` 同一判据（只在 API 主路径给不出数时介入）:API 读数在手时不动它——
/// 那条有服务端实时值、`source=api` 还要留给在线标定;限流冷却期（rate_limited）同理不碰。
/// 推进条件是严格更新（`fetched_at` 变大）,所以重复调用是幂等的。没有它,样本落地后球上
/// 要等下一次取数轮才看得见（token 驱动 / 30 分钟兜底）。
pub fn advance_candidate(prev: Option<&SubscriptionSnapshot>, now: i64) -> Option<SubscriptionSnapshot> {
    decide_advance(prev, snapshot(now)?)
}

/// 纯逻辑（单测直接覆盖）：见 `advance_candidate`。
fn decide_advance(prev: Option<&SubscriptionSnapshot>, cand: SubscriptionSnapshot) -> Option<SubscriptionSnapshot> {
    let Some(prev) = prev else { return Some(cand) };
    let on_local = match prev.status {
        FetchStatus::AuthFailed | FetchStatus::Idle => true,
        FetchStatus::Ok => prev.source == SnapshotSource::Desktop,
        _ => false,
    };
    let newer = match (cand.fetched_at, prev.fetched_at) {
        (Some(c), Some(p)) => c > p,
        (Some(_), None) => true,
        _ => false,
    };
    (on_local && (newer || prev.status != FetchStatus::Ok)).then_some(cand)
}

/// 本地读数过期：当前快照是桌面端样本撑着的 ok,主路径给不出数,而样本已不新鲜
/// （采样暂停 / 桌面端没在跑）→ 该落成 `auth_failed`（窗口数据由 `save_snapshot` 保留）。
/// 主循环每次看一眼都调,红点在暂停后一个看一眼周期内出现,不等取数轮。
/// `primary_live` = 凭据文件在且未过期、未判死——那时下一轮 API 自会更新,这里不插手
/// （闭包：只在快照真靠本地读数撑着时才去读凭据文件）。
pub fn stale_candidate(
    prev: Option<&SubscriptionSnapshot>,
    now: i64,
    primary_live: impl FnOnce() -> bool,
) -> bool {
    let Some(prev) = prev else { return false };
    on_stale_local_reading(prev, false)
        && !primary_live()
        && !latest_any().is_some_and(|(t, ..)| sample_fresh(t, now, &sampler_gate()))
}

/// 纯逻辑（单测直接覆盖）：快照是否正靠本地读数撑着、且主路径帮不上。
fn on_stale_local_reading(prev: &SubscriptionSnapshot, primary_live: bool) -> bool {
    !primary_live && prev.status == FetchStatus::Ok && prev.source == SnapshotSource::Desktop
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
        assert!(build(1000, 5.0, 1.0, 1000 + MAX_SAMPLE_AGE_SECS, &OPEN).is_some());
        assert!(build(1000, 5.0, 1.0, 1001 + MAX_SAMPLE_AGE_SECS, &OPEN).is_none());
        let s = build(1000, 5.0, 1.0, 1200, &OPEN).unwrap();
        assert_eq!(s.fetched_at, Some(1000));
        assert_eq!(s.windows.len(), 2);
        assert_eq!(s.status, FetchStatus::Ok);
    }

    /// 外迁标记四态 + 无标记旧版 + 传统安装,按桌面端真实目录形状造临时树。
    #[test]
    fn data_dirs_follow_user_data_move_markers() {
        let root = std::env::temp_dir().join(format!("tc-claude-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (local, appdata) = (root.join("local"), root.join("roaming"));
        let pkg = |name: &str| local.join("Packages").join(name).join("LocalCache").join("Roaming").join("Claude");
        let moved = |fam: &str| local.join("Claude-Data").join(fam);
        let touch = |p: PathBuf| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"").unwrap();
        };
        // 已外迁：旧目录只剩 left 标记
        std::fs::create_dir_all(moved("claude_left1")).unwrap();
        touch(pkg("Claude_left1").join(".user-data-left.claude_left1"));
        // 搬回：外迁目录是过期副本,不作数
        std::fs::create_dir_all(moved("claude_back1")).unwrap();
        touch(pkg("Claude_back1").join(".user-data-back.claude_back1"));
        // 没搬成
        touch(pkg("Claude_blk1").join(".user-data-blocked.claude_blk1"));
        // 搬运中：两边都算
        std::fs::create_dir_all(moved("claude_go1")).unwrap();
        touch(pkg("Claude_go1").join(".user-data-left.claude_go1"));
        touch(local.join("Packages").join("Claude_go1").join("LocalCache").join("Roaming").join(".user-data-leaving.claude_go1"));
        // 旧版桌面端：无标记
        std::fs::create_dir_all(pkg("AnthropicPBC.Claude_old1")).unwrap();
        // 不像包族名的目录不认;别的应用的包不认
        std::fs::create_dir_all(local.join("Claude-Data").join("NotAFamily")).unwrap();
        std::fs::create_dir_all(pkg("Other_app1")).unwrap();

        let mut got = data_dirs_in(Some(&local), Some(&appdata));
        got.sort();
        let mut want = vec![
            appdata.join("Claude"),
            moved("claude_left1"),
            pkg("Claude_back1"),
            pkg("Claude_blk1"),
            pkg("Claude_go1"),
            moved("claude_go1"),
            pkg("AnthropicPBC.Claude_old1"),
        ];
        want.sort();
        assert_eq!(got, want);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 闸门未知（读不到开关缓存）= 只看样本年龄,即旧行为。
    const OPEN: SamplerGate = SamplerGate { last_interaction: None, window_hours: None };

    #[test]
    fn paused_at_follows_last_interaction_plus_window() {
        let g = |last, hours| SamplerGate { last_interaction: last, window_hours: hours };
        assert_eq!(g(Some(1_000), Some(24.0)).paused_at(), Some(1_000 + 86_400));
        assert_eq!(g(Some(1_000), Some(0.0)).paused_at(), None, "闸门没下发 = 不暂停");
        assert_eq!(g(Some(1_000), None).paused_at(), None, "读不到小时数不猜");
        assert!(g(None, Some(24.0)).is_paused(0), "闸门生效而从没交互过 = 一直暂停");
        assert!(!g(Some(1_000), Some(24.0)).is_paused(1_000 + 86_399));
        assert!(g(Some(1_000), Some(24.0)).is_paused(1_000 + 86_400));
    }

    /// 暂停后只认宽限期内的样本（启动那一条）;没暂停 / 闸门未知仍是 1 小时。
    #[test]
    fn paused_sampler_shortens_sample_freshness() {
        let paused = SamplerGate { last_interaction: Some(0), window_hours: Some(24.0) };
        let live = SamplerGate { last_interaction: Some(90_000), window_hours: Some(24.0) };
        let now = 100_000;
        assert!(sample_fresh(now - PAUSED_SAMPLE_GRACE_SECS, now, &paused));
        assert!(!sample_fresh(now - PAUSED_SAMPLE_GRACE_SECS - 1, now, &paused), "暂停后超过宽限即过期");
        assert!(sample_fresh(now - 3_000, now, &live), "采样活着时 1 小时内都算");
        assert!(sample_fresh(now - 3_000, now, &OPEN), "闸门未知退回旧判据");
        assert!(!sample_fresh(now - MAX_SAMPLE_AGE_SECS - 1, now, &live));
        assert!(build(now - 3_000, 5.0, 1.0, now, &paused).is_none(), "暂停后的旧样本不回落");
    }

    #[test]
    fn last_interaction_reads_config_millis() {
        assert_eq!(last_interaction_of(r#"{"planUsageLastTrayOpenAt":1791822948513}"#), Some(1_791_822_948));
        assert_eq!(last_interaction_of(r#"{"locale":"zh-CN"}"#), None);
        assert_eq!(last_interaction_of("not json"), None);
    }

    /// 按桌面端 fcache 的真实形状造：8 字节头 + gzip JSON。
    fn fcache_bytes(json: &str) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(json.as_bytes()).unwrap();
        let mut raw = b"CLF\x02\x00\x9a\xb7\xe2".to_vec();
        raw.extend(enc.finish().unwrap());
        raw
    }

    #[test]
    fn poll_gate_hours_parses_feature_cache() {
        let with = fcache_bytes(
            r#"{"timestamp":1,"features":{"1294982044":{"value":{"pollRequiresTrayOpenWithinHours":24},"on":true}}}"#,
        );
        assert_eq!(poll_gate_hours_of(&with), Some(24.0));
        let without = fcache_bytes(r#"{"timestamp":1,"features":{"1":{"value":true}}}"#);
        assert_eq!(poll_gate_hours_of(&without), Some(0.0), "没下发 = 闸门关");
        assert_eq!(poll_gate_hours_of(b"CLF\x02garbage"), None, "解不开 = 未知");
    }

    /// 只有「主路径帮不上 + 当前是桌面端样本撑着的 ok」才可能被判过期。
    #[test]
    fn stale_only_applies_to_local_readings() {
        use FetchStatus::*;
        use SnapshotSource::{Api, Desktop};
        let s = |status, source| SubscriptionSnapshot { status, source, ..ok_snapshot(0, 0.0, 0.0) };
        assert!(on_stale_local_reading(&s(Ok, Desktop), false));
        assert!(!on_stale_local_reading(&s(Ok, Desktop), true), "凭据可用时交给下一轮 API");
        assert!(!on_stale_local_reading(&s(Ok, Api), false), "API 读数不归这里管");
        assert!(!on_stale_local_reading(&s(AuthFailed, Desktop), false), "已经是过期态");
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
        let out = apply_sample(&prev, (1_500, 22.0, 20.0), 1_600, &OPEN);
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
        let noise = apply_sample(&prev, (1_500, 29.2, 19.5), 1_600, &OPEN);
        assert_eq!(noise, prev, "取整噪声不触发任何改动");
        // 涨的那一路走 Growth 正常取数,这里不负责
        let grew = apply_sample(&prev, (1_500, 44.0, 20.0), 1_600, &OPEN);
        assert_eq!(grew, prev, "上修不在本函数职责内");
    }

    #[test]
    fn flat_sample_ignores_stale_samples() {
        let prev = ok_snapshot(1_000, 30.0, 20.0);
        let now = 1_500 + MAX_SAMPLE_AGE_SECS + 1;
        assert_eq!(apply_sample(&prev, (1_500, 5.0, 2.0), now, &OPEN), prev, "桌面端没在跑,旧样本不作数");
    }

    fn status_snapshot(status: FetchStatus, source: SnapshotSource, fetched: Option<i64>) -> SubscriptionSnapshot {
        SubscriptionSnapshot { status, source, fetched_at: fetched, ..ok_snapshot(0, 0.0, 0.0) }
    }

    /// 只在 API 主路径给不出数时推进;API 读数在手 / 限流冷却期都不动;严格更新才推进。
    #[test]
    fn advance_only_when_running_on_local_readings() {
        use FetchStatus::*;
        use SnapshotSource::{Api, Desktop};
        let cand = || build(2_000, 7.0, 3.0, 2_100, &OPEN).unwrap();
        let adv = |prev: &SubscriptionSnapshot| decide_advance(Some(prev), cand()).is_some();
        assert!(decide_advance(None, cand()).is_some(), "刚绑定、还没有快照");
        assert!(adv(&status_snapshot(AuthFailed, Api, None)), "凭据失效 → 本地读数顶上");
        assert!(adv(&status_snapshot(Idle, Api, None)));
        assert!(adv(&status_snapshot(AuthFailed, Api, Some(9_999))), "失败态不看时刻,有读数就顶");
        assert!(adv(&status_snapshot(Ok, Desktop, Some(1_000))), "上一条也是本地读数,新的更新");
        assert!(!adv(&status_snapshot(Ok, Desktop, Some(2_000))), "同一条读数不重复推进");
        assert!(!adv(&status_snapshot(Ok, Desktop, Some(3_000))), "比快照旧不退回去");
        assert!(!adv(&status_snapshot(Ok, Api, Some(1_000))), "API 读数在手不动");
        assert!(!adv(&status_snapshot(RateLimited, Api, None)), "限流冷却期不碰");
        assert!(!adv(&status_snapshot(NetworkFailed, Api, None)));
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
