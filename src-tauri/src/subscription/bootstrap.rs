//! 桌面端采样收割 + 标定。
//!
//! 两份数据都在本机、都不花任何 API 请求：
//! - Claude 桌面端 `plan-usage-history.json`：约 15 分钟一条的用量百分比历史（服务端口径,
//!   在线 / 网页用量也在内）;
//! - collector.db：同期每轮的模型与 token。
//!
//! 把相邻两条桌面端样本当作一次「读数对」,区间内**按轮的 `ended_at` 精确切**出本地
//! token 折成代价,就得到一条与在线路径同构、但两个时间窗**同源**的标定样本（见 calib.rs）。
//!
//! 为什么这一路比在线路径更适合桌面端：
//! - **两个窗口同源**。在线路径的读数对区间是 `[fetched_at0, fetched_at1]`,而代价是
//!   demand 按 token **到达时刻**从上一次成功轮累加来的。API 路径上两者差不到一秒,
//!   **桌面端回落路径**的 `fetched_at` 却是样本时刻,可能比轮时刻早十几分钟 ⇒ 「这段
//!   时间花了多少」与「这段时间涨了多少」对应的不是同一段时间。这里按样本时刻切,
//!   偏移归零。
//! - **密度更高**。桌面端 15 分钟一条,比兜底轮（默认 30 分钟）密一倍;而且区间越短,
//!   5h 滚动窗口里「过期掉的旧用量」吃掉新增的比例越小,系统性低估也越轻（calib.rs）。
//! - **样本只增不减**。桌面端自己的历史文件只滚动保留约 14 天,收割进
//!   `desktop_sample` 后**永久留着**（store.rs）——观测密度随时间累积,源文件裁剪掉的
//!   也不会跟着丢。
//!
//! 因此在线路径只保留**两端都是 API 读数**的样本（`mod.rs` 的 `record_pair`）,
//! 桌面端那一路一律由本模块按样本时刻建。
//!
//! 增量续算：水位线 = `usage_pair` 里 `src='desktop'` 的 `MAX（t1)`,每轮只处理水位线
//! 之后的相邻样本对。区间里还没有本地轮记录（采集比桌面端采样慢半拍）时这一对会被
//! 跳过,水位线不前移,下一轮自然重试。
//!
//! 缓存 token 的处理：`turn_raw` 只有输入 / 输出（无缓存列）,缓存读写落在
//! `hourly_usage`（按本地日 + 小时 + 模型）。故按「该轮在本小时内的输入+输出占比」把小时
//! 缓存量摊回每一轮——同一小时内缓存量与轮的体量强相关,这个近似足够支撑量级判据,
//! 且保证与在线路径用的是**同一套代价公式**（否则拟合出的系数会系统性偏大）。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use chrono::{Local, TimeZone};
use rusqlite::{Connection, OpenFlags};

use super::calib::Pair;
use super::cost::{self, Tokens};
use super::model::Platform;
use super::store::SubStore;

/// 采集源 id（Claude 订阅对应的本地源）。
const CLAUDE_SOURCE: &str = "claude-code";

/// 5h 滚动窗口的长度（秒）——算「这段时间从窗尾老掉的代价」要用它回看。
const WINDOW_SECS: i64 = 5 * 3_600;

/// 回溯上限（天）：一轮最多往回看这么久——越久远的样本越可能跨套餐变更,
/// 也把没有本地轮记录的空档反复重扫的成本兜住。
const HORIZON_DAYS: i64 = 14;

/// 本模块建出来的标定样本在 `usage_pair.src` 里的标记（水位线按它取）。
pub const PAIR_SRC: &str = "desktop";

/// 这一路上次是按哪一版准入判据建的样本（见 `calib:ADMISSION_RULE_VERSION`）。
const META_RULE_VER: &str = "claude_desktop_pair_rule";

/// 「本机解释不了的消耗」的统计窗口。
const FOREIGN_WINDOW_HOURS: i64 = 24;

