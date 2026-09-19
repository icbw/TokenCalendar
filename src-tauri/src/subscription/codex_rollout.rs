//! Codex 侧的本地读数回溯（→ 实现;
//! []）。
//!
//! Codex CLI / Desktop 每收到一次模型响应就往 rollout 会话文件里写一条 `token_count`
//! 事件,**服务端在那次响应里回的限流状态被原样记在同一条事件的 `payload.rate_limits`
//! 上**。于是本机就有了一条与 Claude 桌面端 `plan-usage-history.json` 等价的额度读数
//! 历史,而且**比那条更好**：
//!
//! - **读数与代价写在同一行**。`payload.info.last_token_usage` 就是产生这次涨幅的那一笔
//!   调用的 token 明细。Claude 那条路要去 `collector.db` 按轮的 `ended_at` 切区间、
//!   还要把小时级缓存按轮体量摊回（见 bootstrap.rs）,两处近似在这里都不需要。
//!   这不是"更好看",是**实质差别**：里两种取法各干跑一遍,用 `collector.db` 取代价
//!   时拟合值随配对粒度在 1.73〜6.67 之间漂（3.9 倍),用同一行的 token 则稳定在
//!   8.15〜8.66（±3%）。`turn_raw` 是按**对话轮**聚合的,一整轮几十次调用的 token 全挂在
//!   `ended_at` 那一个时刻上,而这里的读数是**按调用**落的——错配随区间变短而放大。
//! - **每条读数自带 `plan_type`**。Claude 那条路只能拿"当前快照的套餐"回标整段历史,
//!   期间升过档就会标错;这里逐条都是准的,跨套餐的区间可以直接不建样本。
//! - **带 `resets_at`**,而 Codex 的快照本来就要显示窗尾。
//!
//! ## 它不只是冷启动：还是一条零请求的实时读数
//!
//! `rate_limits` 是 Codex 在**每次响应**里回的那个服务端数字,与 `wham/usage` 端点
//! 同源、同精度。所以这条路除了回溯建样本,
//! 还负责**把最新一条读数推进快照**（`update_snapshot`）——用贵模型时一个轮次就能吃掉
//! 5h 窗的十几二十个百分点,而取数是按预计消耗触发的、还要等采集器先看见那些 token;
//! 读数在文件里已经是真值了,没有理由让球上显示上一次取数的旧数。
//!
//! ## 配对方式：链式端点,一笔 token 都不丢
//!
//! 读数密到中位 8 秒一条,直接拿**相邻**两条配对的话,绝大多数区间会因为
//! `dt < MIN_PAIR_SECS` 被判出界,那一段的代价就再也进不了任何样本——那是**丢样本**,
//! 与既定原则相反（AGENTS.md / calib.rs：样本只增不删,不准靠丢样本降噪）。
//!
//! 所以这里按**链式端点**配对：端点之间至少相隔 `MIN_PAIR_SECS`,**跨过去的读数不是被
//! 丢掉,而是成了区间内点**——它们对应的调用照常计入该区间的代价。每一笔 token 恰好落在
//! 一个区间里,每一条读数要么是端点、要么是内点。
//!
//! 注意这**不是**"把区间拉长来降噪"：
//! 下界仍是 `MIN_PAIR_SECS` 这个既有常量,一个新阈值都没引入;真正的问题从来不是区间太短,
//! 是代价挂错了时刻,而这条路上它挂对了。
//!
//! ## 区间口径：`（t0, t1]`,端点那一笔算在区间内
//!
//! 与 bootstrap.rs / `record_pair` 同口径（左开右闭）。这一条过：把端点那一笔改成
//! 算进**下一个**区间（`[t0, t1)`）之后,拟合值在各配对粒度下的极差由 0.51 涨到 2.73
//! ——含端点更自洽,因为这条读数正是那笔调用的响应带回来的。
//!
//! ## 增量与工作量
//!
//! 水位线 = `max（usage_pair 里 src='rollout' 的 MAX（t1), 上次扫到的文件 mtime, now - 回溯上限)`。
//! **数据下界与文件下界取同一个值**（文件下界再退 `SCAN_LAG_SECS` 容错）——两者必须一致,
//! 否则会拿"读数齐全但调用缺了一半"的区间去建样本,代价系统性偏低。
//! 代价是每次扫描丢掉一个跨扫描边界的区间,按每次扫描进来几百条读数算可以忽略。
//!
//! 读数照常收割进 `desktop_sample` 永久留着（rollout 会被 Codex 归档 / 被用户清掉 /
//! `CODEX_HOME` 改指向,源没了历史也不能跟着丢）。

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::calib::{self, MIN_PAIR_SECS, Pair};
use super::cost;
use super::model::{FetchStatus, Platform, QuotaWindow, SnapshotSource, SubscriptionSnapshot};
use super::store::SubStore;

/// 本模块建出来的标定样本在 `usage_pair.src` 里的标记（水位线按它取）。
pub const PAIR_SRC: &str = "rollout";

/// 上次扫到的最新文件 mtime（`meta` 表的键）。
///
/// 没有它,"本机有 rollout 但一条 5h 读数都没有"这种账号（老版 CLI 只回报周窗）
/// 每次启动都会把整个回溯窗重扫一遍——水位线靠 `usage_pair` 推进,而它一条都建不出来。
const META_SCANNED_MTIME: &str = "codex_rollout_scanned_mtime";

