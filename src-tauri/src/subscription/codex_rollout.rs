//! Codex 侧的本地读数回溯。
//!
//! Codex CLI / Desktop 每收到一次模型响应就往 rollout 会话文件里写一条 `token_count`
//! 事件,**服务端在那次响应里回的限流状态被原样记在同一条事件的 `payload.rate_limits`
//! 上**。于是本机有一条与 Claude 桌面端 `plan-usage-history.json` 等价的额度读数历史,
//! 而且**比那条更好**：
//!
//! - **读数与代价写在同一行**。`payload.info.last_token_usage` 就是产生这次涨幅的那一笔
//!   调用的 token 明细。Claude 那条路要去 `collector.db` 按轮的 `ended_at` 切区间、
//!   还要把小时级缓存按轮体量摊回（见 bootstrap.rs）,两处近似在这里都不需要。
//!   这是实质差别：`turn_raw` 按**对话轮**聚合,一整轮几十次调用的 token 全挂在 `ended_at`
//!   那一个时刻上,而这里的读数是**按调用**落的;用 `collector.db` 取代价时拟合值随配对粒度
//!   漂移数倍,用同一行的 token 则稳定在 ±3% 以内。
//! - **每条读数自带 `plan_type`**。Claude 那条路只能拿「当前快照的套餐」回标整段历史,
//!   期间升过档就会标错;这里逐条都是准的,跨套餐的区间可以直接不建样本。
//! - **带 `resets_at`**,而 Codex 的快照本来就要显示窗尾。
//!
//! ## 它不只是冷启动：还是一条零请求的实时读数
//!
//! `rate_limits` 是 Codex 在**每次响应**里回的那个服务端数字,与 `wham/usage` 端点
//! 同源、同精度（两边都是整数百分比）。所以这条路除了回溯建样本,还负责**把最新一条读数
//! 推进快照**（`update_snapshot`）——用贵模型时一个轮次就能吃掉 5h 窗的十几二十个百分点,
//! 而取数是按预计消耗触发的、还要等采集器先看见那些 token;读数在文件里已经是真值了,
//! 没有理由让球上显示上一次取数的旧数。
//!
//! ## 配对方式：链式端点,一笔 token 都不丢
//!
//! 读数密到中位 8 秒一条,直接拿**相邻**两条配对的话,绝大多数区间会因为
//! `dt < MIN_PAIR_SECS` 被判出界,那一段的代价就再也进不了任何样本——那是**丢样本**,
//! 与「样本只增不删,不靠丢样本降噪」的原则相反（见 calib.rs）。
//!
//! 所以这里按**链式端点**配对：端点之间至少相隔 `MIN_PAIR_SECS`,**跨过去的读数不是被
//! 丢掉,而是成了区间内点**——它们对应的调用照常计入该区间的代价。每一笔 token 恰好落在
//! 一个区间里,每一条读数要么是端点、要么是内点。
//!
//! 这**不是**「把区间拉长来降噪」：下界仍是 `MIN_PAIR_SECS`,没有引入新阈值;
//! 问题从来不是区间太短,而是代价挂错了时刻,这条路上它挂对了。
//!
//! ## 区间口径：`（t0, t1]`,端点那一笔算在区间内
//!
//! 与 bootstrap.rs / `record_pair` 同口径（左开右闭）。这条读数正是端点那笔调用的响应
//! 带回来的,含端点更自洽;改成 `[t0, t1)` 时拟合值随配对粒度的极差明显变大。
//!
//! ## 增量与工作量
//!
//! 水位线 = `max（usage_pair 里 src='rollout' 的 MAX（t1), 上次扫到的文件 mtime, now - 回溯上限)`。
//! **数据下界与文件下界取同一个值**（文件下界再退 `SCAN_LAG_SECS` 容错）——两者必须一致,
//! 否则会拿「读数齐全但调用缺了一半」的区间去建样本,代价系统性偏低。
//! 代价是每次扫描丢掉一个跨扫描边界的区间,按每次扫描进来几百条读数算可以忽略。
//!
//! **哪些文件要读,与水位线是两件事**：水位线管「要哪一段读数」,要不要打开一个文件只看
//! 它**有没有变长**（`META_FILE_SIZES`,跨重启持久）。文件下界那一路只在记录里没有
//! 这个文件时兜底——rollout 的 mtime 在 Windows 上停在创建时刻,拿它判正在写的文件
//! 会漏掉整个当前会话（见 `rollout_files`）。
//!
//! ## 取数时机：rollout 在跑的时候,只在「本地还看不见的扣费」上发请求
//!
//! **服务端按一次模型调用记账,调用结束时一次性扣**;而 `rate_limits` 记的是调用
//! **开始**时的值、调用**结束**才写入 ⇒ 本地读数恒落后**一次调用**。活跃期调用密
//! （中位 18 秒一次,每次 exec 还会拉起 guardian 审核调用）,下一次调用一写就追平,
//! 这时发 API 请求至多早一次调用,不值得。本地追不上的只有两处:
//!
//! - **一轮的最后一次调用**:之后没有下一次调用替它写数,用户一停手本地就停在扣费前;
//!   ⇒ 该轮 `task_complete` 之后若还有未反映的代价（≥ `MIN_PENDING_PCT`）,取一次;
//! - **一次大调用之后长时间没有下一次调用**:未反映的代价 ≥ 取数阈值,且
//!   `LONG_CALL_GRACE_SECS` 内没有新读数追平 ⇒ 取一次。
//!
//! 「未反映」= 结束时刻晚于**最新被反映时刻**的调用:每条读数反映的是它那次调用开始前
//! 已结束的全部调用（含 guardian 子代理）,API 读数反映的是请求时刻之前的全部调用;
//! 两者取最晚。决策见 `plan_fetch`,排程与退避在 demand.rs。rollout 没有 5h 读数
//! （老版 CLI / API key 登录）时这一路不生效,退回按预计消耗取数。
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
/// 没有它,「本机有 rollout 但一条 5h 读数都没有」这种账号（老版 CLI 只回报周窗）
/// 每次启动都会把整个回溯窗重扫一遍——水位线靠 `usage_pair` 推进,而它一条都建不出来。
const META_SCANNED_MTIME: &str = "codex_rollout_scanned_mtime";