/// 采集滞后余量（秒）：区间太新时本地轮可能还没被扫到,算成「本机零痕迹」是误判。
/// 取 10 分钟——采集最慢档 5 分钟,留一倍余量（与 collector 的活动窗同尺）。
const COLLECT_LAG_MARGIN_SECS: i64 = 600;

/// 「本机 agent 解释不了的消耗」的证据（**无歧义**信号,不依赖标定系数）。
///
/// 判据：相邻两条服务端样本之间,**用量涨了而本机一条轮记录都没有**。
/// 这条判据 ⇒ **不循环**——它不问"涨得合不合理",只问"本机的 agent 有没有动过"。
///
/// **它说明不了是谁在用**,只说明不是本机的 agent。可能的来源至少四种：
///  同一账号在另一台电脑上跑 agent; 网页版; **本机 Claude 桌面端自己的对话**
/// （桌面端的聊天不走 collector 的 `claude-code` 源,却吃同一份配额——本机这一项
/// 很可能占大头）; 手机 App。所以文案一律说「本地 token 解释不了」,
/// **不要说成「别的设备」**。
///
/// 它**不正**任何东西,只是把盲区量出来摆上台面（P0 的全部职责）：链路的取数与读数
/// 本来就走服务端真值,不会错;真正被这类消耗污染的是**标定**——样本的涨幅含别处的量、
/// 代价只有本机的,持续下去会把 `scale` 系统性拉大、取数偏频（见 HANDOFF）。
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct ForeignEvidence {
    /// 统计窗口内**可判定**的相邻样本区间数（太新的不算,见 `COLLECT_LAG_MARGIN_SECS`）。
    pub windows: usize,
    /// 其中「服务端涨了、本机零痕迹」的区间数。
    pub unexplained: usize,
    /// 这些区间累计的服务端涨幅（百分点）。
    pub unexplained_pct: f64,
    /// 统计窗口长度（小时;前端文案用）。
    pub window_hours: i64,
}

/// 每平台最近一次统计结果（写 = 收割轮;读 = 诊断命令,零 IO）。
static EVIDENCE: LazyLock<Mutex<HashMap<Platform, ForeignEvidence>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn evidence_slot() -> std::sync::MutexGuard<'static, HashMap<Platform, ForeignEvidence>> {
    EVIDENCE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 最近一次统计结果（没统计过 → 全零）。
pub fn evidence(platform: Platform) -> ForeignEvidence {
    evidence_slot().get(&platform).copied().unwrap_or_default()
}

/// 纯逻辑：扫一遍窗口内的相邻样本区间,数出「服务端涨了但本机零痕迹」的那些。
/// `cutoff` = 可判定的最晚区间终点（比它新的区间本地轮可能还没采到,不下结论）。
fn scan_foreign(samples: &[(i64, f64, f64)], turns: &[TurnRow], cutoff: i64) -> ForeignEvidence {
    let mut ev = ForeignEvidence { window_hours: FOREIGN_WINDOW_HOURS, ..Default::default() };
    for w in samples.windows(2) {
        let (t0, used5_0, _) = w[0];
        let (t1, used5_1, _) = w[1];
        if t1 > cutoff {
            continue;
        }
        ev.windows += 1;
        // 区间内有任何一轮结束 = 本机动过（左开右闭,与 build_pairs 同口径）
        let local = turns.iter().any(|t| t.ended_at > t0 && t.ended_at <= t1);
        let delta = used5_1 - used5_0;
        if !local && delta > 0.0 {
            ev.unexplained += 1;
            ev.unexplained_pct += delta;
        }
    }
    ev
}

/// 一轮的本地记录（bootstrap 只需要这几列）。
#[derive(Debug, Clone)]
pub struct TurnRow {
    /// 轮结束时刻（unix 秒）。
    pub ended_at: i64,
    pub model: String,
    pub input: i64,
    pub output: i64,
}

impl TurnRow {
    fn base(&self) -> i64 {
        self.input + self.output
    }