/// 这一路上次是按哪一版准入判据建的样本（见 `calib:ADMISSION_RULE_VERSION`）。
const META_RULE_VER: &str = "codex_rollout_pair_rule";

/// 回溯上限（天）：**这是工作量上界,不是口径上界**。
///
/// Claude 那边的 14 天来自"源文件只保 14 天"与"越久远越可能跨套餐";这里两条都不成立
/// ——本机 rollout 留了 85 天,而套餐由每条读数自带、跨套餐的区间本来就会被跳过。
/// 所以这个数唯一的作用是给**首次扫描**的 I/O 封顶：本机 30 天 = 125 个文件 / 490 MB,
/// 90 天 = 293 个 / 884 MB,而两者拟合出来的系数只差 1%（8.28 vs 8.38）。
const HORIZON_DAYS: i64 = 30;

/// 文件下界相对数据下界再退这么多秒：mtime 的粒度、并发写入、时钟回拨都可能让
/// "文件 mtime 比它里面最后一行还早一点"。退一小时是纯保险,重读几个文件而已。
const SCAN_LAG_SECS: i64 = 3600;

/// 5h / 7d 滚动窗在 `rate_limits` 里的窗长（分钟）。
///
/// **必须按 `window_minutes` 认窗口,不能按 `primary` / `secondary` 的位置认**：
/// 本机 2026-07/08 的老记录里 `primary` 装的是**周窗**、`secondary` 为 null
/// （12,869 条,全是老版 CLI 的 plus 账号）。按位置取会把 7d 当成 5h。
const WINDOW_5H_MINUTES: i64 = 300;
const WINDOW_7D_MINUTES: i64 = 10080;

/// 拿不到模型名时的占位（匹配不上任何价目键 ⇒ 走回落价并标 unknown,与采集器同名）。
const UNKNOWN_MODEL: &str = "unknown";

/// 一条额度读数（`rate_limits` 里同一时刻的两个窗口 + 当时的套餐）。
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// unix 秒。
    pub t: i64,
    pub used5: f64,
    pub used7: f64,
    /// 该读数当时的套餐（`rate_limits.plan_type`;缺省 ""）。
    pub plan: String,
    /// 两个窗口的窗尾（unix 秒;上游给了才有）。标定不用,推快照时要——
    /// 悬浮球靠它显示「几点几分重置」。
    pub resets5: Option<i64>,
    pub resets7: Option<i64>,
}

/// 一次模型调用的 token 明细（`info.last_token_usage`,**单次值不是累积值**）。
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// unix 秒。
    pub t: i64,
    pub model: String,
    /// [输入（不含缓存), 输出, 缓存读, 缓存写]——与 `cost:Tokens` 同序。
    pub tokens: [i64; 4],
}

/// 一轮扫描的产出。
#[derive(Debug, Default)]
pub struct Scan {
    pub readings: Vec<Reading>,
    pub calls: Vec<Call>,
    /// 读到的文件数 / 全部候选文件数。
    pub files_read: usize,
    pub files_total: usize,
    pub bytes_read: u64,
    /// 有 `rate_limits` 但**不含 5h 窗口**的读数条数（老版 CLI 只回报周窗;
    /// 它们进不了标定——`Pair` 的两端是 5h 已用百分比——但这件事要在日志里看得）。
    pub weekly_only: usize,
}

/// Codex 家目录（`CODEX_HOME` 覆盖;与 `collector:codex` 同一套发现规则）。
fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| crate::collector::home_dir().map(|h| h.join(".codex")))
}

/// 全部候选 rollout 文件及其 mtime（unix 秒）。
fn rollout_files() -> Vec<(PathBuf, i64)> {
    let Some(home) = codex_home() else { return vec![] };
    let mut paths = vec![];
    crate::collector::jsonl::discover(&home.join("sessions"), true, &mut paths);
    crate::collector::jsonl::discover(&home.join("archived_sessions"), true, &mut paths);
    paths
        .into_iter()
        .filter_map(|p| {
            let m = std::fs::metadata(&p).ok()?.modified().ok()?;
            let secs = m.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
            Some((p, secs))
        })
        .collect()
}

/// 候选文件里最新的 mtime（unix 秒;无文件 → None）。收割前的廉价闸门：
/// 只 stat 不读内容,mtime 没动就不必再走一遍解析。
pub fn newest_mtime() -> Option<i64> {
    rollout_files().into_iter().map(|(_, m)| m).max()
}

/// 扫描 mtime 不早于 `mtime_floor` 的文件,取出 `t >= since` 的读数与调用。
fn scan(since: i64, mtime_floor: i64) -> Scan {
    let files = rollout_files();
    let mut out = Scan { files_total: files.len(), ..Default::default() };
    for (path, mtime) in files {
        // 文件是追加写的 ⇒ 里面每一行的时刻都 ≤ 它的 mtime。mtime 比下界还早的文件
        // 不可能含有要的行,连打开都不必。
        if mtime < mtime_floor {
            continue;
        }
        out.files_read += 1;
        out.bytes_read += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        scan_file(&path, since, &mut out);
    }
    out.readings.sort_by_key(|r| r.t);
    out.calls.sort_by_key(|c| c.t);
    out
}