/// 上次扫到每个 rollout 文件时它有多大（`meta` 表的键,JSON `{"路径": 字节数}`）。
///
/// 持久化是必需的而不是优化：一个长会话的文件 mtime 停在它**开始**的时刻
/// （见 `rollout_files`）,重启之后要是认不出「它比上次长了」,那道 mtime 下界就会
/// 把整个文件跳过去,那段读数再也进不来。
const META_FILE_SIZES: &str = "codex_rollout_file_sizes";

/// 这一路上次是按哪一版准入判据建的样本（见 `calib:ADMISSION_RULE_VERSION`）。
const META_RULE_VER: &str = "codex_rollout_pair_rule";

/// 回溯上限（天）：**这是工作量上界,不是口径上界**。
///
/// Claude 那边的 14 天来自「源文件只保 14 天」与「越久远越可能跨套餐」;这里两条都不成立
/// ——rollout 通常留得更久,而套餐由每条读数自带、跨套餐的区间本来就会被跳过。
/// 所以这个数唯一的作用是给**首次扫描**的 I/O 封顶：30 天约 500 MB,放到 90 天 I/O 近乎
/// 翻倍,拟合系数只差约 1%。
const HORIZON_DAYS: i64 = 30;

/// 文件下界相对数据下界再退这么多秒：mtime 的粒度、并发写入、时钟回拨都可能让
/// 「文件 mtime 比它里面最后一行还早一点」。退一小时是纯保险,重读几个文件而已。
const SCAN_LAG_SECS: i64 = 3600;

/// 5h / 7d 滚动窗在 `rate_limits` 里的窗长（分钟）。
///
/// **必须按 `window_minutes` 认窗口,不能按 `primary` / `secondary` 的位置认**：
/// 老版 CLI 的记录里 `primary` 装的是**周窗**、`secondary` 为 null,按位置取会把 7d 当成 5h。
const WINDOW_5H_MINUTES: i64 = 300;
const WINDOW_7D_MINUTES: i64 = 10080;

/// 5h 滚动窗口的长度（秒）——算「这段时间从窗尾老掉的代价」要用它回看;
/// 扫描下界因此要比建样本的下界再往前一个窗长（见 `ingest`）。
const WINDOW_SECS: i64 = 5 * 3_600;

/// 一轮结束后,未反映的代价至少这么多（百分点）才值得取一次:显示是整数百分比,
/// 半个点以下取回来多半还是同一个数。
const MIN_PENDING_PCT: f64 = 0.5;

/// 一轮结束（`task_complete`）后等这么久再取:服务端记账与 rollout 落盘之间差一两秒,
/// 太早取可能正好读到扣费前的值。
const TURN_END_DELAY_SECS: i64 = 5;

/// 一次大调用结束后给「下一次调用写数追平」留的时间（秒）:活跃期调用中位 18 秒一次,
/// 90 分位约 100 秒;等一分钟还没追平就取。
const LONG_CALL_GRACE_SECS: i64 = 60;

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
    /// 见到的每个候选文件有多大（读了的是读完时的大小,跳过的原样带回）。
    /// `ingest` 把它整份写回 `meta`,当作下一轮"这个文件长了没有"的比较基准。
    pub seen: BTreeMap<String, u64>,
    /// 有 `rate_limits` 但**不含 5h 窗口**的读数条数（老版 CLI 只回报周窗;
    /// 它们进不了标定——`Pair` 的两端是 5h 已用百分比——但这件事要在日志里看得）。
    pub weekly_only: usize,
    /// 读过的每个文件各自的取数时机线索（键 = 路径）。`scan` 只重读变长的文件,
    /// 所以取数计划要跨轮合并（见 `MARKS`）,不能只看这一轮读到的那几个。
    pub marks: BTreeMap<String, Marks>,
}

/// 一个 rollout 文件里与取数时机有关的线索（口径见模块头「取数时机」）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Marks {
    /// 每条 5h 读数所在那次调用的**开始**时刻（同文件里上一条 `token_count` 或
    /// `task_started`）:这条读数反映的是此刻之前已结束的全部调用。
    pub reflects: Vec<i64>,
    /// 这个文件里的调用（结束时刻 + token）。
    pub calls: Vec<Call>,
    /// 主会话（非子代理）的轮生命周期:`（时刻, 是否为结束)`。
    /// 子代理（guardian 审核等）每次审核都自成一轮,算进来会把每次 exec 都当成「一轮结束」。
    pub turn_marks: Vec<(i64, bool)>,
    /// 最新一条 5h 读数的时刻。
    pub latest_reading: Option<i64>,
}

impl Marks {
    fn absorb(&mut self, other: &Marks) {
        self.reflects.extend_from_slice(&other.reflects);
        self.calls.extend_from_slice(&other.calls);
        self.turn_marks.extend_from_slice(&other.turn_marks);
        self.latest_reading = self.latest_reading.max(other.latest_reading);
    }
}

/// 各文件最近一次读到的线索（跨扫描合并用;进程内,重启后随文件变长逐个补回）。
static MARKS: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, Marks>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));

/// 把读过的文件并进缓存（整份替换:`scan_file` 每次都从头读）,删掉已不存在的文件,
/// 返回全部文件合并后的线索。
fn merge_marks(scan: &Scan) -> Marks {
    let mut cache = MARKS.lock().unwrap_or_else(|e| e.into_inner());
    for (k, m) in &scan.marks {
        cache.insert(k.clone(), m.clone());
    }
    cache.retain(|k, _| scan.seen.contains_key(k));
    let mut all = Marks::default();
    for m in cache.values() {
        all.absorb(m);
    }
    all
}

/// 取数计划（`plan_fetch` 的产出）。
#[derive(Debug, Clone, PartialEq)]
pub struct FetchPlan {
    /// 应取时刻（None = 本地读数追得上,不必发请求）。
    pub due: Option<i64>,
    /// 未反映到任何读数里的预计消耗（百分点）。
    pub pending_pct: f64,
    /// 最新一条 5h 读数的时刻（demand 据此判这一路还「活着」）。
    pub latest_reading: i64,
    /// 日志用:turn-end / long-call / none。
    pub reason: &'static str,
}