    /// 本地日 + 小时（与 collector 的 hourly_usage 分桶同口径）。
    fn hour_key(&self) -> (String, u8) {
        let dt = Local.timestamp_opt(self.ended_at, 0).single();
        match dt {
            Some(dt) => (dt.format("%Y-%m-%d").to_string(), dt.format("%H").to_string().parse().unwrap_or(0)),
            None => (String::new(), 0),
        }
    }
}

/// 小时级缓存量（（本地日, 小时, 模型) → [缓存读, 缓存写]）。
pub type HourCache = BTreeMap<(String, u8, String), [i64; 2]>;

/// 纯逻辑：由桌面端样本 + 本地轮记录构造标定样本（可单测,不碰 IO）。
/// `samples` 需按时间升序,元素 = （unix 秒, 5h 已用 %, 7d 已用 %)。
pub fn build_pairs(
    samples: &[(i64, f64, f64)],
    turns: &[TurnRow],
    hour_cache: &HourCache,
) -> Vec<(Pair, (f64, f64), BTreeMap<String, [i64; 4]>)> {
    // 每小时每模型的「输入+输出」总量（缓存摊回的分母）
    let mut hour_base: BTreeMap<(String, u8, String), i64> = BTreeMap::new();
    for t in turns {
        let (day, hour) = t.hour_key();
        *hour_base.entry((day, hour, t.model.clone())).or_insert(0) += t.base();
    }

    // 某个时间区间 （lo, hi] 内结束的轮 → 分模型的四项 token（缓存按体量占比摊回）
    let slice = |lo: i64, hi: i64| -> BTreeMap<String, [i64; 4]> {
        let mut b: BTreeMap<String, [i64; 4]> = BTreeMap::new();
        for t in turns.iter().filter(|t| t.ended_at > lo && t.ended_at <= hi) {
            let (day, hour) = t.hour_key();
            let key = (day, hour, t.model.clone());
            let (cr, cw) = match (hour_cache.get(&key), hour_base.get(&key)) {
                (Some(c), Some(base)) if *base > 0 => {
                    let share = t.base() as f64 / *base as f64;
                    ((c[0] as f64 * share) as i64, (c[1] as f64 * share) as i64)
                }
                _ => (0, 0),
            };
            let slot = b.entry(t.model.clone()).or_insert([0; 4]);
            slot[0] += t.input;
            slot[1] += t.output;
            slot[2] += cr;
            slot[3] += cw;
        }
        b
    };
    // 分模型 token → （总代价, 其中未知模型的代价)；`at` = 取价时刻
    let price = |b: &BTreeMap<String, [i64; 4]>, at: i64| -> (f64, f64) {
        let (mut total, mut unknown) = (0.0, 0.0);
        for (model, v) in b {
            let tokens = Tokens { input: v[0], output: v[1], cache_read: v[2], cache_write: v[3] };
            let (c, known) = cost::cost_of(Platform::Claude, model, &tokens, at);
            total += c;
            if !known {
                unknown += c;
            }
        }
        (total, unknown)
    };

    let mut out = vec![];
    for w in samples.windows(2) {
        let (t0, used5_0, used7_0) = w[0];
        let (t1, used5_1, used7_1) = w[1];
        // 区间内结束的轮（左开右闭,与「两次读数之间」的语义一致）
        let breakdown = slice(t0, t1);
        if breakdown.is_empty() {
            continue;
        }
        // at = t1：该区间右端点,与 store:recompute_stale_costs 同口径
        let (c_total, c_unknown) = price(&breakdown, t1);
        // **老掉的**：发生在 （t0−5h, t1−5h] 的那些轮——5h 是滚动窗口,计数器的变化是
        // 「新花的 − 老掉的」。轮记录读得不够早时这里自然
        // 算成 0 = 不做正,不会算错方向。
        let aged = price(&slice(t0 - WINDOW_SECS, t1 - WINDOW_SECS), t1).0;
        // 桌面端那份采样历史只有两个百分比,没有窗口重置时刻 ⇒ 两端都是 None,
        // 重置只能退回「读数变小了」去判（语义见 calib:Pair:window_reset）。
        let pair = Pair {
            t0,
            t1,
            used5_0,
            used5_1,
            resets5_0: None,
            resets5_1: None,
            cost: c_total,
            unknown_cost: c_unknown,
            aged_cost: aged,
        };
        if !pair.usable(super::calib::scale(Platform::Claude)) {
            continue; // 重置轮 / 间隔越界 / 零代价,与在线样本同一套筛选
        }
        out.push((pair, (used7_0, used7_1), breakdown));
    }
    out
}