/// 扫一个 rollout 文件（行式流读;先做子串预筛再解析 JSON）。
///
/// 预筛不是微优化：rollout 的字节数绝大部分是 `session_meta` 的 base_instructions 与
/// 消息正文,本机最大的单个文件 44 MB,逐行 serde 解析它们纯属浪费。
fn scan_file(path: &Path, since: i64, out: &mut Scan) {
    let Ok(file) = std::fs::File::open(path) else { return };
    let mut reader = std::io::BufReader::new(file);
    // 模型在 `turn_context` 行的 `payload.model`（轮级设置,token_count 行不带)——
    // 与 collector:codex 同一条。逐行推进,token_count 取当时最近的那个值。
    let mut model = String::new();
    let mut buf: Vec<u8> = vec![];
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // 按字节读 + lossy：`read_line` 遇到一个非法 UTF-8 字节就返回错误,那会让
        // **整个文件的剩余部分**被静默丢掉（本机最大的 rollout 44 MB）。写到一半的
        // 尾行同理——它解析不出 JSON 被跳过,mtime 还在动,下一轮自然重读。
        let line = String::from_utf8_lossy(&buf);
        let is_ctx = line.contains("\"turn_context\"");
        let is_tok = line.contains("\"token_count\"");
        if !is_ctx && !is_tok {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(payload) = v.get("payload") else { continue };
        // 精确键名：token_count 行有 `model_context_window` 而没有 `model`,不会误命中
        if let Some(m) = payload.get("model").and_then(|x| x.as_str()) {
            model = m.to_string();
        }
        if payload.get("type").and_then(|x| x.as_str()) != Some("token_count") {
            continue;
        }
        let Some(t) = payload_time(&v) else { continue };
        if t < since {
            continue;
        }
        if let Some(call) = parse_call(payload, t, &model) {
            out.calls.push(call);
        }
        match parse_reading(payload, t) {
            Some(r) => out.readings.push(r),
            None if payload.get("rate_limits").is_some_and(|r| !r.is_null()) => {
                out.weekly_only += 1;
            }
            None => {}
        }
    }
}

/// 行首时间戳 → unix 秒。
fn payload_time(line: &Value) -> Option<i64> {
    let ts = line.get("timestamp")?.as_str()?;
    crate::collector::rfc3339_to_millis(ts).map(|ms| ms.div_euclid(1000))
}

/// `payload.rate_limits` → 一条读数（**没有 5h 窗口就不成立**,返回 None）。
fn parse_reading(payload: &Value, t: i64) -> Option<Reading> {
    let rl = payload.get("rate_limits")?;
    let plan = rl.get("plan_type").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let (mut used5, mut used7) = (None, None);
    let (mut resets5, mut resets7) = (None, None);
    // 按 window_minutes 认窗口,不按 primary / secondary 的位置认（见常量注释）
    for slot in ["primary", "secondary"] {
        let Some(w) = rl.get(slot) else { continue };
        let (Some(minutes), Some(used)) = (
            w.get("window_minutes").and_then(|x| x.as_i64()),
            w.get("used_percent").and_then(|x| x.as_f64()),
        ) else {
            continue;
        };
        let reset = w.get("resets_at").and_then(|x| x.as_i64());
        match minutes {
            WINDOW_5H_MINUTES => (used5, resets5) = (Some(used), reset),
            WINDOW_7D_MINUTES => (used7, resets7) = (Some(used), reset),
            _ => {}
        }
    }
    // 7d 缺失不致命（它不参与标定,只是档案）;5h 缺失这条读数就没用
    Some(Reading { t, used5: used5?, used7: used7.unwrap_or(0.0), plan, resets5, resets7 })
}

/// `payload.info.last_token_usage` → 一次调用的 token 明细。
///
/// 口径与 `collector:codex` 一致：`input_tokens` 是**含缓存**的原值,减掉缓存读得到
/// 计费口径的输入;`output_tokens` 保持 provider 口径（含 reasoning）。
/// 本机验过 `Σ last_token_usage.total` 与文件末条 `total_token_usage.total` 逐位相同
/// ⇒ 它确实是**单次值**,不必差分。
fn parse_call(payload: &Value, t: i64, model: &str) -> Option<Call> {
    let usage = payload.get("info")?.get("last_token_usage")?;
    let get = |k: &str| usage.get(k).and_then(|x| x.as_i64()).unwrap_or(0).max(0);
    // 两个字段名并存：新格式 cached_input_tokens / 旧 cache_read_input_tokens
    let cache_read = get("cached_input_tokens").max(get("cache_read_input_tokens"));
    let tokens = [
        (get("input_tokens") - cache_read).max(0),
        get("output_tokens"),
        cache_read,
        get("cache_write_input_tokens"),
    ];
    if tokens.iter().all(|x| *x == 0) {
        return None;
    }
    let model = if model.is_empty() { UNKNOWN_MODEL } else { model };
    Some(Call { t, model: model.to_string(), tokens })
}