/// 取数计划（纯函数,单测直接覆盖;口径见模块头「取数时机」）。
/// `api_at` = 快照里最近一次 **API** 读数的时刻;`scale` = 百分点 / 美元当量;
/// `threshold` = 当前取数阈值（已含低余量收紧）。rollout 没有 5h 读数 → None。
pub fn plan_fetch(marks: &Marks, api_at: Option<i64>, scale: f64, threshold: f64) -> Option<FetchPlan> {
    let latest_reading = marks.latest_reading?;
    let reflected = marks.reflects.iter().copied().max()?.max(api_at.unwrap_or(0));
    let mut pending_usd = 0.0;
    let mut last_call = None::<i64>;
    for c in marks.calls.iter().filter(|c| c.t > reflected) {
        let tokens = cost::Tokens { input: c.tokens[0], output: c.tokens[1], cache_read: c.tokens[2], cache_write: c.tokens[3] };
        pending_usd += cost::cost_of(Platform::Codex, &c.model, &tokens, c.t).0;
        last_call = Some(last_call.map_or(c.t, |x| x.max(c.t)));
    }
    let pending_pct = pending_usd * scale;
    let plan = |due, reason| Some(FetchPlan { due, pending_pct, latest_reading, reason });
    let Some(last_call) = last_call else { return plan(None, "none") };
    // 主会话最后一个生命周期事件是「结束」且不早于最后一次未反映的调用 = 这一轮收工了
    let turn_end = marks.turn_marks.iter().max_by_key(|m| m.0).filter(|m| m.1 && m.0 >= last_call).map(|m| m.0);
    match turn_end {
        Some(end) if pending_pct >= MIN_PENDING_PCT => plan(Some(end + TURN_END_DELAY_SECS), "turn-end"),
        _ if pending_pct >= threshold => plan(Some(last_call + LONG_CALL_GRACE_SECS), "long-call"),
        _ => plan(None, "none"),
    }
}

/// Codex 家目录（`CODEX_HOME` 覆盖;与 `collector:codex` 同一套发现规则）。
fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| crate::collector::home_dir().map(|h| h.join(".codex")))
}

/// 全部候选 rollout 文件及其**世代** `（mtime 秒, 字节数)`。
///
/// **mtime 单独用不得**：Windows 上 Codex 追加写 rollout 时 mtime 停在创建时刻,
/// 会话结束、进程退出之后也不补。只看 mtime 的闸门会在**正在用的那个会话**上彻底
/// 瞎掉,而那正是最需要实时读数的时候。
///
/// 字节数是准的（同一次 `stat` 里就能拿到,不多一次系统调用）,而 rollout 是**追加
/// 写**的 ⇒ **字节数变了就是有新行,没变就没有**。这与 `collector:jsonl:generation`
/// 的 `（size, mtime)` 是同一套判据。
fn rollout_files() -> Vec<(PathBuf, i64, u64)> {
    let Some(home) = codex_home() else { return vec![] };
    let mut paths = vec![];
    crate::collector::jsonl::discover(&home.join("sessions"), true, &mut paths);
    crate::collector::jsonl::discover(&home.join("archived_sessions"), true, &mut paths);
    paths
        .into_iter()
        .filter_map(|p| {
            let md = std::fs::metadata(&p).ok()?;
            let m = md.modified().ok()?;
            let secs = m.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
            Some((p, secs, md.len()))
        })
        .collect()
}

/// 收割前的廉价闸门（只 stat 不读内容）：`（最新 mtime, 全部候选文件的总字节数)`。
/// 无文件 → None。**总字节数是主信号**（理由见 `rollout_files`）;mtime 一并带上
/// 只为兜住「文件被换成同样大小的另一份」这种极端情形。
pub fn generation() -> Option<(i64, u64)> {
    let files = rollout_files();
    if files.is_empty() {
        return None;
    }
    let newest = files.iter().map(|(_, m, _)| *m).max().unwrap_or(0);
    let bytes = files.iter().map(|(_, _, n)| *n).sum();
    Some((newest, bytes))
}

/// 候选文件里最新的 mtime（unix 秒;无文件 → None）——只给水位线
/// （`META_SCANNED_MTIME`）用,**不当闸门**。
pub fn newest_mtime() -> Option<i64> {
    rollout_files().into_iter().map(|(_, m, _)| m).max()
}

/// 扫描有新内容的文件,取出 `t >= since` 的读数与调用。
///
/// `seen` = 上次扫到每个文件时它有多大（`ingest` 从 `meta` 取、扫完写回;
/// 跨重启持久,所以一个长会话在重启之后照样认得出「它又长了」）。
/// **跳过一个文件的唯一理由是它没长**；`mtime_floor` 只在**记录里没有这个文件**
/// （首扫 / 新装 / 换了 `CODEX_HOME`）时兜底,它的职责仅仅是给首扫的 I/O 封顶,
/// 判不了一个正在被追加的文件（mtime 不动,见 `rollout_files`）。
///
/// （`pub（super)` 是给 `smoke` 的直接测量用的——它要的是原始读数与调用,不是样本。）
pub(super) fn scan(since: i64, mtime_floor: i64, seen: &BTreeMap<String, u64>) -> Scan {
    let files = rollout_files();
    let mut out = Scan { files_total: files.len(), ..Default::default() };
    for (path, mtime, size) in files {
        let key = path.to_string_lossy().to_string();
        // 跳过的也要把当前大小记进 `seen`：它是下一轮的比较基准,也让被删掉的文件
        // 自然从记录里掉出去（只登记这一轮真实存在的文件）。
        if !needs_read(seen.get(&key).copied(), size, mtime, mtime_floor) {
            out.seen.insert(key, size);
            continue;
        }
        out.files_read += 1;
        out.bytes_read += size;
        let (r0, c0) = (out.readings.len(), out.calls.len());
        let mut marks = Marks::default();
        scan_file(&path, since, &mut out, &mut marks);
        marks.calls = out.calls[c0..].to_vec();
        marks.latest_reading = out.readings[r0..].iter().map(|r| r.t).max();
        out.marks.insert(key.clone(), marks);
        out.seen.insert(key, size);
    }
    out.readings.sort_by_key(|r| r.t);
    out.calls.sort_by_key(|c| c.t);
    out
}

/// 这个文件要不要打开。`known` = 上次扫到它时它有多大（`None` = 没见过）。
///
/// 抽成纯函数是因为它是这条路**唯一的漏读风险点**：判错一次,一个正在写的会话
/// 就整段进不来,而那段读数在源文件被清掉之后再也拿不回来。
fn needs_read(known: Option<u64>, size: u64, mtime: i64, mtime_floor: i64) -> bool {
    match known {
        // 追加写 ⇒ 字节数没变就没有新行;变了（含被截断重写,`size` 变小）就得读。
        // **不看 mtime**——它在 Windows 上停在文件创建时刻。
        Some(prev) => prev != size,
        // 没见过：只剩 mtime 这一个线索,拿它给首扫的 I/O 封顶。
        None => mtime >= mtime_floor,
    }
}