/// 一条待落库的标定样本：`（样本, （7d 两端), breakdown JSON)`。
type PairItem = (Pair, (f64, f64), String);

/// 按**当前套餐的起始观测时刻**把一批样本切成「标真套餐」与「标套餐未知」两摞。
///
/// 边界取 `t0`（区间**开始**的时刻）：跨在边界上的那一条,它的消耗有一部分发生在旧套餐
/// 下 ⇒ 归到「未知」这一摞才不会把旧套餐的量记到新套餐头上。
/// `since = None`（没有已知边界）→ 整批归「真套餐」,与加这条之前的行为逐条相同。
fn split_by_plan_since(items: Vec<PairItem>, since: Option<i64>) -> (Vec<PairItem>, Vec<PairItem>) {
    match since {
        Some(b) => items.into_iter().partition(|(p, _, _)| p.t0 >= b),
        None => (items, vec![]),
    }
}

/// 读 collector.db 里近 `HORIZON_DAYS` 天的轮记录（只读连接;库不存在 → 空）。
fn read_turns(db: &Path, since: i64) -> Vec<TurnRow> {
    let Ok(conn) = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return vec![];
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT ended_at, model_key, input_tokens, output_tokens FROM turn_raw
         WHERE agent_key = ?1 AND ended_at >= ?2",
    ) else {
        return vec![];
    };
    let rows = stmt.query_map(rusqlite::params![CLAUDE_SOURCE, since * 1000], |r| {
        Ok(TurnRow {
            // 库里是毫秒,本模块统一用秒
            ended_at: r.get::<_, i64>(0)? / 1000,
            model: r.get(1)?,
            input: r.get(2)?,
            output: r.get(3)?,
        })
    });
    rows.map(|rs| rs.flatten().collect()).unwrap_or_default()
}

/// 读小时级缓存量（同一只读连接开销很小,分开写更清楚）。
/// `since_day` = 本地日字符串下界（`%Y-%m-%d`,与 collector 的分桶同口径;字典序
/// 即时间序）——稳态下水位线就在十几分钟前,这一查只落在一两天的桶上。
fn read_hour_cache(db: &Path, since_day: &str) -> HourCache {
    let Ok(conn) = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return HourCache::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT day, hour, model_key, cache_read_tokens, cache_write_tokens FROM hourly_usage
         WHERE agent_key = ?1 AND day >= ?2",
    ) else {
        return HourCache::new();
    };
    let rows = stmt.query_map([CLAUDE_SOURCE, since_day], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u8, r.get::<_, String>(2)?),
            [r.get::<_, i64>(3)?, r.get::<_, i64>(4)?],
        ))
    });
    rows.map(|rs| rs.flatten().collect()).unwrap_or_default()
}