/// 纯逻辑：由读数序列 + 调用序列构造标定样本（可单测,不碰 IO）。
///
/// 两个入参都要求**按时间升序**。返回 `（样本, （7d 两端), 分模型明细, 该区间的套餐)`。
/// 配对方式与区间口径见模块头。
pub fn build_pairs(
    readings: &[Reading],
    calls: &[Call],
) -> Vec<(Pair, (f64, f64), BTreeMap<String, [i64; 4]>, String)> {
    // 端点链：相邻端点至少相隔 MIN_PAIR_SECS。被跳过的读数成为区间内点,
    // 它们对应的调用照常计入该区间的代价——**一笔 token 都不丢**。
    let mut ends: Vec<&Reading> = vec![];
    for r in readings {
        match ends.last() {
            None => ends.push(r),
            Some(prev) if r.t - prev.t >= MIN_PAIR_SECS => ends.push(r),
            _ => {}
        }
    }

    let mut out = vec![];
    let mut cur = 0usize; // 调用游标,随区间单调前进
    for w in ends.windows(2) {
        let (a, b) = (w[0], w[1]);
        // 左开右闭 （t0, t1]：端点那一笔算在本区间（口径理由见模块头）
        while cur < calls.len() && calls[cur].t <= a.t {
            cur += 1;
        }
        let mut breakdown: BTreeMap<String, [i64; 4]> = BTreeMap::new();
        while cur < calls.len() && calls[cur].t <= b.t {
            let slot = breakdown.entry(calls[cur].model.clone()).or_insert([0; 4]);
            for (k, v) in calls[cur].tokens.iter().enumerate() {
                slot[k] += v;
            }
            cur += 1;
        }
        // 跨套餐的区间不建样本：scale 是「这个套餐一个窗口有多大」,两端不是同一个
        // 套餐时这条样本无从归属（本机历史里 plan_type 在 plus / edu 之间跳过 40 多次）。
        if a.plan != b.plan || breakdown.is_empty() {
            continue;
        }
        // at = t1：区间右端点,与 store:recompute_stale_costs / bootstrap 同口径
        let (total, unknown) = cost::cost_of_breakdown(Platform::Codex, &breakdown, b.t);
        let pair = Pair {
            t0: a.t,
            t1: b.t,
            used5_0: a.used5,
            used5_1: b.used5,
            // rollout 的每条读数都带窗尾 ⇒ 跨重置这件事可以**直接判**,不必靠
            // 「读数变小了」推断（语义见 calib:Pair:window_reset）。
            resets5_0: a.resets5,
            resets5_1: b.resets5,
            cost: total,
            unknown_cost: unknown,
        };
        if !pair.usable(calib::scale(Platform::Codex)) {
            continue; // 跨重置 / 间隔越界 / 零代价 / 比值离谱,与另外两路同一套筛选
        }
        out.push((pair, (a.used7, b.used7), breakdown, b.plan.clone()));
    }
    out
}

/// 用最新的 rollout 读数推进 Codex 快照（**零网络**;返回是否真的改了）。
///
/// 这一步的价值不在省请求,在**时效**：`rate_limits` 是 Codex 在每次响应里带回来的
/// **同一个服务端数字**,只是走本地文件到手。用贵模型时一个轮次就能吃掉 5h 窗的
/// 十几二十个百分点,而取数是按预计消耗触发的——触发判据本身要等采集器先看见那些
/// token。读数在文件里已经是真值了,没有理由还让球上显示上一次取数的旧数。
///
/// 两条约束：
/// - **必须比上次读数新**（`t > prev.fetched_at`）。旧样本不能冒充进展,否则
///   `advanced` 会误判、账目被错误清零。
/// - **上下都改**。Claude 那条 `apply_flat_sample` 只下,因为它处理的是「兜底轮没涨」
///   那个特殊情形;这里是一条**完整的、更新的**读数,两个方向都该照搬。
fn update_snapshot(store: &SubStore, readings: &[Reading], now: i64) -> bool {
    let Some(r) = readings.last() else { return false };
    let prev = store.load_snapshot(Platform::Codex);
    if prev.as_ref().and_then(|p| p.fetched_at).is_some_and(|t0| r.t <= t0) {
        return false; // 不比手上的新,什么都证明不了
    }
    let snap = SubscriptionSnapshot {
        platform: Platform::Codex,
        // 读数自带套餐;缺省时沿用上一条快照的,别把已知的 plan 退化成 unknown
        plan_type: if r.plan.is_empty() {
            prev.as_ref().map(|p| p.plan_type.clone()).unwrap_or_else(|| "unknown".into())
        } else {
            r.plan.clone()
        },
        windows: vec![
            QuotaWindow { kind: "5h".into(), used_percent: r.used5, resets_at: r.resets5 },
            QuotaWindow { kind: "7d".into(), used_percent: r.used7, resets_at: r.resets7 },
        ],
        // 取读数时刻而非时刻：它就是那次 API 调用发生的时刻
        fetched_at: Some(r.t.min(now)),
        status: FetchStatus::Ok,
        source: SnapshotSource::Rollout,
    };
    if prev.as_ref() == Some(&snap) {
        return false;
    }
    if let Err(e) = store.save_snapshot(&snap) {
        crate::dev_log!("[subscription] codex rollout snapshot save failed: {e}");
        return false;
    }
    // 读数推进了,账目必须跟着清零——`demand` 记的是「**距上次读数**大约消耗了百分之几」,
    // 而这条读数已经把那些消耗算进去了。不清的话估算会重复计一遍,把取数触发得比
    // 需要的早（取数是按这个估算排的）。取走的账目直接丢掉不可惜：同一批 token 的
    // 标定样本由本模块自己按读数时刻建,比在线路那条准（见模块头）。
    let dropped = super::demand::take_account(Platform::Codex, now);
    crate::dev_log!(
        "[subscription] codex snapshot from rollout: 5h={:.0}% 7d={:.0}% plan={} age={}s          (no request, est {:.2}% cleared)",
        r.used5,
        r.used7,
        snap.plan_type,
        now - r.t,
        dropped.cost * calib::scale(Platform::Codex)
    );
    true
}