/// 扫一个 rollout 文件（行式流读;先做子串预筛再解析 JSON）。
///
/// 预筛不是微优化：rollout 的字节数绝大部分是 `session_meta` 的 base_instructions 与
/// 消息正文,单个文件可达几十 MB,逐行 serde 解析它们纯属浪费。
fn scan_file(path: &Path, since: i64, out: &mut Scan, marks: &mut Marks) {
    let Ok(file) = std::fs::File::open(path) else { return };
    let mut reader = std::io::BufReader::new(file);
    // 模型在 `turn_context` 行的 `payload.model`（轮级设置,token_count 行不带)——
    // 与 collector:codex 同一判据。逐行推进,token_count 取当时最近的那个值。
    let mut model = String::new();
    // 调用边界:上一条 token_count / task_started 的时刻（下一次调用从这里开始）
    let mut boundary: Option<i64> = None;
    // 子代理会话（guardian 审核等）:`session_meta.payload.source.subagent` 存在
    let mut subagent = false;
    let mut buf: Vec<u8> = vec![];
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // 按字节读 + lossy：`read_line` 遇到一个非法 UTF-8 字节就返回错误,那会让
        // **整个文件的剩余部分**被静默丢掉。写到一半的尾行解析不出 JSON 被跳过,
        // 文件字节数还在变,下一轮自然重读。
        let line = String::from_utf8_lossy(&buf);
        let is_ctx = line.contains("\"turn_context\"");
        let is_tok = line.contains("\"token_count\"");
        let is_meta = line.contains("\"session_meta\"") && line.contains("\"subagent\"");
        let is_turn = line.contains("\"task_started\"") || line.contains("\"task_complete\"") || line.contains("\"turn_aborted\"");
        if !is_ctx && !is_tok && !is_meta && !is_turn {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(payload) = v.get("payload") else { continue };
        if v.get("type").and_then(|x| x.as_str()) == Some("session_meta") {
            subagent = payload.get("source").and_then(|s| s.get("subagent")).is_some();
            continue;
        }
        let kind = payload.get("type").and_then(|x| x.as_str());
        if matches!(kind, Some("task_started" | "task_complete" | "turn_aborted")) {
            let Some(t) = payload_time(&v) else { continue };
            let end = kind != Some("task_started");
            if !end {
                boundary = Some(t);
            }
            if !subagent && t >= since {
                marks.turn_marks.push((t, end));
            }
            continue;
        }
        // 精确键名：token_count 行有 `model_context_window` 而没有 `model`,不会误命中
        if let Some(m) = payload.get("model").and_then(|x| x.as_str()) {
            model = m.to_string();
        }
        if payload.get("type").and_then(|x| x.as_str()) != Some("token_count") {
            continue;
        }
        let Some(t) = payload_time(&v) else { continue };
        let started = boundary.replace(t).unwrap_or(t);
        if t < since {
            continue;
        }
        if let Some(call) = parse_call(payload, t, &model) {
            out.calls.push(call);
        }
        match parse_reading(payload, t) {
            Some(r) => {
                out.readings.push(r);
                marks.reflects.push(started);
            }
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
/// `Σ last_token_usage.total` 与文件末条 `total_token_usage.total` 逐位相同
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
    account_since: Option<i64>,
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
        // 套餐时这条样本无从归属（同一台机器上 plan_type 可能在多个账号间来回跳）。
        if a.plan != b.plan || breakdown.is_empty() {
            continue;
        }
        // **跨账号切换点的区间也不建样本**。每个账号有自己独立的额度窗口
        // ⇒ 跨在切换点上的 Δ 是拿两个账号的读数相减出来的,毫无意义。`plan_type` 挡不住
        // 这件事:可能有两个同套餐的账号交替使用。边界只对「从现在往后」有效（rollout 里
        // 一个账号字段都没有,历史补不回来）,更早的区间仍靠套餐与窗尾判据兜着。
        if account_since.is_some_and(|t| a.t < t && t <= b.t) {
            continue;
        }
        // at = t1：区间右端点,与 store:recompute_stale_costs / bootstrap 同口径
        let (total, unknown) = cost::cost_of_breakdown(Platform::Codex, &breakdown, b.t);
        // **老掉的**：发生在 （t0−5h, t1−5h] 的那些调用——5h 是滚动窗口,计数器的变化
        // 是「新花的 − 老掉的」。调用按时刻有序,二分定位。
        let aged = {
            let (lo, hi) = (a.t - WINDOW_SECS, b.t - WINDOW_SECS);
            let i = calls.partition_point(|c| c.t <= lo);
            let j = calls.partition_point(|c| c.t <= hi);
            let mut old_b: BTreeMap<String, [i64; 4]> = BTreeMap::new();
            for c in &calls[i..j] {
                let slot = old_b.entry(c.model.clone()).or_insert([0; 4]);
                for (k, v) in c.tokens.iter().enumerate() {
                    slot[k] += v;
                }
            }
            cost::cost_of_breakdown(Platform::Codex, &old_b, hi).0
        };
        let pair = Pair {
            t0: a.t,
            t1: b.t,
            used5_0: a.used5,
            used5_1: b.used5,
            // rollout 的每条读数都带窗尾 ⇒ 跨重置这件事可以**直接判**,不必靠
            // 「读数变小了」推断（语义见 calib:Pair:window_changed）。
            resets5_0: a.resets5,
            resets5_1: b.resets5,
            cost: total,
            unknown_cost: unknown,
            aged_cost: aged,
        };
        if !pair.usable(calib::scale(Platform::Codex)) {
            continue; // 跨重置 / 间隔越界 / 零代价 / 比值过高等,与另外两路同一套筛选
        }
        out.push((pair, (a.used7, b.used7), breakdown, b.plan.clone()));
    }
    out
}

/// 窗尾在同一个窗口内逐条会抖几秒（服务端按请求时刻换算）;换号 / 窗口到期重开时差的是小时。
const WINDOW_JITTER_SECS: i64 = 120;

/// 两条读数是否属于同一个配额窗口：套餐相同,且两端窗尾都已知、相差不超过抖动容差。
/// 任一端没有窗尾 = 判不了 ⇒ 按「不同窗」处理（照搬新读数,即改动前的行为）。
fn same_window(a: Option<i64>, b: Option<i64>, plan_a: &str, plan_b: &str) -> bool {
    plan_a == plan_b && matches!((a, b), (Some(a), Some(b)) if (a - b).abs() <= WINDOW_JITTER_SECS)
}

/// 用最新的 rollout 读数推进 Codex 快照（**零网络**;返回是否真的改了）。
///
/// 这一步的价值不在省请求,在**时效**：`rate_limits` 是 Codex 在每次响应里带回来的
/// **同一个服务端数字**,只是走本地文件到手。用贵模型时一个轮次就能吃掉 5h 窗的
/// 十几二十个百分点,而取数是按预计消耗触发的——触发判据本身要等采集器先看见那些
/// token。读数在文件里已经是真值了,没有理由还让球上显示上一次取数的旧数。
///
/// 三条约束：
/// - **必须比上次读数新**（`t > prev.fetched_at`）。旧样本不能冒充进展,否则
///   `advanced` 会误判、账目被错误清零。
/// - **换窗 / 换号时上下都改**。窗尾变了（窗口到期重开、换账号）或源没给窗尾,
///   这是一条**完整的、更新的**读数,两个方向都照搬。
/// - **同一窗口内只升不降**。`rate_limits` 是这次模型调用**开始**时服务端回的数,却在
///   调用**结束**时才写进文件:主会话一次长调用（几十秒到几分钟）写下的数,可能比期间
///   guardian 审核子代理（每次 exec 工具调用自动拉起,`codex-auto-review`）刚写的更旧。
///   按写入先后取「最后一条」会让球上的数倒退。
///   服务端同一窗口的用量只增不减（同窗相邻 API 读数 201 对,下降 0 次）⇒ 同窗取最大。
fn update_snapshot(store: &SubStore, readings: &[Reading], now: i64) -> bool {
    let prev = store.load_snapshot(Platform::Codex);
    let t0 = prev.as_ref().and_then(|p| p.fetched_at);
    // 比手上新的读数;一条都没有 = 什么都证明不了
    let fresh: Vec<&Reading> = readings.iter().filter(|r| t0.is_none_or(|t0| r.t > t0)).collect();
    let Some(r) = fresh.last().copied() else { return false };
    let prev_plan = prev.as_ref().map(|p| p.plan_type.as_str()).unwrap_or("");
    let plan_of = |x: &Reading| if x.plan.is_empty() { prev_plan.to_string() } else { x.plan.clone() };
    let plan = plan_of(r);
    // 一个窗口的取值:本批里与最新一条同窗的读数取最大,再与快照里同窗的值取最大
    let settle = |kind: &str, used: fn(&Reading) -> f64, resets: fn(&Reading) -> Option<i64>| -> f64 {
        let mut v = used(r);
        for x in &fresh {
            if same_window(resets(x), resets(r), &plan_of(x), &plan) {
                v = v.max(used(x));
            }
        }
        if let Some(w) = prev.as_ref().and_then(|p| p.windows.iter().find(|w| w.kind == kind)) {
            if same_window(w.resets_at, resets(r), prev_plan, &plan) {
                v = v.max(w.used_percent);
            }
        }
        v
    };
    let used5 = settle("5h", |x| x.used5, |x| x.resets5);
    let used7 = settle("7d", |x| x.used7, |x| x.resets7);
    // 同窗取大之后与快照一模一样 = 这批读数只是迟到的旧数,不是进展:不写、不清账目
    // （清了会把「距上次读数」的消耗记漏,取数被推迟）。
    if let Some(p) = prev.as_ref() {
        let same = |kind: &str, v: f64| p.windows.iter().any(|w| w.kind == kind && w.used_percent == v);
        if p.status == FetchStatus::Ok && p.plan_type == plan && same("5h", used5) && same("7d", used7) {
            let resets_same = p.windows.iter().any(|w| w.kind == "5h" && same_window(w.resets_at, r.resets5, &p.plan_type, &plan));
            if resets_same {
                return false;
            }
        }
    }
    let snap = SubscriptionSnapshot {
        platform: Platform::Codex,
        // 读数自带套餐;缺省时沿用上一条快照的,别把已知的 plan 退化成 unknown
        plan_type: if plan.is_empty() { "unknown".into() } else { plan.clone() },
        windows: vec![
            QuotaWindow { kind: "5h".into(), used_percent: used5, resets_at: r.resets5 },
            QuotaWindow { kind: "7d".into(), used_percent: used7, resets_at: r.resets7 },
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
        used5,
        used7,
        snap.plan_type,
        now - r.t,
        dropped.cost * calib::scale(Platform::Codex)
    );
    true
}

/// 扫描的比较基准 `seen` = 上一轮各文件的大小（坏 JSON / 没有这个键 ⇒ 空表,
/// 退化成「按文件下界首扫一遍」）。判据升版时必须返回空表：重建要把回溯上限内的文件
/// 全部重读,而那些文件多半一个字节都没长,按大小比较会全部跳过、重建一行都建不出来。
fn scan_baseline(rule_stale: bool, stored: Option<String>) -> BTreeMap<String, u64> {
    if rule_stale {
        return BTreeMap::new();
    }
    stored.and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default()
}

/// 收割 + 增量标定 + 快照推进（主轮询在 rollout 世代变化时调用;**零网络、零凭据**）。
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
    // 扫描下界再往前推一个窗长（见下方 `scan` 调用）：老化量的原料在 `（t0−5h, …]`,少扫
    // 这一段会把最早那批区间的老化量算成 0。读数仍按 `since` 切（build_pairs 之前过滤）,只多要调用。
    let seen = scan_baseline(rule_stale, store.meta_str(META_FILE_SIZES));
    let scan = scan(since - WINDOW_SECS, since - SCAN_LAG_SECS - WINDOW_SECS, &seen);
    if scan.files_read == 0 {
        return (0, 0, false);
    }

    // 读数回到原来的下界：多扫出来的那一个窗长只是老化量的原料,不该让它把水位线
    // 之前的区间重新建一遍（收割本身是幂等的,多收几条只会让历史更全,照收）。
    let readings: Vec<Reading> =
        scan.readings.iter().filter(|r| r.t >= since).cloned().collect();
    let samples: Vec<_> =
        scan.readings.iter().map(|r| (r.t, r.used5, r.used7, r.plan.clone())).collect();
    let harvested = store.insert_samples_of(Platform::Codex, &samples).unwrap_or(0);
    // 同一批读数再落一遍**归一化序列**（两层并存,见 store 建表注释）。
    // 这一路比桌面端全：rollout 的每条 rate_limits 都自带窗尾与套餐,两个窗口各一行。
    let quota_rows: Vec<_> = scan
        .readings
        .iter()
        .flat_map(|r| {
            [("5h", r.used5, r.resets5), ("7d", r.used7, r.resets7)].map(|(kind, used, resets)| {
                super::model::QuotaReading {
                    t: r.t,
                    kind: kind.into(),
                    used_percent: used,
                    resets_at: resets,
                    plan_type: r.plan.clone(),
                    source: super::model::SnapshotSource::Rollout,
                }
            })
        })
        .collect();
    if let Err(e) = store.insert_readings(Platform::Codex, &quota_rows) {
        crate::dev_log!("[subscription] codex quota_reading insert failed: {e}");
    }

    // 样本按套餐分组落库：insert_pairs 一次只带一个 plan_type,而回溯窗里可能跨过套餐变更。
    let mut by_plan: BTreeMap<String, Vec<(Pair, (f64, f64), String)>> = BTreeMap::new();
    let account_since = store.account_since(Platform::Codex);
    for (pair, used7, breakdown, plan) in
        build_pairs(&readings, &scan.calls, account_since)
    {
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
    // 同理写在建样本之后。真变了才写,没动的表不必反复打 WAL。
    if scan.seen != seen {
        if let Ok(json) = serde_json::to_string(&scan.seen) {
            let _ = store.set_meta_str(META_FILE_SIZES, &json);
        }
    }

    if harvested > 0 || pairs > 0 || scan.weekly_only > 0 {
        log_scan(store, &scan, harvested, pairs);
    }
    // 快照推进排在最后：它只依赖读数,与建样本互不影响,放这儿保证即使建样本
    // 一条都没建出来（比如全是 Δ=0 的区间）,球上的数照样是最新的。
    let snapshot_changed = update_snapshot(store, &scan.readings, now);

    // 取数计划:本地追不上的扣费才排请求（见模块头「取数时机」）。排在快照推进之后:
    // 阈值的低余量收紧要看刚推进的 5h 余量。
    let snap = store.load_snapshot(Platform::Codex);
    // API 读数反映请求之前结束的全部调用:取快照（若还是 API 来源）与 demand 记下的最近一次,较晚者
    let api_at = snap
        .as_ref()
        .filter(|s| s.source == SnapshotSource::Api)
        .and_then(|s| s.fetched_at)
        .unwrap_or(0)
        .max(super::demand::last_api_reading(Platform::Codex));
    let api_at = (api_at > 0).then_some(api_at);
    let remaining = snap.as_ref().and_then(|s| s.windows.iter().find(|w| w.kind == "5h")).map(|w| 100.0 - w.used_percent);
    let threshold = super::demand::effective_threshold(remaining);
    if let Some(plan) = plan_fetch(&merge_marks(&scan), api_at, calib::scale(Platform::Codex), threshold) {
        super::demand::set_rollout_plan(Platform::Codex, plan.due, plan.latest_reading);
        crate::dev_log!(
            "[subscription] codex rollout plan: pending {:.2}% → {} ({})",
            plan.pending_pct,
            plan.due.map_or("no fetch".to_string(), |d| format!("fetch in {}s", (d - now).max(0))),
            plan.reason
        );
    }
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
         → kept {} over {:.1}d (median gap {}s) → +{} pair(s), quota_reading {} row(s)",
        harvested,
        scan.files_read,
        scan.files_total,
        scan.bytes_read as f64 / 1_048_576.0,
        total,
        span_days,
        gap,
        pairs,
        store.quota_reading_count(Platform::Codex)
    );
    if scan.weekly_only > 0 {
        // 老版 CLI 只回报周窗 ⇒ 这些读数进不了标定。
        // 不当错误,但要让「它们存在且被跳过」这件事在日志里看得。
        crate::dev_log!(
            "[subscription] codex rollout: {} reading(s) carried no 5h window (weekly-only, skipped)",
            scan.weekly_only
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 判据升版：回溯上限内没长过的文件也必须重读,否则重建一行样本都建不出来。
    #[test]
    fn stale_rule_rescans_unchanged_files() {
        let stored = Some(r#"{"a.jsonl":4096}"#.to_string());
        let fresh = scan_baseline(false, stored.clone());
        assert!(!needs_read(fresh.get("a.jsonl").copied(), 4_096, 300, 200), "判据当前:没长就跳过");
        let stale = scan_baseline(true, stored);
        assert!(stale.is_empty());
        assert!(needs_read(stale.get("a.jsonl").copied(), 4_096, 300, 200), "判据升版:下界内的文件照读");
        assert!(scan_baseline(false, Some("not json".into())).is_empty(), "坏 JSON 退化成空表");
    }

    /// 追加写的文件：字节数没变 = 没有新行,不必打开。
    #[test]
    fn unchanged_file_is_skipped() {
        assert!(!needs_read(Some(100), 100, 0, i64::MIN));
    }

    /// Windows 上正在被 Codex 追加的 rollout：mtime 冻在会话开始的时刻
    /// （远早于文件下界）,但它一直在长。只看 mtime 的判据会让整个当前会话
    /// 一条读数都进不来——而那正是最需要实时读数的时候。
    #[test]
    fn growing_file_with_frozen_old_mtime_is_read() {
        let session_start = 1_000;
        let floor = session_start + 7_200; // 会话已经跑了两小时,早过了文件下界
        assert!(needs_read(Some(100), 4_096, session_start, floor));
    }

    /// 被截断 / 重写（字节数变小）也要重读——幂等,重复的读数会被主键挡掉。
    #[test]
    fn truncated_file_is_read() {
        assert!(needs_read(Some(4_096), 100, 0, i64::MIN));
    }

    /// 没见过的文件只剩 mtime 一个线索：它就是首扫的 I/O 闸,该拦的还得拦。
    #[test]
    fn unknown_file_falls_back_to_mtime_floor() {
        assert!(!needs_read(None, 4_096, 100, 200));
        assert!(needs_read(None, 4_096, 300, 200));
    }

    fn reading(t: i64, used5: f64, plan: &str) -> Reading {
        Reading { t, used5, used7: 10.0, plan: plan.into(), resets5: None, resets7: None }
    }

    /// 真实量级的一笔调用：Codex 每 1% 配额约合 $0.12 等价用量（scale≈8 %/美元）,
    /// 太小的 token 量会让隐含比值落到 calib 的可信带之外,样本会被当成账目错配丢掉。
    fn call(t: i64, model: &str, input: i64) -> Call {
        Call { t, model: model.into(), tokens: [input, 0, 0, 0] }
    }

    fn line(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn window_is_picked_by_length_not_by_slot() {
        // 老版 CLI 把**周窗**放在 primary、secondary 为 null
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
        scan_file(&path, 0, &mut out, &mut Marks::default());
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

    /// 同一窗口内快照只升不降:主会话长调用迟写的旧数（调用开始时的服务端值）
    /// 不能把 guardian 子代理刚写的新数压回去;换窗 / 换号照常回落。
    #[test]
    fn snapshot_never_regresses_within_one_window() {
        let dir = std::env::temp_dir().join(format!("tc_cxroll_mono_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = SubStore::open(&dir.join("subscriptions.db")).unwrap();
        let rd = |t: i64, u5: f64, r5: i64, plan: &str| Reading {
            t,
            used5: u5,
            used7: 36.0,
            plan: plan.into(),
            resets5: Some(r5),
            resets7: Some(500_000),
        };
        let used5 = |s: &SubStore| s.load_snapshot(Platform::Codex).unwrap().windows[0].used_percent;

        // ① 同一批里:guardian 先写 95,主会话后写迟到的 91 → 取 95（窗尾抖几秒仍算同窗）
        assert!(update_snapshot(&store, &[rd(1_000, 95.0, 20_000, "plus"), rd(1_001, 91.0, 20_003, "plus")], 1_010));
        assert_eq!(used5(&store), 95.0);
        let snap = store.load_snapshot(Platform::Codex).unwrap();
        assert_eq!(snap.fetched_at, Some(1_001), "时刻仍取最新写入那条");

        // ② 下一批只有迟到的更低读数 → 不是进展:不写、不广播
        assert!(!update_snapshot(&store, &[rd(1_050, 93.0, 19_998, "plus")], 1_060));
        assert_eq!(used5(&store), 95.0);
        assert_eq!(store.load_snapshot(Platform::Codex).unwrap().fetched_at, Some(1_001), "不推进时刻 = 不清账目");

        // ③ 同窗更高的照常上修
        assert!(update_snapshot(&store, &[rd(1_100, 97.0, 20_001, "plus")], 1_110));
        assert_eq!(used5(&store), 97.0);

        // ④ 换号（套餐不同,窗尾差几小时）→ 从 0 起,允许回落
        assert!(update_snapshot(&store, &[rd(1_200, 0.0, 38_000, "edu")], 1_210));
        assert_eq!(used5(&store), 0.0);

        // ⑤ 同套餐但窗尾差得远（窗口到期重开 / 同档第二个账号）→ 同样允许回落
        assert!(update_snapshot(&store, &[rd(1_300, 40.0, 38_010, "edu")], 1_310));
        assert!(update_snapshot(&store, &[rd(1_400, 2.0, 60_000, "edu")], 1_410));
        assert_eq!(used5(&store), 2.0);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 取数计划:本地读数追得上就不发请求;一轮收工 / 大调用后久无下一次调用才排。
    /// scale 取极大 / 极小值,让「有没有未反映代价」与具体价目解耦。
    #[test]
    fn fetch_plan_only_covers_charges_local_readings_cannot_see() {
        const BIG: f64 = 1e9; // 任何非零代价都远超阈值
        const TINY: f64 = 1e-12; // 任何代价都不到半个点
        let marks = |reflects: Vec<i64>, calls: Vec<Call>, turn: Vec<(i64, bool)>| Marks {
            reflects,
            calls,
            turn_marks: turn,
            latest_reading: Some(1_000),
        };
        let c = |t| call(t, "gpt-5", 1_000);

        // ① 一轮收工:最后一次调用（t=1000,它的读数反映的是 990 之前）→ 收工 5 秒后取
        let m = marks(vec![990], vec![c(980), c(1_000)], vec![(900, false), (1_001, true)]);
        let p = plan_fetch(&m, None, BIG, 5.0).unwrap();
        assert_eq!((p.due, p.reason), (Some(1_006), "turn-end"));
        assert!(p.pending_pct > 0.0);

        // ② guardian 子代理的读数在主会话调用结束之后开始 ⇒ 已反映,不必取
        let m = marks(vec![990, 1_002], vec![c(1_000)], vec![(900, false), (1_001, true)]);
        assert_eq!(plan_fetch(&m, None, BIG, 5.0).unwrap().due, None);

        // ③ API 读数晚于最后一次调用 ⇒ 已反映
        let m = marks(vec![990], vec![c(1_000)], vec![(1_001, true)]);
        assert_eq!(plan_fetch(&m, Some(1_003), BIG, 5.0).unwrap().due, None);

        // ④ 轮还没收工、未反映代价 ≥ 阈值 ⇒ 给下一次调用 60 秒追平,追不上再取
        let m = marks(vec![990], vec![c(1_000)], vec![(900, false)]);
        assert_eq!(plan_fetch(&m, None, BIG, 5.0).unwrap(), FetchPlan { due: Some(1_060), pending_pct: plan_fetch(&m, None, BIG, 5.0).unwrap().pending_pct, latest_reading: 1_000, reason: "long-call" });

        // ⑤ 轮没收工、代价不到阈值 ⇒ 等下一次调用写数,不取
        assert_eq!(plan_fetch(&m, None, TINY, 5.0).unwrap().due, None);

        // ⑥ 收工了但代价不到半个点 ⇒ 取回来多半还是同一个整数,不取
        let m = marks(vec![990], vec![c(1_000)], vec![(1_001, true)]);
        assert_eq!(plan_fetch(&m, None, TINY, 5.0).unwrap().due, None);

        // ⑦ 新一轮已开始（最后一个生命周期事件是开始）⇒ 不按收工处理
        let m = marks(vec![990], vec![c(1_000)], vec![(1_001, true), (1_010, false)]);
        assert_eq!(plan_fetch(&m, None, BIG, 5.0).unwrap().reason, "long-call");

        // ⑧ 没有 5h 读数（老版 CLI / API key）⇒ 这一路不生效,交还预计消耗
        assert!(plan_fetch(&Marks::default(), None, BIG, 5.0).is_none());
    }

    /// 扫描时的线索:读数所在调用的开始 = 同文件上一条 token_count / task_started;
    /// 子代理文件的轮事件不进 turn_marks（每次 exec 审核都会自成一轮）。
    #[test]
    fn scan_marks_call_starts_and_skips_subagent_turns() {
        let dir = std::env::temp_dir().join(format!("tc_cxroll_marks_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rl = r#""rate_limits":{"plan_type":"plus","primary":{"used_percent":5.0,"window_minutes":300,"resets_at":99999},"secondary":null}"#;
        let tc = |ts: &str| format!(r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":10,"output_tokens":5}}}},{rl}}}}}"#);
        let ev = |ts: &str, kind: &str| format!(r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"{kind}"}}}}"#);
        let main = [
            ev("2026-09-22T21:00:00Z", "task_started"),
            tc("2026-09-22T21:00:20Z"),
            tc("2026-09-22T21:01:00Z"),
            ev("2026-09-22T21:01:01Z", "task_complete"),
        ]
        .join("
");
        let main_path = dir.join("main.jsonl");
        std::fs::write(&main_path, main + "
").unwrap();
        let sub = [
            r#"{"timestamp":"2026-09-22T21:00:30Z","type":"session_meta","payload":{"source":{"subagent":{"other":"guardian"}}}}"#.to_string(),
            ev("2026-09-22T21:00:30Z", "task_started"),
            tc("2026-09-22T21:00:33Z"),
            ev("2026-09-22T21:00:33Z", "task_complete"),
        ]
        .join("
");
        let sub_path = dir.join("sub.jsonl");
        std::fs::write(&sub_path, sub + "
").unwrap();

        let t = |s: &str| crate::collector::rfc3339_to_millis(s).unwrap() / 1000;
        let mut out = Scan::default();
        let mut m = Marks::default();
        scan_file(&main_path, 0, &mut out, &mut m);
        assert_eq!(m.reflects, vec![t("2026-09-22T21:00:00Z"), t("2026-09-22T21:00:20Z")], "开始 = task_started / 上一条 token_count");
        assert_eq!(m.turn_marks, vec![(t("2026-09-22T21:00:00Z"), false), (t("2026-09-22T21:01:01Z"), true)]);
        let mut s = Marks::default();
        scan_file(&sub_path, 0, &mut out, &mut s);
        assert_eq!(s.reflects, vec![t("2026-09-22T21:00:30Z")], "子代理读数照样算反映时刻");
        assert!(s.turn_marks.is_empty(), "子代理的轮不算一轮收工");
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
        scan_file(&path, 0, &mut out, &mut Marks::default());
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
        let out = build_pairs(&readings, &calls, None);
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
        let out = build_pairs(&readings, &calls, None);
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
        let out = build_pairs(&readings, &calls, None);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].0.t0, out[0].0.t1), (1_060, 1_120));
        assert_eq!(out[0].3, "edu");
    }

    /// **跨账号切换点的区间不建样本**：每个账号有自己独立的额度窗口,
    /// 跨在切换点上的 Δ 是拿两个账号的读数相减出来的。边界之外的区间照常建。
    #[test]
    fn intervals_straddling_an_account_switch_are_dropped() {
        let readings =
            vec![reading(1_000, 5.0, "plus"), reading(1_060, 8.0, "plus"), reading(1_120, 11.0, "plus")];
        let calls = vec![call(1_030, "gpt-5.6-sol", 20_000), call(1_090, "gpt-5.6-sol", 20_000)];
        assert_eq!(build_pairs(&readings, &calls, None).len(), 2, "没有边界 → 两条都建");
        // 边界落在第一个区间内部 ⇒ 只剩第二条
        let out = build_pairs(&readings, &calls, Some(1_030));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.t0, 1_060);
        // 边界正好落在端点上：(t0, t1] 左开右闭 ⇒ t == t0 不算跨,t == t1 算
        assert_eq!(build_pairs(&readings, &calls, Some(1_000)).len(), 2, "边界=区间起点,不算跨");
        assert_eq!(build_pairs(&readings, &calls, Some(1_060)).len(), 1, "边界=区间终点,算跨");
        // 边界比所有读数都晚（刚记下账号）⇒ 一条都不跨
        assert_eq!(build_pairs(&readings, &calls, Some(9_999)).len(), 2);
    }

    /// 没涨的读数**收**,只要这段消耗本来就不该动一格。
    #[test]
    fn a_flat_reading_below_one_step_still_makes_a_sample() {
        let flat = vec![reading(1_000, 5.0, "edu"), reading(1_060, 5.0, "edu")];
        // 按 Codex 的出厂预设,这一笔的预计涨幅约 0.7 个百分点 ⇒ 不够动一格是正常的
        // （配对用的那笔 60k 折合约 2.1 个百分点,已经越过 ZERO_DELTA_SLACK 的上限）
        let small = vec![call(1_030, "gpt-5.6-sol", 20_000)];
        let out = build_pairs(&flat, &small, None);
        assert_eq!(out.len(), 1, "Δ=0 的有效样本计回分母");
        assert_eq!(out[0].0.used5_1 - out[0].0.used5_0, 0.0);
        // 预计要涨好几个百分点却纹丝不动 ⇒ 账目不对,照丢
        let huge = vec![call(1_030, "gpt-5.6-sol", 60_000_000)];
        assert!(build_pairs(&flat, &huge, None).is_empty());
        // 没有调用 = 别处在用,不能拿来标定本地换算
        let no_call = vec![reading(1_000, 5.0, "edu"), reading(1_060, 9.0, "edu")];
        assert!(build_pairs(&no_call, &[], None).is_empty());
    }

    #[test]
    fn auto_review_heavy_intervals_are_kept_out_of_calibration() {
        // codex-auto-review 是路由标签不是模型名 ⇒ 折价但标 unknown;过半即不参与标定
        let readings = vec![reading(1_000, 0.0, "edu"), reading(1_060, 2.0, "edu")];
        let calls = vec![call(1_030, "codex-auto-review", 200_000)];
        assert!(build_pairs(&readings, &calls, None).is_empty(), "未知模型占比过半 ⇒ 不建样本");
    }
}