/// 本地日字符串（与 collector 的 `hourly_usage` 分桶同口径）。
fn day_of(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// 收割 + 增量标定（主轮询每轮调用一次;**零网络、零凭据**）。
/// 返回 `（本次新收割的样本数, 本次新建的标定样本数)`。
///
/// 三步都幂等：收割按主键忽略重复,建样本按水位线只处理新区间,没有新东西就是几条
/// 索引查询的开销。首次运行（空库 / 刚升级）等价于原来的冷启动——水位线落在
/// `now - HORIZON_DAYS`,一次把近两周补齐。
pub fn ingest(sub_store: &SubStore, collector_db: &Path, now: i64) -> (usize, usize) {
    let harvested = sub_store
        .insert_samples(Platform::Claude, &super::claude_desktop::all_samples())
        .unwrap_or(0);

    // 水位线 = 桌面端那一路已经建到哪儿;没有则从回溯上限起（= 原冷启动行为）。
    let floor = now - HORIZON_DAYS * 86_400;
    // 判据升版 ⇒ 这一路把「读数还在的那一段」按新判据重建一次:水位线整体退回下界,
    // 旧行等新样本建出来之后再按它们的首尾删（见 calib:ADMISSION_RULE_VERSION）。
    let rule_stale =
        sub_store.meta_i64(META_RULE_VER) != Some(super::calib::ADMISSION_RULE_VERSION);
    let since = if rule_stale {
        floor
    } else {
        sub_store.latest_pair_t1(Platform::Claude, PAIR_SRC).unwrap_or(floor).max(floor)
    };
    // 轮记录读一次覆盖两处用途：标定按水位线切,证据统计按固定 24 小时窗——取两者较早的
    // 起点一次读完,免得同一张表在一轮里扫两遍。
    let win_since = now - FOREIGN_WINDOW_HOURS * 3600;
    // 再往前一个窗长：老化量的原料在 `（t0−5h, …]`,少读这一段会把最早那批区间的
    // 老化量算成 0（见 build_pairs）。
    let turns = read_turns(collector_db, since.min(win_since) - WINDOW_SECS);

    let samples = sub_store.samples_since(Platform::Claude, since);
    let mut n = 0;
    if samples.len() >= 2 {
        let hour_cache = read_hour_cache(collector_db, &day_of(since - WINDOW_SECS));
        // 首轮（空库 / 刚升级）一次能建上千条 ⇒ 一次事务写完,不逐条提交
        // breakdown 写**规范的裸 map**（与在线路同形状）——旧版在这里包了一层
        // `{"src":…,"models":{…}}`,而 src 现已是独立列。形状统一,将来按新权重
        // 重算存量样本时才不会只能恢复一半（见 cost:parse_breakdown）。
        let items: Vec<_> = build_pairs(&samples, &turns, &hour_cache)
            .into_iter()
            .map(|(pair, used7, breakdown)| {
                let json = serde_json::to_string(&breakdown).unwrap_or_else(|_| "{}".into());
                (pair, used7, json)
            })
            .collect();
        // 套餐标注：桌面端的采样历史**不带套餐**,本机只有
        // 「当前快照的套餐」这一个已知量。旧版拿它硬套整段回补区间——换过档的人会被
        // 把旧套餐的历史标成新套餐,而 `pairs_for_fit` 按套餐筛之后,**标错比不标更糟**
        // （它会把新套餐的估计往旧套餐拖）。
        //
        // 故以 store 记下的**当前套餐起始观测时刻**为界一分为二：从它之后开始的区间标
        // 真套餐,之前的标空串 = **套餐未知**（`pairs_for_fit` 放行,但不会被算成别的
        // 套餐的账）。没有边界——存量库升上来、或从未成功取过一次数——就沿用旧行为
        // 整批标当前套餐,免得升级当天把刚建的样本全标成未知。
        let plan = sub_store
            .load_snapshot(Platform::Claude)
            .map(|s| s.plan_type)
            .unwrap_or_default();
        // 重建：先有新样本,再删它们覆盖到的那一段旧行——顺序反过来就会在这中间
        // 把「删了又建不出来」的区间永久丢掉。
        let removed = if rule_stale && !items.is_empty() {
            let lo = items.iter().map(|(p, _, _)| p.t0).min().unwrap_or(0);
            let hi = items.iter().map(|(p, _, _)| p.t1).max().unwrap_or(0);
            let r = sub_store.delete_pairs_in(Platform::Claude, PAIR_SRC, lo, hi);
            let _ = sub_store.set_meta_i64(META_RULE_VER, super::calib::ADMISSION_RULE_VERSION);
            Some((r, lo, hi))
        } else {
            None
        };
        let (current, unknown) = split_by_plan_since(items, sub_store.plan_since(Platform::Claude));
        n = sub_store
            .insert_pairs(Platform::Claude, &current, PAIR_SRC, &plan)
            .unwrap_or(0)
            + sub_store
                .insert_pairs(Platform::Claude, &unknown, PAIR_SRC, "")
                .unwrap_or(0);
        if !unknown.is_empty() {
            crate::dev_log!(
                "[subscription] claude desktop pairs: {} before plan boundary marked plan-unknown, {} as '{}'",
                unknown.len(),
                current.len(),
                plan
            );
        }
        if let Some((r, lo, hi)) = removed {
            crate::dev_log!(
                "[subscription] claude desktop pairs rebuilt under admission rule v{}: \
                 -{} old, +{} new over [{}, {}]",
                super::calib::ADMISSION_RULE_VERSION,
                r,
                n,
                lo,
                hi
            );
        }
    }
    if n > 0 {
        super::calib::refit_from_store(sub_store, Platform::Claude);
    }

    // 「本机解释不了的消耗」证据：**窗口统计,不是按轮累加**——被跳过的区间每轮都会
    // 重新评估一次（水位线只在真的插进样本时前移）,累加会把同一个区间数很多遍。
    // 每轮重算一遍固定 24 小时窗,天然幂等。turns 为空（本机一天没用过）时照常统计
    // ——那正是「全是别处在用」的情形,不能跳过。
    let ev = scan_foreign(
        &sub_store.samples_since(Platform::Claude, win_since),
        &turns,
        now - COLLECT_LAG_MARGIN_SECS,
    );
    let changed = evidence(Platform::Claude).unexplained != ev.unexplained;
    evidence_slot().insert(Platform::Claude, ev);

    if harvested > 0 || n > 0 {
        log_density(sub_store, harvested, n);
    }
    if ev.unexplained > 0 && (changed || harvested > 0) {
        crate::dev_log!(
            "[subscription] claude usage not explained by local tokens: {}/{} window(s) in {}h, +{:.1}%",
            ev.unexplained,
            ev.windows,
            ev.window_hours,
            ev.unexplained_pct
        );
    }
    (harvested, n)
}

/// 收割结果 + **采样密度**一行：
/// 已留存条数 / 跨度 / **间隔中位数**——桌面端源文件只保 14 天,跨度超过它就说明
/// `desktop_sample` 真的在累积源里已经没有的观测。
///
/// 间隔取**中位数**而非均值：桌面端只在自己运行时
/// 采样,关机 / 休眠留下的十几小时空档会把均值拉到实际节律的两倍以上,而这行字的全部用处
/// 就是让人一眼看出采样密度。
fn log_density(sub_store: &SubStore, harvested: usize, pairs: usize) {
    let total = sub_store.sample_count(Platform::Claude);
    let span_days = match sub_store.sample_span(Platform::Claude) {
        Some((first, last)) if total > 1 => (last - first) as f64 / 86_400.0,
        _ => 0.0,
    };
    let gap = sub_store.median_sample_gap(Platform::Claude).unwrap_or(0);
    crate::dev_log!(
        "[subscription] claude desktop harvest +{} sample(s) → kept {} over {:.1}d (median gap {}s) → +{} pair(s)",
        harvested,
        total,
        span_days,
        gap,
        pairs
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(ended_at: i64, model: &str, input: i64, output: i64) -> TurnRow {
        TurnRow { ended_at, model: model.into(), input, output }
    }

    /// 样本间隔 900 秒（桌面端节律）,用量按 1% 涨。
    fn samples() -> Vec<(i64, f64, f64)> {
        vec![(10_000, 10.0, 4.0), (10_900, 11.0, 4.0), (11_800, 13.0, 5.0)]
    }

    #[test]
    fn pairs_take_turns_inside_each_interval() {
        let turns = vec![
            turn(9_500, "claude-sonnet-5", 50_000, 1_000), // 第一条样本之前 → 不计
            turn(10_500, "claude-sonnet-5", 100_000, 2_000),
            turn(11_500, "claude-opus-5", 30_000, 3_000),
        ];
        let out = build_pairs(&samples(), &turns, &HourCache::new());
        assert_eq!(out.len(), 2, "两个区间各一条样本");
        assert_eq!(out[0].0.t0, 10_000);
        assert_eq!(out[0].2["claude-sonnet-5"][0], 100_000, "只取区间内结束的轮");
        assert!(out[1].0.cost > out[0].0.cost, "Opus 区间代价更高（权重 5 倍）");
    }

    #[test]
    fn hourly_cache_is_shared_out_by_turn_size() {
        // 同一小时的两轮落在**不同区间**,体量 3:1 → 小时缓存按同比例摊给两个区间。
        // 两个时刻都在同一 UTC 半小时内,任何整点 / 半点时区都不会把它们分到两个小时桶。
        // token 量按真实量级取（每 1% 对应几十万基准 token）——否则隐含比值落在
        // calib.rs 的可信带之外,样本会被当成账目错配丢掉。
        let samples = vec![(10_900, 10.0, 4.0), (11_100, 11.0, 4.0), (11_900, 12.0, 5.0)];
        let turns = vec![
            turn(11_000, "claude-sonnet-5", 300_000, 0),
            turn(11_200, "claude-sonnet-5", 100_000, 0),
        ];
        let (day, hour) = turns[0].hour_key();
        assert_eq!((day.clone(), hour), turns[1].hour_key(), "两轮必须同一小时桶");
        let mut cache = HourCache::new();
        cache.insert((day, hour, "claude-sonnet-5".into()), [800_000, 40_000]);
        let out = build_pairs(&samples, &turns, &cache);
        assert_eq!(out[0].2["claude-sonnet-5"][2], 600_000, "大轮拿 3/4 缓存读");
        assert_eq!(out[0].2["claude-sonnet-5"][3], 30_000);
        assert_eq!(out[1].2["claude-sonnet-5"][2], 200_000, "小轮拿 1/4");
        assert_eq!(out[1].2["claude-sonnet-5"][3], 10_000);
    }

    /// 「服务端涨了但本机零痕迹」= 多机 / 网页消耗的无歧义证据,不依赖标定系数。
    #[test]
    fn foreign_use_is_counted_only_when_local_is_silent() {
        // 三个区间:①本机有轮 ②本机没轮但涨了 ③本机没轮且没涨（滚动过期）
        let samples = vec![
            (10_000, 10.0, 4.0),
            (10_900, 12.0, 4.0), // ① 有本地轮
            (11_800, 15.0, 5.0), // ② 零本地轮却涨了 3 个点 ⇒ 别处在用
            (12_700, 14.0, 5.0), // ③ 零本地轮且掉了 ⇒ 正常的窗口过期
        ];
        let turns = vec![turn(10_500, "claude-sonnet-5", 100_000, 0)];
        let ev = scan_foreign(&samples, &turns, 99_999);
        assert_eq!(ev.windows, 3);
        assert_eq!(ev.unexplained, 1, "只有第二段算");
        assert!((ev.unexplained_pct - 3.0).abs() < 1e-9);
        assert_eq!(ev.window_hours, FOREIGN_WINDOW_HOURS);
    }

    /// 太新的区间不下结论——本地轮可能还没被采集扫到,误判成「本机零痕迹」最伤。
    #[test]
    fn too_recent_windows_are_not_judged() {
        let samples = vec![(10_000, 10.0, 4.0), (10_900, 13.0, 4.0)];
        assert_eq!(scan_foreign(&samples, &[], 10_899).windows, 0, "区间终点比 cutoff 新 → 不判");
        assert_eq!(scan_foreign(&samples, &[], 10_900).unexplained, 1, "到点即可判");
    }

    /// 本机一整天没用过 ⇒ 全窗口都是别处在用,这正是要抓的情形,不能因为没有轮就跳过。
    #[test]
    fn all_foreign_when_local_never_ran() {
        let samples = vec![(10_000, 10.0, 4.0), (10_900, 12.0, 4.0), (11_800, 14.0, 5.0)];
        let ev = scan_foreign(&samples, &[], 99_999);
        assert_eq!((ev.windows, ev.unexplained), (2, 2));
        assert!((ev.unexplained_pct - 4.0).abs() < 1e-9);
    }

    #[test]
    fn intervals_without_local_turns_are_skipped() {
        // 桌面端涨了但本地没有轮 = 在线 / 网页用量,不能用来标定本地换算
        let out = build_pairs(&samples(), &[turn(10_500, "claude-sonnet-5", 100_000, 0)], &HourCache::new());
        assert_eq!(out.len(), 1, "只剩有本地轮的那个区间");
        assert_eq!(out[0].0.t1, 10_900);
    }

    /// 掉下去的读数丢,没涨的读数**收**——只要它本来就不该涨得动一格
    /// （2026-09-19 定案,判据见 `calib::Pair::usable`）。
    #[test]
    fn dropping_readings_are_dropped_but_flat_ones_are_kept() {
        let turns = vec![turn(10_500, "claude-sonnet-5", 100_000, 0)];
        // 用量掉了 = 窗口重置（桌面端采样没有窗尾,只能这么判）
        let reset = vec![(10_000, 30.0, 4.0), (10_900, 2.0, 4.0)];
        assert!(build_pairs(&reset, &turns, &HourCache::new()).is_empty());
        // 没涨,而这段消耗本来就不够动一个百分点 ⇒ 有效样本,计回分母
        let flat = vec![(10_000, 10.0, 4.0), (10_900, 10.0, 4.0)];
        assert_eq!(build_pairs(&flat, &turns, &HourCache::new()).len(), 1);
        // 预计要涨好几个百分点却纹丝不动 ⇒ 不是量化,是账目不对,照丢
        let heavy = vec![turn(10_500, "claude-sonnet-5", 100_000_000, 0)];
        assert!(build_pairs(&flat, &heavy, &HourCache::new()).is_empty());
    }

    /// 套餐边界（设计 §9-10 ②）：边界之前**开始**的区间标「套餐未知」,
    /// 之后的才标真套餐;没有边界 = 整批标真套餐（存量库升上来的行为不变）。
    #[test]
    fn pairs_starting_before_the_plan_boundary_are_marked_plan_unknown() {
        let item = |t0: i64| -> PairItem {
            (
                Pair {
                    t0,
                    t1: t0 + 900,
                    used5_0: 10.0,
                    used5_1: 11.0,
                    resets5_0: None,
                    resets5_1: None,
                    cost: 1.0,
                    unknown_cost: 0.0,
                    aged_cost: 0.0,
                },
                (4.0, 4.0),
                "{}".into(),
            )
        };
        let batch = || vec![item(1_000), item(2_000), item(3_000)];

        // 边界落在第二条的起点:它和之后的算当前套餐,更早的算未知
        let (current, unknown) = split_by_plan_since(batch(), Some(2_000));
        assert_eq!(current.iter().map(|(p, _, _)| p.t0).collect::<Vec<_>>(), [2_000, 3_000]);
        assert_eq!(unknown.iter().map(|(p, _, _)| p.t0).collect::<Vec<_>>(), [1_000]);

        // 跨在边界上的区间（t0 < 边界 <= t1）归「未知」——它的消耗有一部分发生在旧套餐下
        let (current, unknown) = split_by_plan_since(batch(), Some(2_500));
        assert_eq!(current.iter().map(|(p, _, _)| p.t0).collect::<Vec<_>>(), [3_000]);
        assert_eq!(unknown.len(), 2);

        // 没有边界 → 一条都不标未知
        let (current, unknown) = split_by_plan_since(batch(), None);
        assert_eq!(current.len(), 3);
        assert!(unknown.is_empty());
    }
}