/// 收割 + 增量标定 + 快照推进（主轮询每轮调用一次;**零网络、零凭据**）。
/// 返回 `（本次新收割的读数条数, 本次新建的标定样本数, 快照是否变了)`。
pub fn ingest(store: &SubStore, now: i64) -> (usize, usize, bool) {
    let floor = now - HORIZON_DAYS * 86_400;
    // 判据升版 ⇒ 下界整体退回回溯上限,把「文件还在的那一段」按新判据重建一次
    // （旧行等新样本建出来之后再删,见 calib:ADMISSION_RULE_VERSION）。
    let rule_stale = store.meta_i64(META_RULE_VER) != Some(calib::ADMISSION_RULE_VERSION);
    // 数据下界：三者取大。文件下界由它再退 SCAN_LAG_SECS——**两者必须同源**,
    // 否则会拿"读数齐全但调用缺了一半"的区间去建样本（见模块头）。
    let since = if rule_stale {
        floor
    } else {
        store
            .latest_pair_t1(Platform::Codex, PAIR_SRC)
            .unwrap_or(0)
            .max(store.meta_i64(META_SCANNED_MTIME).unwrap_or(0))
            .max(floor)
    };
    let scan = scan(since, since - SCAN_LAG_SECS);
    if scan.files_read == 0 {
        return (0, 0, false);
    }

    let samples: Vec<_> =
        scan.readings.iter().map(|r| (r.t, r.used5, r.used7, r.plan.clone())).collect();
    let harvested = store.insert_samples_of(Platform::Codex, &samples).unwrap_or(0);

    // 样本按套餐分组落库：insert_pairs 一次只带一个 plan_type,而回溯窗里可能跨过套餐变更。
    let mut by_plan: BTreeMap<String, Vec<(Pair, (f64, f64), String)>> = BTreeMap::new();
    for (pair, used7, breakdown, plan) in build_pairs(&scan.readings, &scan.calls) {
        let json = serde_json::to_string(&breakdown).unwrap_or_else(|_| "{}".into());
        by_plan.entry(plan).or_default().push((pair, used7, json));
    }
    // 重建：先有新样本,再删它们覆盖到的那一段旧行——顺序反过来就会在这中间把
    // 「删了又建不出来」的区间永久丢掉（会话文件被清掉 / CODEX_HOME 改指向都会这样）。
    let span = by_plan
        .values()
        .flatten()
        .fold(None::<(i64, i64)>, |acc, (p, _, _)| match acc {
            None => Some((p.t0, p.t1)),
            Some((lo, hi)) => Some((lo.min(p.t0), hi.max(p.t1))),
        });
    let removed = match (rule_stale, span) {
        (true, Some((lo, hi))) => {
            let r = store.delete_pairs_in(Platform::Codex, PAIR_SRC, lo, hi);
            let _ = store.set_meta_i64(META_RULE_VER, calib::ADMISSION_RULE_VERSION);
            Some((r, lo, hi))
        }
        _ => None,
    };
    let mut pairs = 0;
    for (plan, items) in &by_plan {
        pairs += store.insert_pairs(Platform::Codex, items, PAIR_SRC, plan).unwrap_or(0);
    }
    if let Some((r, lo, hi)) = removed {
        crate::dev_log!(
            "[subscription] codex rollout pairs rebuilt under admission rule v{}: \
             -{} old, +{} new over [{}, {}]",
            calib::ADMISSION_RULE_VERSION,
            r,
            pairs,
            lo,
            hi
        );
    }
    if pairs > 0 {
        calib::refit_from_store(store, Platform::Codex);
    }

    // 扫到哪儿了：下一轮的文件下界。**必须在建样本之后写**——中途 panic / 关机时
    // 宁可下一轮重扫一遍（幂等）,也不能记成"已扫过"而把那段读数永久跳过。
    if let Some(m) = newest_mtime() {
        let _ = store.set_meta_i64(META_SCANNED_MTIME, m);
    }

    if harvested > 0 || pairs > 0 || scan.weekly_only > 0 {
        log_scan(store, &scan, harvested, pairs);
    }
    // 快照推进排在最后：它只依赖读数,与建样本互不影响,放这儿保证即使建样本
    // 一条都没建出来（比如全是 Δ=0 的区间）,球上的数照样是最新的。
    let snapshot_changed = update_snapshot(store, &scan.readings, now);
    (harvested, pairs, snapshot_changed)
}

/// 收割结果 + 读数密度一行（与 Claude 那一路同形状,便于两边对照看）。
fn log_scan(store: &SubStore, scan: &Scan, harvested: usize, pairs: usize) {
    let total = store.sample_count(Platform::Codex);
    let span_days = match store.sample_span(Platform::Codex) {
        Some((first, last)) if total > 1 => (last - first) as f64 / 86_400.0,
        _ => 0.0,
    };
    let gap = store.median_sample_gap(Platform::Codex).unwrap_or(0);
    crate::dev_log!(
        "[subscription] codex rollout harvest +{} reading(s) from {}/{} file(s) ({:.1} MB) \
         → kept {} over {:.1}d (median gap {}s) → +{} pair(s)",
        harvested,
        scan.files_read,
        scan.files_total,
        scan.bytes_read as f64 / 1_048_576.0,
        total,
        span_days,
        gap,
        pairs
    );
    if scan.weekly_only > 0 {
        // 老版 CLI（本机 2026-07/08 的 plus 记录）只回报周窗 ⇒ 这些读数进不了标定。
        // 不当错误,但要让"它们存在且被跳过"这件事在日志里看得。
        crate::dev_log!(
            "[subscription] codex rollout: {} reading(s) carried no 5h window (weekly-only, skipped)",
            scan.weekly_only
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(t: i64, used5: f64, plan: &str) -> Reading {
        Reading { t, used5, used7: 10.0, plan: plan.into(), resets5: None, resets7: None }
    }

    /// 真实量级的一笔调用：每 1% 配额约合 $0.12 等价用量（本机实测 scale≈8 %/美元）,
    /// 太小的 token 量会让隐含比值落到 calib 的可信带之外,样本会被当成账目错配丢掉。
    fn call(t: i64, model: &str, input: i64) -> Call {
        Call { t, model: model.into(), tokens: [input, 0, 0, 0] }
    }

    fn line(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn window_is_picked_by_length_not_by_slot() {
        // 老版 CLI 把**周窗**放在 primary、secondary 为 null（本机 12,869 条）
        let weekly_only = line(
            r#"{"rate_limits":{"primary":{"used_percent":18.0,"window_minutes":10080},
                 "secondary":null,"plan_type":"plus"}}"#,
        );
        assert_eq!(parse_reading(&weekly_only, 100), None, "没有 5h 窗口 ⇒ 这条读数不成立");

        // 新格式：5h 在 primary、7d 在 secondary
        let both = line(
            r#"{"rate_limits":{"primary":{"used_percent":7.0,"window_minutes":300},
                 "secondary":{"used_percent":19.0,"window_minutes":10080},"plan_type":"edu"}}"#,
        );
        assert_eq!(
            parse_reading(&both, 100),
            Some(Reading {
                t: 100,
                used5: 7.0,
                used7: 19.0,
                plan: "edu".into(),
                resets5: None,
                resets7: None
            })
        );

        // 位置反过来也要认得（只按 window_minutes 判）
        let swapped = line(
            r#"{"rate_limits":{"primary":{"used_percent":19.0,"window_minutes":10080},
                 "secondary":{"used_percent":7.0,"window_minutes":300},"plan_type":"edu"}}"#,
        );
        assert_eq!(parse_reading(&swapped, 100).unwrap().used5, 7.0);
    }

    #[test]
    fn call_tokens_are_cache_exclusive_and_single_shot() {
        let payload = line(
            r#"{"info":{"last_token_usage":{"input_tokens":26025,"cached_input_tokens":18176,
                 "cache_write_input_tokens":40,"output_tokens":151,"reasoning_output_tokens":68},
                 "total_token_usage":{"input_tokens":999999}}}"#,
        );
        let c = parse_call(&payload, 100, "gpt-5.6-sol").unwrap();
        assert_eq!(c.tokens, [26025 - 18176, 151, 18176, 40], "输入要减掉缓存读");
        assert_eq!(c.model, "gpt-5.6-sol");
        // 全零的一笔不落（有些 token_count 只带 total）
        let empty = line(r#"{"info":{"last_token_usage":{"input_tokens":0,"output_tokens":0}}}"#);
        assert_eq!(parse_call(&empty, 100, "m"), None);
    }

    #[test]
    fn model_comes_from_the_preceding_turn_context() {
        let dir = std::env::temp_dir().join(format!("tc_cxroll_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        let lines = [
            r#"{"timestamp":"2026-09-17T15:00:00.000Z","type":"turn_context","payload":{"type":"turn_context","model":"gpt-5.6-sol","cwd":"E:\\x"}}"#,
            r#"{"timestamp":"2026-09-17T15:00:10.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"output_tokens":10}},"rate_limits":{"primary":{"used_percent":3.0,"window_minutes":300},"secondary":{"used_percent":9.0,"window_minutes":10080},"plan_type":"edu"}}}"#,
            // 正文行必须被预筛挡掉,不能因为它解析不出 payload 就中断整个文件
            r#"{"timestamp":"2026-09-17T15:00:11.000Z","type":"event_msg","payload":{"type":"agent_message","message":"token_count 这三个字出现在正文里也不该出事"}}"#,
            r#"{"timestamp":"2026-09-17T15:00:20.000Z","type":"turn_context","payload":{"type":"turn_context","model":"gpt-5.6-luna"}}"#,
            r#"{"timestamp":"2026-09-17T15:00:30.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":2000,"output_tokens":20}}}}"#,
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let mut out = Scan::default();
        scan_file(&path, 0, &mut out);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(out.calls.len(), 2);
        assert_eq!(out.calls[0].model, "gpt-5.6-sol");
        assert_eq!(out.calls[1].model, "gpt-5.6-luna", "第二条要用后一条 turn_context 的模型");
        assert_eq!(out.readings.len(), 1, "只有第一条 token_count 带 rate_limits");
        assert_eq!(out.readings[0].plan, "edu");
        assert_eq!(out.weekly_only, 0);
    }

    /// 快照推进：只认**比手上更新**的读数,两个方向都改,窗尾与套餐照搬。
    #[test]
    fn snapshot_advances_only_on_a_newer_reading() {
        let dir = std::env::temp_dir().join(format!("tc_cxroll_snap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = SubStore::open(&dir.join("subscriptions.db")).unwrap();
        let api = SubscriptionSnapshot {
            platform: Platform::Codex,
            plan_type: "edu".into(),
            windows: vec![
                QuotaWindow { kind: "5h".into(), used_percent: 20.0, resets_at: Some(9_000) },
                QuotaWindow { kind: "7d".into(), used_percent: 30.0, resets_at: Some(99_000) },
            ],
            fetched_at: Some(1_000),
            status: FetchStatus::Ok,
            source: SnapshotSource::Api,
        };
        store.save_snapshot(&api).unwrap();

        // ① 更旧的读数证明不了什么
        let old = Reading { t: 900, used5: 55.0, used7: 60.0, plan: "edu".into(), resets5: None, resets7: None };
        assert!(!update_snapshot(&store, &[old], 2_000));
        assert_eq!(store.load_snapshot(Platform::Codex).unwrap(), api);

        // ② 更新的读数照搬进去（**上修**——贵模型一个轮次吃掉几十个点的那种情形）
        let fresh = Reading {
            t: 1_500,
            used5: 55.0,
            used7: 34.0,
            plan: "edu".into(),
            resets5: Some(9_500),
            resets7: Some(99_500),
        };
        assert!(update_snapshot(&store, std::slice::from_ref(&fresh), 2_000));
        let got = store.load_snapshot(Platform::Codex).unwrap();
        assert_eq!(got.source, SnapshotSource::Rollout, "不能冒充 api——它不能当在线样本的端点");
        assert_eq!(got.fetched_at, Some(1_500), "取读数时刻,不是本轮时刻");
        assert_eq!(got.windows[0].used_percent, 55.0);
        assert_eq!(got.windows[0].resets_at, Some(9_500), "窗尾照搬");
        assert_eq!(got.windows[1].used_percent, 34.0);
        assert_eq!(got.plan_type, "edu");

        // ③ 同一条读数再来一次 = 空操作（不许白广播）
        assert!(!update_snapshot(&store, &[fresh], 2_000));

        // ④ 下修同样要跟（5h 是滚动窗,空闲期余量自己回升）
        let later = Reading { t: 2_000, used5: 3.0, used7: 34.0, plan: String::new(), resets5: None, resets7: None };
        assert!(update_snapshot(&store, &[later], 2_100));
        let got = store.load_snapshot(Platform::Codex).unwrap();
        assert_eq!(got.windows[0].used_percent, 3.0);
        assert_eq!(got.plan_type, "edu", "读数没带套餐时沿用上一条,别退化成 unknown");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一个非法 UTF-8 字节（写到一半 / 磁盘问题）不许把整个文件的剩余部分吃掉。
    #[test]
    fn a_broken_byte_does_not_truncate_the_rest_of_the_file() {
        let dir = std::env::temp_dir().join(format!("tc_cxroll_utf8_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        let good = br#"{"timestamp":"2026-09-17T15:00:30.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":2000,"output_tokens":20}},"rate_limits":{"primary":{"used_percent":3.0,"window_minutes":300},"secondary":{"used_percent":9.0,"window_minutes":10080},"plan_type":"edu"}}}"#;
        let mut bytes: Vec<u8> = vec![];
        bytes.extend_from_slice(b"{\"type\":\"token_count\",\"x\":\"");
        bytes.push(0xFF); // 非法字节,这一行本来就解析不出 JSON
        bytes.extend_from_slice(b"\"}\n");
        bytes.extend_from_slice(good);
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).unwrap();
        let mut out = Scan::default();
        scan_file(&path, 0, &mut out);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(out.readings.len(), 1, "坏字节之后的行必须照常读到");
        assert_eq!(out.calls.len(), 1);
    }

    #[test]
    fn interior_readings_become_interval_points_not_dropped_samples() {
        // 三条读数,前两条相隔 10 秒（< MIN_PAIR_SECS）⇒ 中间那条当内点。
        // 它区间里的调用**必须照样计入**,否则就是在丢样本。
        let readings =
            vec![reading(1_000, 0.0, "edu"), reading(1_010, 1.0, "edu"), reading(1_060, 4.0, "edu")];
        let calls = vec![
            call(1_005, "gpt-5.6-sol", 60_000),  // 内点之前
            call(1_030, "gpt-5.6-sol", 60_000),  // 内点之后
            call(1_060, "gpt-5.6-sol", 60_000),  // 端点那一笔,算在区间内
        ];
        let out = build_pairs(&readings, &calls);
        assert_eq!(out.len(), 1, "端点链只出一条样本（1000 → 1060）");
        assert_eq!(out[0].0.t0, 1_000);
        assert_eq!(out[0].0.t1, 1_060);
        assert_eq!(out[0].2["gpt-5.6-sol"][0], 180_000, "三笔调用一笔不少");
        assert_eq!(out[0].0.used5_1 - out[0].0.used5_0, 4.0);
        assert_eq!(out[0].3, "edu");
    }

    #[test]
    fn endpoint_call_belongs_to_the_interval_that_ends_on_it() {
        // (t0, t1]：t0 那一笔属于**上一个**区间,t1 那一笔属于本区间
        let readings = vec![reading(1_000, 0.0, "edu"), reading(1_060, 2.0, "edu"), reading(1_120, 4.0, "edu")];
        let calls = vec![call(1_000, "gpt-5.6-sol", 90_000), call(1_060, "gpt-5.6-sol", 60_000)];
        let out = build_pairs(&readings, &calls);
        assert_eq!(out.len(), 1, "第二个区间没有调用 ⇒ 零代价,不建样本");
        assert_eq!(out[0].0.t1, 1_060);
        assert_eq!(out[0].2["gpt-5.6-sol"][0], 60_000, "t0 那一笔不算进来");
    }

    #[test]
    fn plan_change_inside_an_interval_is_skipped() {
        let readings = vec![
            reading(1_000, 0.0, "plus"),
            reading(1_060, 3.0, "edu"), // 套餐变了 ⇒ 这个区间不建样本
            reading(1_120, 6.0, "edu"),
        ];
        let calls = vec![call(1_030, "gpt-5.6-sol", 60_000), call(1_100, "gpt-5.6-sol", 60_000)];
        let out = build_pairs(&readings, &calls);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].0.t0, out[0].0.t1), (1_060, 1_120));
        assert_eq!(out[0].3, "edu");
    }

    #[test]
    fn window_reset_is_judged_by_the_window_tail() {
        let calls = vec![call(1_030, "gpt-5.6-sol", 60_000)];
        // 掉了 = 窗口重置（没有窗尾时的兜底判据）
        let reset = vec![reading(1_000, 30.0, "edu"), reading(1_060, 2.0, "edu")];
        assert!(build_pairs(&reset, &calls).is_empty());
        // **窗尾前移 = 重置的直接证据**,哪怕读数看着还在涨（2026-09-19）
        let crossed = vec![
            Reading { resets5: Some(1_050), ..reading(1_000, 5.0, "edu") },
            Reading { resets5: Some(19_050), ..reading(1_060, 8.0, "edu") },
        ];
        assert!(build_pairs(&crossed, &calls).is_empty());
        // 同一个窗口内（窗尾没动）照常建样本
        let same = vec![
            Reading { resets5: Some(19_050), ..reading(1_000, 5.0, "edu") },
            Reading { resets5: Some(19_050), ..reading(1_060, 8.0, "edu") },
        ];
        assert_eq!(build_pairs(&same, &calls).len(), 1);
    }

    /// 没涨的读数**收**,只要这段消耗本来就不该动一格（PHASE15 §9-8 定案）。
    #[test]
    fn a_flat_reading_below_one_step_still_makes_a_sample() {
        let flat = vec![reading(1_000, 5.0, "edu"), reading(1_060, 5.0, "edu")];
        // 按 Codex 的出厂预设,这一笔的预计涨幅约 0.7 个百分点 ⇒ 不够动一格是正常的
        // （配对用的那笔 60k 折合约 2.1 个百分点,已经越过 ZERO_DELTA_SLACK 的上限）
        let small = vec![call(1_030, "gpt-5.6-sol", 20_000)];
        let out = build_pairs(&flat, &small);
        assert_eq!(out.len(), 1, "Δ=0 的有效样本计回分母");
        assert_eq!(out[0].0.used5_1 - out[0].0.used5_0, 0.0);
        // 预计要涨好几个百分点却纹丝不动 ⇒ 账目不对,照丢
        let huge = vec![call(1_030, "gpt-5.6-sol", 60_000_000)];
        assert!(build_pairs(&flat, &huge).is_empty());
        // 没有调用 = 别处在用,不能拿来标定本地换算
        let no_call = vec![reading(1_000, 5.0, "edu"), reading(1_060, 9.0, "edu")];
        assert!(build_pairs(&no_call, &[]).is_empty());
    }

    #[test]
    fn auto_review_heavy_intervals_are_kept_out_of_calibration() {
        // codex-auto-review 是路由标签不是模型名 ⇒ 折价但标 unknown;过半即不参与标定
        let readings = vec![reading(1_000, 0.0, "edu"), reading(1_060, 2.0, "edu")];
        let calls = vec![call(1_030, "codex-auto-review", 200_000)];
        assert!(build_pairs(&readings, &calls).is_empty(), "未知模型占比过半 ⇒ 不建样本");
    }
}
