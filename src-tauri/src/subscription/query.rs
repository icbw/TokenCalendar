//! 订阅价格与读数的**只读查询面**：六条命令,只读、零网络、不碰任何写连接。
//!
//! | 组 | 命令 | 出什么 |
//! | --- | --- | --- |
//! | 价格 | `get_price_models` | 某平台**全部模型的全部生效期**（价格梯度图的数据源） |
//! | 价格 | `get_price_at` | 某**时刻**全线的有效价目（官方价目对照表 / 诊断） |
//! | 价格 × 用量 | `get_model_usage` | 区间内**分模型 × 价目段**的用量与美元当量 |
//! | 价格 × 轮次 | `get_message_budget` | 分模型「一轮吃掉多少 5h 额度」（剩余消息数的分母） |
//! | 读数 | `get_quota_readings` | 归一化读数序列（`quota_reading`） |
//! | 读数 | `get_quota_days` | 日级汇总（`quota_daily`;年度曲线走这条,别去扫读数层） |
//!
//! 两条口径红线,写在这里免得展示层自己发明：
//!
//! - **美元当量 ≠ 账单**。`usd` 是「这些 token 若按官方 API 单价计费值多少钱」,
//!   而用户付的是固定月费。文案一律说「相当于」。
//! - **取价只有一个入口**。分模型用量的单价取自 [`cost:priced_at`],即 `cost_of`
//!   自己用的那把尺子——回落模型与 `codex-auto-review` 的路由折价都发生在那里,
//!   绕过它去查价目表会得出另一个数。

use serde::Serialize;
use tauri::State;

use super::cost;
use super::model::{Platform, QuotaDay, QuotaReading};
use super::price::{self, PriceRow};
use super::SubscriptionReader;
use crate::AppState;

/// f64 的 `Sum` 以 `-0.0` 起步（加法单位元保号）,于是**空的求和结果是负零**,
/// 序列化出去就是 `-0.0`——展示层拿到会印成「$-0.00」。这里把它按回正零。
fn unsigned_zero(v: f64) -> f64 {
    v + 0.0
}

/// 平台名解析（命令面统一的入参校验）。
fn platform_of(s: &str) -> Result<Platform, String> {
    Platform::from_str(s).ok_or_else(|| format!("unknown platform: {s}"))
}

/// 本地日期 `YYYY-MM-DD` 的第 `hour` 小时的起点（unix 秒）。
///
/// 夏令时的"空洞小时"（本地时钟跳过的那一小时）取其后第一个真实存在的时刻
/// ——那个小时里不可能有用量记录,这一步只是不让它把整行丢掉。
fn local_hour_ts(day: &str, hour: u8) -> Option<i64> {
    use chrono::TimeZone;
    let d = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    let naive = d.and_hms_opt(hour as u32, 0, 0)?;
    chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .or_else(|| chrono::Local.from_local_datetime(&(naive + chrono::Duration::hours(1))).earliest())
        .map(|dt| dt.timestamp())
}

// ---------- 价格 ----------

/// 某平台**全部模型的全部生效期**（按 match_key、生效期升序）。
///
/// 有第二行的模型就是被官方降过价的,价格梯度图里的台阶正是这些行。
#[tauri::command]
pub fn get_price_models(platform: String) -> Result<Vec<PriceRow>, String> {
    Ok(price::rows_for(platform_of(&platform)?))
}

/// 某时刻全线的有效价目（`at` 省略 = 此刻）。
#[derive(Debug, Serialize)]
pub struct PriceAtResult {
    pub platform: Platform,
    /// 实际取价的时刻（回显入参;省略时是服务端的"此刻"）。
    pub at: i64,
    /// 每个 `match_key` 至多一行。
    pub rows: Vec<PriceRow>,
}

/// 某时刻各模型的有效价目（官方价目对照表 / 诊断用）。
#[tauri::command]
pub fn get_price_at(platform: String, at: Option<i64>) -> Result<PriceAtResult, String> {
    let platform = platform_of(&platform)?;
    let at = at.unwrap_or_else(|| chrono::Utc::now().timestamp());
    Ok(PriceAtResult { platform, at, rows: price::rows_at(platform, at) })
}

// ---------- 价格 × 用量 ----------

/// 一个模型在**一段价目生效期**内的用量与美元当量。
#[derive(Debug, Serialize)]
pub struct ModelUsageSegment {
    /// 该段价目的起点（`None` = 没命中任何价目键,单价来自回落常量）。
    pub effective_from: Option<i64>,
    /// 实际命中的价目键（`None` = 回落;`codex-auto-review` 命中的是它**路由到**的键）。
    pub match_key: Option<String>,
    /// 该段价目是否可信（回落 / 路由标签 → false）。
    pub known: bool,
    pub usd_input: f64,
    pub usd_output: f64,
    pub usd_cache_read: f64,
    pub usd_cache_write: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// 该段 token 按该段单价折成的美元当量。
    pub usd: f64,
}

/// 一个模型在整个区间内的用量（`segments` 之和 + 不可切分的轮次）。
#[derive(Debug, Serialize)]
pub struct ModelUsageRow {
    /// collector 里的模型键,原样（`unknown` = 采集时没认出模型的兜底行）。
    pub model_key: String,
    /// 展示名（取命中价目行的 `display_name`;没命中则回 `model_key`）。
    pub display_name: String,
    /// 全部段的价目都可信才为真。
    pub known: bool,
    /// 用户发起的对话轮次。
    ///
    /// **它不按价目段切分**：轮次只有日粒度,把一天的轮次摊到两段上就是造数据。
    /// 跨降价那天的模型,这个数仍然是整个区间的合计。
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub total_tokens: i64,
    /// 美元当量合计（= 各段各按各自单价算完再相加）。
    pub usd: f64,
    /// 按价目生效期切开的明细（一个模型没被降过价 ⇒ 只有一段）。
    pub segments: Vec<ModelUsageSegment>,
}

/// `get_model_usage` 的出口。
#[derive(Debug, Serialize)]
pub struct ModelUsageResult {
    pub platform: Platform,
    /// 实际生效的日区间（回显;入参省略时 = 该平台在 collector 里的全部历史）。
    pub from: String,
    pub to: String,
    /// 区间内全部模型的美元当量合计。
    pub usd_total: f64,
    /// 其中**价目不可信**那部分的美元当量（回落模型 + `codex-auto-review`）。
    /// 展示层据此给「这部分是估的」的提示,不要把它当成误差棒。
    pub usd_unknown: f64,
    /// 按美元当量降序。
    pub rows: Vec<ModelUsageRow>,
}

/// 区间内的**分模型**用量与代价,按各模型自己的价目生效段切分。
///
/// - `from` / `to` = 本地日期 `YYYY-MM-DD`,闭区间;省略 = 该平台的全部历史。
/// - 用量取自 collector 的**小时表**,每小时按其起点时刻取价 ⇒ 降价分界落在哪一刻
///   就从哪一刻切开,不需要「这一天算旧价还是新价」这种人为规则。
/// - 代价口径与 `cost.rs` 完全一致（同一个 [`cost:priced_at`]）。
#[tauri::command]
pub fn get_model_usage(
    platform: String,
    from: Option<String>,
    to: Option<String>,
    state: State<'_, AppState>,
) -> Result<ModelUsageResult, String> {
    let platform = platform_of(&platform)?;
    crate::commands::with_reader(&state, |store| Ok(model_usage(store, platform, from, to)))
}

/// [`get_model_usage`] 的本体（与命令分开,好让真库 smoke 直接喂一个 collector.db 副本）。
pub(super) fn model_usage(
    store: &crate::collector::store::Store,
    platform: Platform,
    from: Option<String>,
    to: Option<String>,
) -> ModelUsageResult {
    use std::collections::BTreeMap;

    let agent = platform.collector_source();
    // 区间没给就取该 agent 的全部历史;一条记录都没有就落成空区间（`day <= ""`
    // 谁也匹配不上）——「这个平台还没有本机用量」是正常状态,不是错误。
    let span = store.agent_day_span(agent);
    let from = from.or_else(|| span.as_ref().map(|s| s.0.clone())).unwrap_or_default();
    let to = to.or_else(|| span.as_ref().map(|s| s.1.clone())).unwrap_or_default();

    //  小时 × 模型 → 按 （模型, 价目段) 归并
    //    段的身份用 （effective_from, match_key)：同一个模型在区间内跨过降价就会
    //    分出两个桶,没跨过就只有一个。
    type SegKey = (Option<i64>, Option<String>);
    let mut acc: BTreeMap<String, BTreeMap<SegKey, (cost::Priced, [i64; 4])>> = BTreeMap::new();
    for (day, hour, model, tok) in store.model_usage_hours(agent, &from, &to) {
        if tok.iter().all(|v| *v == 0) {
            continue;
        }
        let Some(at) = local_hour_ts(&day, hour) else { continue };
        let priced = cost::priced_at(platform, &model, at);
        let key = (priced.effective_from, priced.match_key.clone());
        let e = acc
            .entry(model)
            .or_default()
            .entry(key)
            .or_insert_with(|| (priced, [0; 4]));
        for i in 0..4 {
            e.1[i] += tok[i];
        }
    }

    //  轮次按模型挂上（只有日粒度,不进段;理由见 ModelUsageRow:requests）
    let requests: BTreeMap<String, i64> =
        store.model_requests(agent, &from, &to).into_iter().collect();

    let mut rows: Vec<ModelUsageRow> = acc
        .into_iter()
        .map(|(model_key, segs)| {
            let mut segs: Vec<(cost::Priced, [i64; 4])> = segs.into_values().collect();
            segs.sort_by_key(|(p, _)| p.effective_from.unwrap_or(i64::MIN));
            // 展示名取**最后一段**命中的那行：模型改名 / 拆代号之后,面板该叫新名字。
            // `Priced` 自己就带着它,不必再回查一遍价目表。
            let display_name = segs
                .last()
                .map(|(p, _)| p.display_name.clone())
                .unwrap_or_else(|| model_key.clone());
            let segments: Vec<ModelUsageSegment> = segs
                .into_iter()
                .map(|(p, t)| {
                    let tokens = cost::Tokens {
                        input: t[0],
                        output: t[1],
                        cache_read: t[2],
                        cache_write: t[3],
                    };
                    ModelUsageSegment {
                        effective_from: p.effective_from,
                        match_key: p.match_key.clone(),
                        known: p.known,
                        usd_input: p.usd_input,
                        usd_output: p.usd_output,
                        usd_cache_read: p.usd_cache_read,
                        usd_cache_write: p.usd_cache_write,
                        input_tokens: t[0],
                        output_tokens: t[1],
                        cache_read_tokens: t[2],
                        cache_write_tokens: t[3],
                        usd: p.usd_of(&tokens),
                    }
                })
                .collect();
            let sum = |f: fn(&ModelUsageSegment) -> i64| segments.iter().map(f).sum::<i64>();
            let input_tokens = sum(|s| s.input_tokens);
            let output_tokens = sum(|s| s.output_tokens);
            let cache_read_tokens = sum(|s| s.cache_read_tokens);
            let cache_write_tokens = sum(|s| s.cache_write_tokens);
            ModelUsageRow {
                known: segments.iter().all(|s| s.known),
                requests: requests.get(&model_key).copied().unwrap_or(0),
                total_tokens: input_tokens + output_tokens + cache_read_tokens + cache_write_tokens,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                usd: unsigned_zero(segments.iter().map(|s| s.usd).sum()),
                display_name,
                model_key,
                segments,
            }
        })
        .collect();
    rows.sort_by(|a, b| b.usd.total_cmp(&a.usd).then(a.model_key.cmp(&b.model_key)));

    ModelUsageResult {
        platform,
        from,
        to,
        usd_total: unsigned_zero(rows.iter().map(|r| r.usd).sum()),
        usd_unknown: unsigned_zero(
            rows.iter()
                .flat_map(|r| r.segments.iter())
                .filter(|s| !s.known)
                .map(|s| s.usd)
                .sum(),
        ),
        rows,
    }
}

// ---------- 剩余消息数 ----------

/// 每轮代价的取样窗口（天,含今天）：够攒出中位数,又跟得上用法的变化。
const BUDGET_LOOKBACK_DAYS: i64 = 30;
/// 一个模型至少要有这么多轮才给估计——两三轮的中位数就是那两三轮本身。
pub(super) const BUDGET_MIN_TURNS: usize = 5;
/// 「主力模型」看最近这么多天谁的轮最多（没有就退回整个取样窗口）。
const MAIN_MODEL_DAYS: i64 = 7;
/// 周窗口的名义长度。实际窗口可能更短（平台会主动提前重置）,所以它只当上界用。
const WEEK_SECS: i64 = 7 * 86_400;

/// 一个模型的每轮代价。
#[derive(Debug, Serialize)]
pub struct MessageCostRow {
    /// collector 里的模型键,原样。
    pub model_key: String,
    /// 命中价目行的展示名（可能是区间名,如「Claude Opus 4.5〜5」;短名由展示层自己取）。
    pub display_name: String,
    /// 取样窗口里以它为主的用户轮数（= 中位数的样本数）。
    pub turns: usize,
    /// 每轮代价的**中位数**（美元当量,只含根会话自己的 token）。
    pub median_usd: f64,
    /// 一轮吃掉 5h 窗口的百分点 = 中位代价 × `overhead` × `scale`。
    /// 展示层用它去除自己显示的剩余 %,得到还能发几条;100 ÷ 它 = 一个满窗口多少条。
    pub pct_per_turn: f64,
    /// 一轮吃掉**周**窗口的百分点（周系数样本不够时 `None`,且只在带 `week` 查询时给）。
    pub pct_per_turn_week: Option<f64>,
}

/// `get_message_budget` 的出口：分模型的「每轮吃掉多少额度」。
///
/// **只给每轮代价,不给剩余条数**——剩余 % 以展示层手里的快照为准,在那边除,
/// 免得条数与表盘上的百分比来自两个不同时刻。
#[derive(Debug, Serialize)]
pub struct MessageBudget {
    pub platform: Platform,
    /// 取样窗口起点（本地日,含）。
    pub from: String,
    /// 当前标定系数（百分点 / 美元当量,5h 窗口）。
    pub scale: f64,
    /// 系数是否已按校准（false = 出厂预设,估计更粗）。
    pub calibrated: bool,
    /// 子会话开销倍率 = 全部切片代价 ÷ 根会话切片代价（≥ 1）。
    /// 子代理 / Codex 自动审查不算用户轮,但吃同一份额度——按比例摊进每一轮。
    pub overhead: f64,
    /// 主力模型（最近 7 天用户轮最多的那个;`None` = 没有够样本的模型）。
    pub main_model: Option<String>,
    /// 够样本的模型,按轮数降序。
    pub rows: Vec<MessageCostRow>,
    /// 周窗口那一半（只在 `week = true` 时给;悬浮球不要它）。
    pub week: Option<WeekForecast>,
}

/// 一次周重置。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WeekReset {
    /// 新窗口的起点（unix 秒;读数稀疏时是「最晚不早于它」的估计,见 [`week_window`]）。
    pub t: i64,
    /// 提前重置：发生在上一个窗口申报的重置时刻之前（平台主动重置）。
    pub early: bool,
}

/// 当前周窗口里某个模型已经发了多少条。
#[derive(Debug, Serialize)]
pub struct WeekTurns {
    pub model_key: String,
    pub display_name: String,
    pub turns: usize,
}

/// 周窗口：系数、当前窗口的起止、重置历史、窗口内已发条数。
#[derive(Debug, Serialize)]
pub struct WeekForecast {
    /// 周系数（百分点 / 美元当量;样本不够 = `None`,展示层不出周剩余那一列）。
    pub scale: Option<f64>,
    /// 参与周系数拟合的样本数。
    pub pairs: u32,
    /// 当前账号（= 最近一条周读数的套餐名;空 = 不知道）。一台机器可能轮换几个账号,
    /// 周窗口是**按账号**的,下面几项都只看这个账号。
    pub account: String,
    /// 当前窗口起点（`None` = 没有任何周读数）。
    pub start: Option<i64>,
    /// 当前窗口申报的重置时刻（`None` = 来源没给,或窗口已过期、新窗尾还没读到）。
    pub resets_at: Option<i64>,
    /// 这个账号在读数历史里的全部重置,按时间升序（区间筛选交给展示层）。
    pub resets: Vec<WeekReset>,
    /// 当前窗口里的用户轮,按模型（代价最大者）,按条数降序。
    pub turns: Vec<WeekTurns>,
    /// 当前窗口里的用户轮合计。
    pub total_turns: usize,
}

/// 分模型的每轮额度代价（悬浮球 hover、设置页模型选择、Insights 消息数表的数据源）。
///
/// 「消息」= 用户发起的对话轮次（`request_count` 口径）。每轮代价取**中位数**：
/// 轮体量分布重尾,均值会被少数巨型轮带偏。一轮用了几个模型时归给代价最大的那个。
/// `week = true` 时另附周窗口那一半（读数历史 + 周系数,悬浮球不需要）。
#[tauri::command]
pub fn get_message_budget(
    platform: String,
    week: Option<bool>,
    state: State<'_, AppState>,
    sub: State<'_, SubscriptionReader>,
) -> Result<MessageBudget, String> {
    let platform = platform_of(&platform)?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let from = (chrono::Local::now().date_naive()
        - chrono::Duration::days(BUDGET_LOOKBACK_DAYS - 1))
    .format("%Y-%m-%d")
    .to_string();
    let parts = crate::commands::with_reader(&state, |store| {
        Ok(store.turn_model_parts(platform.collector_source(), &from))
    })?;
    let pairs = super::calib::sample_count(platform);
    let mut budget = message_budget(
        platform,
        from,
        &parts,
        now_ms,
        super::calib::scale(platform),
        pairs >= super::calib::CALIBRATED_PAIRS,
    );
    if week.unwrap_or(false) {
        let (fit, readings) = {
            let store = sub.0.lock().unwrap_or_else(|e| e.into_inner());
            (
                super::calib::week_scale_from_store(&store, platform),
                store.quota_readings(platform, "7d", i64::MIN / 2, i64::MAX / 2),
            )
        };
        budget.attach_week(platform, &parts, &readings, fit, now_ms);
    }
    Ok(budget)
}

/// 一条用户轮（根会话的轮）：起点、代价最大的模型、该模型价目是否可信、整轮代价。
struct RootTurn<'a> {
    started_at: i64,
    model: &'a str,
    known: bool,
    usd: f64,
}

/// 模型键 → 展示名。
type Names<'a> = std::collections::HashMap<&'a str, String>;

/// 切片折价 → 用户轮列表 + 子会话开销倍率 + 模型展示名。
fn root_turns(
    platform: Platform,
    parts: &[crate::collector::store::TurnModelPart],
) -> (Vec<RootTurn<'_>>, f64, Names<'_>) {
    use std::collections::{BTreeMap, HashMap};

    let (mut all_usd, mut root_usd) = (0.0_f64, 0.0_f64);
    // （会话, 轮) → （轮起点, 模型 → （代价, 价目可信))
    let mut turns: BTreeMap<(&str, i64), (i64, HashMap<&str, (f64, bool)>)> = BTreeMap::new();
    let mut names: Names = HashMap::new();
    for p in parts {
        let priced = cost::priced_at(platform, &p.model_key, p.started_at.div_euclid(1000));
        let usd = priced.usd_of(&cost::Tokens {
            input: p.tokens[0],
            output: p.tokens[1],
            cache_read: p.tokens[2],
            cache_write: p.tokens[3],
        });
        all_usd += usd;
        if !p.root {
            continue;
        }
        root_usd += usd;
        names.entry(p.model_key.as_str()).or_insert(priced.display_name);
        let t = turns
            .entry((p.session_id.as_str(), p.turn_seq))
            .or_insert_with(|| (p.started_at, HashMap::new()));
        let e = t.1.entry(p.model_key.as_str()).or_insert((0.0, priced.known));
        e.0 += usd;
    }
    let overhead = if root_usd > 0.0 { (all_usd / root_usd).max(1.0) } else { 1.0 };
    let list = turns
        .into_values()
        .filter_map(|(started_at, models)| {
            let usd: f64 = models.values().map(|(u, _)| u).sum();
            let (model, (_, known)) = models
                .into_iter()
                .max_by(|a, b| a.1 .0.total_cmp(&b.1 .0).then(b.0.cmp(a.0)))?;
            Some(RootTurn { started_at, model, known, usd })
        })
        .collect();
    (list, overhead, names)
}

/// [`get_message_budget`] 的本体（纯函数,测试直接喂切片）。
pub(super) fn message_budget(
    platform: Platform,
    from: String,
    parts: &[crate::collector::store::TurnModelPart],
    now_ms: i64,
    scale: f64,
    calibrated: bool,
) -> MessageBudget {
    use std::collections::BTreeMap;

    let (turns, overhead, names) = root_turns(platform, parts);

    // 零 token 的轮（秒中止 / 纯本地命令）不是「一条消息的代价」;主模型价目不可信
    // （回落 / 路由标签）的轮不进样本
    let recent_ms = now_ms - MAIN_MODEL_DAYS * 86_400_000;
    let mut per_model: BTreeMap<&str, (Vec<f64>, usize)> = BTreeMap::new();
    for t in turns.iter().filter(|t| t.usd > 0.0 && t.known) {
        let e = per_model.entry(t.model).or_default();
        e.0.push(t.usd);
        if t.started_at >= recent_ms {
            e.1 += 1;
        }
    }

    // 够样本的模型出中位数
    let mut rows: Vec<(MessageCostRow, usize)> = per_model
        .into_iter()
        .filter(|(_, (v, _))| v.len() >= BUDGET_MIN_TURNS)
        .map(|(model, (mut v, recent))| {
            v.sort_by(|a, b| a.total_cmp(b));
            let n = v.len();
            let median = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 };
            (
                MessageCostRow {
                    model_key: model.to_string(),
                    display_name: names.get(model).cloned().unwrap_or_else(|| model.to_string()),
                    turns: n,
                    median_usd: median,
                    pct_per_turn: median * overhead * scale,
                    pct_per_turn_week: None,
                },
                recent,
            )
        })
        .collect();
    rows.sort_by(|a, b| b.0.turns.cmp(&a.0.turns).then(a.0.model_key.cmp(&b.0.model_key)));
    let main_model = rows
        .iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(a.0.turns.cmp(&b.0.turns)).then(b.0.model_key.cmp(&a.0.model_key)))
        .map(|(r, _)| r.model_key.clone());

    MessageBudget {
        platform,
        from,
        scale,
        calibrated,
        overhead,
        main_model,
        rows: rows.into_iter().map(|(r, _)| r).collect(),
        week: None,
    }
}

impl MessageBudget {
    /// 挂上周窗口那一半：周系数换算每行的周代价,读数历史切出当前账号的窗口与重置,
    /// 再数窗口里这个账号发了多少条。
    pub(super) fn attach_week(
        &mut self,
        platform: Platform,
        parts: &[crate::collector::store::TurnModelPart],
        readings: &[super::model::QuotaReading],
        fit: Option<(f64, u32)>,
        now_ms: i64,
    ) {
        use std::collections::HashMap;

        let (scale, pairs) = fit.map_or((None, 0), |(s, n)| (Some(s), n));
        for r in &mut self.rows {
            r.pct_per_turn_week = scale.map(|s| r.median_usd * self.overhead * s);
        }
        let w = week_window(readings, now_ms.div_euclid(1000));
        let (turns, _, names) = root_turns(platform, parts);
        let mut counts: HashMap<&str, usize> = HashMap::new();
        if let Some(start) = w.start {
            for t in &turns {
                let at = t.started_at.div_euclid(1000);
                if at >= start && w.account_at(at) == w.account {
                    *counts.entry(t.model).or_default() += 1;
                }
            }
        }
        let mut list: Vec<WeekTurns> = counts
            .into_iter()
            .map(|(m, n)| WeekTurns {
                model_key: m.to_string(),
                display_name: names.get(m).cloned().unwrap_or_else(|| m.to_string()),
                turns: n,
            })
            .collect();
        list.sort_by(|a, b| b.turns.cmp(&a.turns).then(a.model_key.cmp(&b.model_key)));
        self.week = Some(WeekForecast {
            scale,
            pairs,
            total_turns: list.iter().map(|t| t.turns).sum(),
            turns: list,
            account: w.account,
            start: w.start,
            resets_at: w.resets_at,
            resets: w.resets,
        });
    }
}

/// 从周读数历史里切出的当前账号窗口。
pub(super) struct WeekWindow {
    pub account: String,
    pub start: Option<i64>,
    pub resets_at: Option<i64>,
    pub resets: Vec<WeekReset>,
    /// （时刻, 账号) 升序——把一条用户轮归给当时在用的账号。
    timeline: Vec<(i64, String)>,
}

impl WeekWindow {
    /// 时刻 `t` 在用的账号：该时刻及之前最近一条读数的账号;更早没有读数就取第一条。
    fn account_at(&self, t: i64) -> &str {
        let i = self.timeline.partition_point(|(at, _)| *at <= t);
        self.timeline.get(i.saturating_sub(1)).map_or("", |(_, a)| a.as_str())
    }
}

/// 读数掉回这么低才算「清零」（整数刻度 + 重置后几分钟里可能已经用掉一两格）。
const RESET_FLOOR_PCT: f64 = 5.0;
/// 且至少掉了这么多——从 3% 掉到 0% 分不清是重置还是噪声。
const RESET_DROP_PCT: f64 = 5.0;
/// 清零后这段时间内又回到原水位 = 不是重置,是读数倒退（Codex rollout 里并发调用
/// 交错写入会出现一次「旧值」,见的 rollout 驱动取数）。
const RESET_BOUNCE_SECS: i64 = 3600;
/// 窗尾前移超过这么多 = 进了新窗口（同一窗口内窗尾只会有几分钟的抖动）。
const RESET_TAIL_JUMP_SECS: i64 = 86_400;
/// 重置早于上一窗口申报时刻超过这么多才算「提前重置」（读数时刻本身有几分钟的粒度）。
const EARLY_SLACK_SECS: i64 = 3600;

/// 周读数 → 当前账号的窗口与重置历史（纯函数）。
///
/// **周窗口不一定正好 7 天**：平台会主动提前重置。所以窗口起点不从窗尾倒推 7 天了事,
/// 而是看读数里的重置痕迹：
/// - 窗尾（`resets_at`）前移超过一天,或
/// - 读数从 ≥ 5 点掉回 ≤ 5%,且之后一小时内没有弹回原水位。
///
/// 重置时刻取「上一条读数 → 这一条」之间最可信的那一点：上一窗口申报的窗尾若落在
/// 两条读数之间就取它（按期重置,精确）;否则取 `max（上一条读数, 新窗尾 − 7 天)`
/// （提前重置时新窗尾 − 7 天就是重置那一刻;读数稀疏时退回上一条读数,即「最晚不早于」）。
/// 当前窗口起点另外不早于「当前窗尾 − 7 天」。窗尾已过而新窗口还没读到时,窗尾本身
/// 记作一次按期重置,窗口从那里起算。
///
/// 账号 = 读数的套餐名（空名沿用前一条的）。同名套餐的两个账号交替使用时,读数带窗尾的
/// 能认出来：候选重置之后若旧窗尾（还没到期）又被申报,旧窗口就还活着;或者「新」窗尾
/// 早先就被申报过,那个窗口本来就在——两种都是换账号,不算重置。
/// **没有窗尾的读数认不出**——那一段里来回跳仍可能被看成重置。
pub(super) fn week_window(readings: &[super::model::QuotaReading], now: i64) -> WeekWindow {
    let mut rs: Vec<&super::model::QuotaReading> = readings.iter().collect();
    rs.sort_by_key(|r| r.t);
    // 账号：空名沿用前一条;开头的空名取第一个有名字的
    let mut last = rs
        .iter()
        .find(|r| !r.plan_type.is_empty())
        .map(|r| r.plan_type.clone())
        .unwrap_or_default();
    let timeline: Vec<(i64, String)> = rs
        .iter()
        .map(|r| {
            if !r.plan_type.is_empty() {
                last = r.plan_type.clone();
            }
            (r.t, last.clone())
        })
        .collect();
    let Some(account) = timeline.last().map(|(_, a)| a.clone()) else {
        return WeekWindow { account: String::new(), start: None, resets_at: None, resets: vec![], timeline };
    };
    let mine: Vec<&super::model::QuotaReading> = rs
        .iter()
        .zip(&timeline)
        .filter(|(_, (_, a))| *a == account)
        .map(|(r, _)| *r)
        .collect();

    let mut resets: Vec<WeekReset> = Vec::new();
    let mut end: Option<i64> = None; // 当前窗口最新申报的窗尾
    // 最近一次重置之后的第一条读数时刻（新窗尾第一次读到时回头校准重置时刻用）
    let mut after_reset: Option<i64> = None;
    for (i, r) in mine.iter().enumerate() {
        if i > 0 {
            let prev = mine[i - 1];
            let tail_jump = matches!((r.resets_at, end), (Some(a), Some(b)) if a - b > RESET_TAIL_JUMP_SECS);
            // 新窗尾 − 7 天比上一条读数还晚 ⇒ 上一条在更早的窗口里（账号隔了几天才又用,
            // 回来时读数不一定掉到底——新窗口里可能已经用了一截）
            let stale_prev = r.resets_at.is_some_and(|a| prev.t < a - WEEK_SECS);
            let dropped = prev.used_percent >= RESET_DROP_PCT
                && r.used_percent <= RESET_FLOOR_PCT
                && prev.used_percent - r.used_percent >= RESET_DROP_PCT
                && !mine[i + 1..]
                    .iter()
                    .take_while(|x| x.t <= r.t + RESET_BOUNCE_SECS)
                    .any(|x| x.used_percent >= prev.used_percent - 1.0);
            // 旧窗口后来又出现了（之后某条读数申报的窗尾仍是旧窗尾,且旧窗尾还没到）
            // ⇒ 旧窗口没被重置,这是同名套餐的另一个账号插了进来
            let old_alive = end.is_some_and(|e| {
                mine[i + 1..]
                    .iter()
                    .take_while(|x| x.t < e)
                    .any(|x| x.resets_at.is_some_and(|a| (a - e).abs() <= EARLY_SLACK_SECS))
            });
            // 「新」窗尾早就被申报过 ⇒ 那个窗口本来就在,是切回了另一个账号
            let seen_before = r.resets_at.is_some_and(|a| {
                mine[..i - 1]
                    .iter()
                    .any(|x| x.resets_at.is_some_and(|b| (a - b).abs() <= EARLY_SLACK_SECS))
            });
            if (tail_jump || stale_prev || dropped) && !old_alive && !seen_before {
                let t = match end {
                    Some(e) if e > prev.t && e <= r.t => e,
                    _ => r.resets_at.map_or(prev.t, |a| prev.t.max(a - WEEK_SECS)),
                };
                // 「提前」只在有申报窗尾可比时判：没有窗尾时,同名套餐的两个账号来回切
                // 与真的提前重置在读数上长得一样,硬判只会造出假的「提前」
                let early = end.is_some_and(|e| t < e - EARLY_SLACK_SECS);
                resets.push(WeekReset { t, early });
                after_reset = Some(r.t);
                end = None;
            }
        }
        if let Some(a) = r.resets_at {
            // 重置之后第一次读到新窗尾：重置时刻不早于「新窗尾 − 7 天」,也不晚于重置后的
            // 第一条读数——读数稀疏时,这一步把「最晚不早于上一条读数」收紧到真实时刻附近
            if end.is_none() {
                if let (Some(last), Some(first)) = (resets.last_mut(), after_reset) {
                    last.t = last.t.max(a - WEEK_SECS).min(first);
                }
            }
            end = Some(a);
        }
    }
    // 起点：最近一次重置,但不早于「当前窗尾 − 7 天」;读数里没有重置痕迹时窗口
    // 早在第一条读数之前就开始了 ⇒ 有窗尾就取窗尾 − 7 天,没有才退回第一条读数
    let mut start = match (resets.last(), end) {
        (Some(l), Some(e)) => Some(l.t.max(e - WEEK_SECS)),
        (Some(l), None) => Some(l.t),
        (None, Some(e)) => Some(e - WEEK_SECS),
        (None, None) => mine.first().map(|r| r.t),
    };
    let mut resets_at = end;
    // 窗尾已过、新窗口还没读到：窗尾就是一次按期重置
    if let Some(e) = end {
        if now >= e {
            resets.push(WeekReset { t: e, early: false });
            start = Some(e);
            resets_at = None;
        }
    }
    WeekWindow { account, start, resets_at, resets, timeline }
}

// ---------- 读数 ----------

/// 归一化读数序列（`[from, to]` 闭区间,unix 秒;`kind` 省略 = 该平台全部窗口种类）。
///
/// 一行 = 一个窗口在某一时刻的一次读数。**年度曲线不要走这条**——那是几千行的量,
/// 日级汇总已经把它算好了（`get_quota_days`）。这条是给「某一天里发生了什么」
/// 这类下钻用的。
#[tauri::command]
pub fn get_quota_readings(
    platform: String,
    kind: Option<String>,
    from: i64,
    to: i64,
    state: State<'_, SubscriptionReader>,
) -> Result<Vec<QuotaReading>, String> {
    let platform = platform_of(&platform)?;
    let store = state.0.lock().unwrap_or_else(|e| e.into_inner());
    let kinds = match kind {
        Some(k) => vec![k],
        None => store.quota_kinds(platform),
    };
    let mut out: Vec<QuotaReading> = kinds
        .iter()
        .flat_map(|k| store.quota_readings(platform, k, from, to))
        .collect();
    out.sort_by(|a, b| a.t.cmp(&b.t).then(a.kind.cmp(&b.kind)));
    Ok(out)
}

/// 日级汇总（`[from_day, to_day]` 闭区间,本地日期 `YYYY-MM-DD`;`kind` 省略 = 全部种类）。
///
/// `gain_pct` 是**下界不是账单**——滚动窗口里消耗与过期同时发生;
/// 要讲「这天用了多少」必须同时看 `carry_secs`,它说明这笔涨幅是跨多久攒出来的。
#[tauri::command]
pub fn get_quota_days(
    platform: String,
    kind: Option<String>,
    from: String,
    to: String,
    state: State<'_, SubscriptionReader>,
) -> Result<Vec<QuotaDay>, String> {
    let platform = platform_of(&platform)?;
    let store = state.0.lock().unwrap_or_else(|e| e.into_inner());
    let kinds = match kind {
        Some(k) => vec![k],
        None => store.quota_kinds(platform),
    };
    let mut out: Vec<QuotaDay> = kinds
        .iter()
        .flat_map(|k| store.quota_days(platform, k, &from, &to))
        .collect();
    out.sort_by(|a, b| a.day.cmp(&b.day).then(a.kind.cmp(&b.kind)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本地小时起点：相邻两小时正好差 3600 秒,且 0 点就是该日零点。
    #[test]
    fn local_hour_ts_is_hourly() {
        let a = local_hour_ts("2026-09-19", 0).unwrap();
        let b = local_hour_ts("2026-09-19", 1).unwrap();
        assert_eq!(b - a, 3600);
        assert_eq!(local_hour_ts("2026-09-19", 23).unwrap() - a, 23 * 3600);
        assert!(local_hour_ts("不是日期", 0).is_none());
    }

    /// `get_price_at` 的两段夹具：分界前后各取各的,且每个键只出一行。
    #[test]
    fn price_at_picks_one_row_per_key() {
        const T: i64 = 1_800_000_000;
        let rows = price::two_segment_fixture(T);
        price::with_rows(&rows, || {
            let before = price::rows_at(Platform::Claude, T - 1);
            let after = price::rows_at(Platform::Claude, T + 1);
            assert_eq!(before.len(), 2, "两个键各一行（降价的那个不许出两行）");
            assert_eq!(after.len(), 2);
            let pick = |v: &[PriceRow], k: &str| {
                v.iter().find(|r| r.match_key == k).unwrap().usd_input
            };
            assert_eq!(pick(&before, "claude-widget-1"), 10.0);
            assert_eq!(pick(&after, "claude-widget-1"), 1.0);
            assert_eq!(pick(&before, "claude-steady-1"), pick(&after, "claude-steady-1"));
            // 全量读口仍然出三行（两段 + 对照）
            assert_eq!(price::rows_for(Platform::Claude).len(), 3);
        });
    }

    /// 取价入口唯一：`priced_at` 给的单价折出来的数 == `cost_of` 的数,
    /// 且 `codex-auto-review` 在两边都判不可信。
    #[test]
    fn priced_at_agrees_with_cost_of() {
        let at = 1_790_000_000;
        let t = cost::Tokens { input: 1_000_000, output: 500_000, cache_read: 2_000_000, cache_write: 0 };
        for (p, model) in [
            (Platform::Claude, "claude-opus-5"),
            (Platform::Codex, "gpt-5.6-sol"),
            (Platform::Codex, "codex-auto-review"),
            (Platform::Codex, "完全没见过的模型"),
        ] {
            let priced = cost::priced_at(p, model, at);
            let (usd, known) = cost::cost_of(p, model, &t, at);
            assert_eq!(priced.usd_of(&t), usd, "{model} 两条路算出来必须是同一个数");
            assert_eq!(priced.known, known, "{model} 可信度判定必须一致");
        }
        assert!(!cost::priced_at(Platform::Codex, "codex-auto-review", at).known);
        assert_eq!(
            cost::priced_at(Platform::Codex, "codex-auto-review", at).match_key.as_deref(),
            Some("gpt-5-6-luna"),
            "路由标签要说明自己折的是谁的价"
        );
        assert_eq!(cost::priced_at(Platform::Codex, "没见过", at).match_key, None, "回落不假称命中");
    }

    // ---------- get_model_usage 本体 ----------

    /// 造一个只有指定几笔用量的内存采集库。
    fn store_with(
        rows: &[(&str, u8, &str, [i64; 4], i64)],
    ) -> crate::collector::store::Store {
        use crate::collector::store::{Batch, Store, Tokens};
        let mut store = Store::open_in_memory().expect("in-memory store");
        let mut batch = Batch::default();
        for (day, hour, model, t, turns) in rows {
            batch.add_usage(
                day,
                Some(*hour),
                "claude-code",
                model,
                Tokens {
                    input: t[0],
                    output: t[1],
                    total: t[0] + t[1],
                    cache_read: t[2],
                    cache_write: t[3],
                },
                *turns,
            );
        }
        store.commit("test", &batch).expect("commit");
        store
    }

    /// 本地日 `day` 的第 `hour` 小时落在哪个 unix 秒（夹具用,与被测同一条换算）。
    fn ts(day: &str, hour: u8) -> i64 {
        local_hour_ts(day, hour).unwrap()
    }

    /// 没降过价的模型：一个模型一段,合计 = 段和,区间省略 = 全部历史。
    #[test]
    fn usage_rolls_up_into_one_segment_when_the_price_never_moved() {
        let store = store_with(&[
            ("2026-09-10", 9, "claude-steady-1", [1_000_000, 0, 0, 0], 3),
            ("2026-09-11", 14, "claude-steady-1", [0, 1_000_000, 0, 0], 2),
        ]);
        let rows = price::two_segment_fixture(ts("2026-09-20", 0));
        let got = price::with_rows(&rows, || {
            model_usage(&store, Platform::Claude, None, None)
        });
        assert_eq!((got.from.as_str(), got.to.as_str()), ("2026-09-10", "2026-09-11"), "区间省略 = 全部历史");
        assert_eq!(got.rows.len(), 1);
        let r = &got.rows[0];
        assert_eq!(r.segments.len(), 1, "没降过价就只有一段");
        assert_eq!(r.requests, 5, "轮次按模型合计");
        assert_eq!(r.input_tokens, 1_000_000);
        assert_eq!(r.output_tokens, 1_000_000);
        // 夹具：steady 输入 $3/Mtok、输出 ×5 = $15/Mtok
        assert!((r.usd - 18.0).abs() < 1e-9, "{}", r.usd);
        assert!((got.usd_total - 18.0).abs() < 1e-9);
        assert_eq!(got.usd_unknown, 0.0, "命中价目 ⇒ 没有不可信的那部分");
        assert!(got.usd_unknown.is_sign_positive(), "空求和不许出负零");
        assert!(r.known);
    }

    /// 模型在区间中间被降价 ⇒ 分成两段,各按各自的价算,
    /// 而同区间里没降价的模型一段都不多。
    #[test]
    fn a_price_drop_splits_that_model_and_leaves_the_others_alone() {
        // 降价时刻 = 09-11 本地 00:00 ⇒ 09-10 的用量按旧价、09-11 的按新价
        let t = ts("2026-09-11", 0);
        let store = store_with(&[
            ("2026-09-10", 9, "claude-widget-1", [1_000_000, 0, 0, 0], 1),
            ("2026-09-11", 9, "claude-widget-1", [1_000_000, 0, 0, 0], 1),
            ("2026-09-10", 9, "claude-steady-1", [1_000_000, 0, 0, 0], 1),
            ("2026-09-11", 9, "claude-steady-1", [1_000_000, 0, 0, 0], 1),
        ]);
        let rows = price::two_segment_fixture(t);
        let got = price::with_rows(&rows, || {
            model_usage(&store, Platform::Claude, None, None)
        });
        let widget = got.rows.iter().find(|r| r.model_key == "claude-widget-1").unwrap();
        let steady = got.rows.iter().find(|r| r.model_key == "claude-steady-1").unwrap();
        assert_eq!(widget.segments.len(), 2, "跨降价的模型切成两段");
        assert_eq!(steady.segments.len(), 1, "没降价的模型一段都不多");
        // widget: 旧价 $10/Mtok + 新价 $1/Mtok
        assert_eq!(widget.segments[0].usd_input, 10.0);
        assert_eq!(widget.segments[1].usd_input, 1.0);
        assert_eq!(widget.segments[1].effective_from, Some(t));
        assert!((widget.segments[0].usd - 10.0).abs() < 1e-9);
        assert!((widget.segments[1].usd - 1.0).abs() < 1e-9);
        assert!((widget.usd - 11.0).abs() < 1e-9, "合计 = 各段各按各自单价再相加");
        assert!((steady.usd - 6.0).abs() < 1e-9, "对照模型两天都按 $3/Mtok");
        assert_eq!(widget.requests, 2, "轮次不切段,整区间合计");
        // 排序：贵的在前
        assert_eq!(got.rows[0].model_key, "claude-widget-1");
    }

    /// 日区间是闭区间,越界的那天不进来。
    #[test]
    fn the_day_range_is_inclusive_on_both_ends() {
        let store = store_with(&[
            ("2026-09-09", 9, "claude-steady-1", [1_000_000, 0, 0, 0], 1),
            ("2026-09-10", 9, "claude-steady-1", [1_000_000, 0, 0, 0], 1),
            ("2026-09-11", 9, "claude-steady-1", [1_000_000, 0, 0, 0], 1),
        ]);
        let rows = price::two_segment_fixture(ts("2026-09-20", 0));
        let got = price::with_rows(&rows, || {
            model_usage(
                &store,
                Platform::Claude,
                Some("2026-09-10".into()),
                Some("2026-09-10".into()),
            )
        });
        assert_eq!(got.rows[0].input_tokens, 1_000_000, "只有中间那天");
        assert_eq!(got.rows[0].requests, 1);
    }

    /// 没有本机用量的平台给空结果,不报错（新装机就是这个样子）。
    #[test]
    fn a_platform_with_no_local_usage_is_empty_not_an_error() {
        let store = store_with(&[("2026-09-10", 9, "claude-steady-1", [1_000, 0, 0, 0], 1)]);
        let got = model_usage(&store, Platform::Codex, None, None);
        assert!(got.rows.is_empty());
        assert_eq!(got.usd_total, 0.0);
        assert!(got.usd_total.is_sign_positive(), "空求和不许出负零");
        assert!(got.usd_unknown.is_sign_positive(), "空求和不许出负零");
        assert_eq!((got.from.as_str(), got.to.as_str()), ("", ""));
    }

    /// 认不出的模型走回落价,且**如实标成不可信**并计进 `usd_unknown`。
    #[test]
    fn unknown_models_fall_back_and_say_so() {
        let store = store_with(&[("2026-09-10", 9, "claude-没见过的代号", [1_000_000, 0, 0, 0], 1)]);
        let rows = price::two_segment_fixture(ts("2026-09-20", 0));
        let got = price::with_rows(&rows, || {
            model_usage(&store, Platform::Claude, None, None)
        });
        let r = &got.rows[0];
        assert!(!r.known);
        assert_eq!(r.segments[0].match_key, None);
        assert_eq!(r.segments[0].effective_from, None);
        assert_eq!(r.display_name, "claude-没见过的代号", "没命中就回模型键本身");
        assert_eq!(got.usd_unknown, got.usd_total, "整份都是估的");
        assert!((got.usd_total - 2.0).abs() < 1e-9, "Claude 回落 = Sonnet 5 输入价 $2/Mtok");
    }

    // ---------- 剩余消息数 ----------

    use crate::collector::store::TurnModelPart;

    /// 一个切片：`seq` 轮,`root` = 是否根会话,只给输入 token（夹具价目 steady = $3/Mtok）。
    fn part(session: &str, seq: i64, root: bool, model: &str, input: i64, at_ms: i64) -> TurnModelPart {
        TurnModelPart {
            session_id: session.into(),
            turn_seq: seq,
            started_at: at_ms,
            root,
            model_key: model.into(),
            tokens: [input, 0, 0, 0],
        }
    }

    /// 中位数而不是均值：一个巨型轮不把每轮代价拖上去;样本不够的模型不出行。
    #[test]
    fn budget_takes_the_median_turn_and_needs_enough_turns() {
        const NOW: i64 = 1_800_000_000_000;
        let mut parts: Vec<TurnModelPart> = [1, 1, 1, 1, 100]
            .iter()
            .enumerate()
            .map(|(i, m)| part("a", i as i64, true, "claude-steady-1", m * 1_000_000, NOW - 1000))
            .collect();
        // 样本不够（4 轮）的模型
        parts.extend((0..4).map(|i| part("b", i, true, "claude-widget-1", 1_000_000, NOW - 1000)));
        let rows = price::two_segment_fixture(NOW / 1000 + 86_400);
        let got = price::with_rows(&rows, || {
            message_budget(Platform::Claude, "x".into(), &parts, NOW, 0.5, true)
        });
        assert_eq!(got.rows.len(), 1, "4 轮不够出估计");
        let r = &got.rows[0];
        assert_eq!(r.turns, 5);
        assert!((r.median_usd - 3.0).abs() < 1e-9, "中位数 = 一轮 1Mtok × $3,{}", r.median_usd);
        assert!((r.pct_per_turn - 1.5).abs() < 1e-9, "× scale 0.5");
        assert_eq!(got.main_model.as_deref(), Some("claude-steady-1"));
        assert_eq!(got.overhead, 1.0, "没有子会话就没有开销");
    }

    /// 子会话不算轮,但它的代价按比例摊进每一轮;一轮混用模型时归给代价最大的那个。
    #[test]
    fn budget_spreads_subagent_cost_and_assigns_mixed_turns_to_the_dominant_model() {
        const NOW: i64 = 1_800_000_000_000;
        let mut parts = Vec::new();
        for i in 0..5 {
            parts.push(part("root", i, true, "claude-steady-1", 2_000_000, NOW));
            parts.push(part("root", i, true, "claude-widget-1", 100_000, NOW)); // 顺带用了一点
            parts.push(part("sub", i, false, "claude-steady-1", 2_100_000, NOW)); // 子代理 = 同量
        }
        let rows = price::two_segment_fixture(NOW / 1000 + 86_400);
        let got = price::with_rows(&rows, || {
            message_budget(Platform::Claude, "x".into(), &parts, NOW, 1.0, true)
        });
        assert_eq!(got.rows.len(), 1, "widget 从没当过主模型");
        assert_eq!(got.rows[0].model_key, "claude-steady-1");
        assert_eq!(got.rows[0].turns, 5, "子会话的切片不算轮");
        let turn = 2.0 * 3.0 + 0.1 * 10.0; // steady $3 + widget 旧价 $10
        assert!((got.rows[0].median_usd - turn).abs() < 1e-9, "{}", got.rows[0].median_usd);
        let overhead = (turn + 2.1 * 3.0) / turn;
        assert!((got.overhead - overhead).abs() < 1e-9);
        assert!((got.rows[0].pct_per_turn - turn * overhead).abs() < 1e-9);
    }

    /// 价目不可信的主模型（回落 / 路由标签）与零代价的轮都不进样本。
    #[test]
    fn budget_skips_untrusted_models_and_empty_turns() {
        const NOW: i64 = 1_800_000_000_000;
        let mut parts: Vec<TurnModelPart> =
            (0..6).map(|i| part("a", i, true, "claude-没见过的代号", 1_000_000, NOW)).collect();
        parts.extend((10..16).map(|i| part("a", i, true, "claude-steady-1", 0, NOW)));
        let rows = price::two_segment_fixture(NOW / 1000 + 86_400);
        let got = price::with_rows(&rows, || {
            message_budget(Platform::Claude, "x".into(), &parts, NOW, 1.0, false)
        });
        assert!(got.rows.is_empty());
        assert_eq!(got.main_model, None);
        assert!(!got.calibrated);
    }

    /// 主力模型看最近 7 天,不看整个取样窗口的总轮数。
    #[test]
    fn main_model_follows_the_last_week() {
        const NOW: i64 = 1_800_000_000_000;
        const OLD: i64 = NOW - 20 * 86_400_000;
        let mut parts: Vec<TurnModelPart> =
            (0..20).map(|i| part("old", i, true, "claude-widget-1", 1_000_000, OLD)).collect();
        parts.extend((0..6).map(|i| part("new", i, true, "claude-steady-1", 1_000_000, NOW)));
        let rows = price::two_segment_fixture(NOW / 1000 + 86_400);
        let got = price::with_rows(&rows, || {
            message_budget(Platform::Claude, "x".into(), &parts, NOW, 1.0, true)
        });
        assert_eq!(got.rows[0].model_key, "claude-widget-1", "行按总轮数排");
        assert_eq!(got.main_model.as_deref(), Some("claude-steady-1"), "主力看最近一周");
    }

    // ---------- 周窗口 ----------

    use crate::subscription::model::SnapshotSource;

    const H: i64 = 3600;
    const D: i64 = 86_400;

    fn rd(t: i64, used: f64, resets_at: Option<i64>, plan: &str) -> QuotaReading {
        QuotaReading {
            t,
            kind: "7d".into(),
            used_percent: used,
            resets_at,
            plan_type: plan.into(),
            source: SnapshotSource::Api,
        }
    }

    /// 按期重置：上一窗尾落在两条读数之间 ⇒ 重置时刻就是那个窗尾,不算提前。
    #[test]
    fn a_scheduled_reset_lands_on_the_announced_tail() {
        const T0: i64 = 1_800_000_000;
        let end1 = T0 + 2 * D;
        let v = vec![
            rd(T0, 40.0, Some(end1), "max"),
            rd(T0 + D, 60.0, Some(end1), "max"),
            rd(end1 + 2 * H, 1.0, Some(end1 + 7 * D), "max"),
        ];
        let w = week_window(&v, end1 + 3 * H);
        assert_eq!(w.resets, vec![WeekReset { t: end1, early: false }]);
        assert_eq!(w.start, Some(end1));
        assert_eq!(w.resets_at, Some(end1 + 7 * D));
    }

    /// 提前重置：窗尾还没到读数就清零了,新窗尾 = 重置那刻 + 7 天 ⇒ 起点与「提前」都认得出。
    #[test]
    fn an_early_reset_is_flagged_and_starts_the_window() {
        const T0: i64 = 1_800_000_000;
        let end1 = T0 + 3 * D;
        let reset = T0 + D + 30 * 60;
        let v = vec![
            rd(T0, 40.0, Some(end1), "max"),
            rd(T0 + D, 55.0, Some(end1), "max"),
            rd(T0 + D + H, 0.0, Some(reset + 7 * D), "max"),
        ];
        let w = week_window(&v, T0 + D + 2 * H);
        assert_eq!(w.resets.len(), 1);
        assert!(w.resets[0].early, "早于申报窗尾两天 = 提前重置");
        assert_eq!(w.resets[0].t, reset, "新窗尾 − 7 天");
        assert_eq!(w.start, Some(reset));
    }

    /// 读数倒退一次又弹回（rollout 并发交错写入）不算重置;没有窗尾的来源也能靠清零认出重置。
    #[test]
    fn a_bounce_is_not_a_reset_but_a_sustained_drop_is() {
        const T0: i64 = 1_800_000_000;
        let v = vec![
            rd(T0, 30.0, None, "plus"),
            rd(T0 + 60, 0.0, None, "plus"),
            rd(T0 + 120, 31.0, None, "plus"),
            rd(T0 + 5 * D, 70.0, None, "plus"),
            rd(T0 + 5 * D + 600, 0.0, None, "plus"),
            rd(T0 + 5 * D + 2 * H, 2.0, None, "plus"),
        ];
        let w = week_window(&v, T0 + 6 * D);
        assert_eq!(w.resets.len(), 1, "只有持续的那次清零");
        assert_eq!(w.resets[0].t, T0 + 5 * D, "没有窗尾可用 ⇒ 最晚不早于上一条读数");
        assert!(!w.resets[0].early, "隔得太远没法判提前");
        assert_eq!(w.start, Some(T0 + 5 * D));
    }

    /// 换账号：窗口、重置只看当前账号;用户轮按当时在用的账号归属。
    #[test]
    fn accounts_are_kept_apart() {
        const T0: i64 = 1_800_000_000;
        let v = vec![
            rd(T0, 50.0, Some(T0 + 6 * D), "edu"),
            rd(T0 + H, 10.0, Some(T0 + 4 * D), "plus"), // 换到 plus:读数掉了,但不是重置
            rd(T0 + 2 * H, 55.0, Some(T0 + 6 * D), "edu"),
            rd(T0 + 3 * H, 12.0, Some(T0 + 4 * D), "plus"),
        ];
        let w = week_window(&v, T0 + 4 * H);
        assert_eq!(w.account, "plus");
        assert!(w.resets.is_empty(), "账号来回切不是重置");
        assert_eq!(w.start, Some(T0 + 4 * D - 7 * D), "没有重置痕迹 ⇒ 窗尾 − 7 天（窗口早于第一条读数就开始了）");
        assert_eq!(w.account_at(T0 + 30 * 60), "edu");
        assert_eq!(w.account_at(T0 + H + 1), "plus");
        assert_eq!(w.account_at(T0 - D), "edu", "更早没有读数就取第一条");
    }

    /// 同名套餐的两个账号交替：各自申报各自的窗尾,旧窗尾之后又出现 ⇒ 不是重置。
    #[test]
    fn two_accounts_on_one_plan_are_not_resets() {
        const T0: i64 = 1_800_000_000;
        let (ea, eb) = (T0 + 5 * D, T0 + 2 * D);
        let v = vec![
            rd(T0, 40.0, Some(ea), "plus"),
            rd(T0 + H, 3.0, Some(eb), "plus"),
            rd(T0 + 2 * H, 41.0, Some(ea), "plus"),
            rd(T0 + 3 * H, 4.0, Some(eb), "plus"),
            rd(T0 + 4 * H, 42.0, Some(ea), "plus"),
        ];
        let w = week_window(&v, T0 + 5 * H);
        assert!(w.resets.is_empty(), "{:?}", w.resets);
        assert_eq!(w.resets_at, Some(ea));
    }

    /// 窗尾已过、新窗口还没读到：窗尾记一次按期重置,窗口从那里起算,窗尾未知。
    #[test]
    fn an_expired_window_resets_at_its_tail() {
        const T0: i64 = 1_800_000_000;
        let v = vec![rd(T0, 80.0, Some(T0 + D), "max")];
        let w = week_window(&v, T0 + 2 * D);
        assert_eq!(w.resets, vec![WeekReset { t: T0 + D, early: false }]);
        assert_eq!(w.start, Some(T0 + D));
        assert_eq!(w.resets_at, None);
    }

    /// 周那一半：每行周代价 = 中位 × 开销 × 周系数;窗口里的轮按模型数,窗口外的不算。
    #[test]
    fn attach_week_counts_turns_since_the_window_start() {
        const NOW: i64 = 1_800_000_000_000;
        let start = NOW / 1000 - D;
        let mut parts: Vec<TurnModelPart> =
            (0..5).map(|i| part("a", i, true, "claude-steady-1", 1_000_000, NOW - 1000)).collect();
        parts.extend((5..8).map(|i| part("a", i, true, "claude-steady-1", 1_000_000, (start - H) * 1000)));
        let rows = price::two_segment_fixture(NOW / 1000 + D);
        let mut b = price::with_rows(&rows, || {
            message_budget(Platform::Claude, "x".into(), &parts, NOW, 1.0, true)
        });
        let readings = vec![rd(start + 60, 3.0, Some(start + 7 * D), "max")];
        price::with_rows(&rows, || b.attach_week(Platform::Claude, &parts, &readings, Some((0.1, 30)), NOW));
        let w = b.week.as_ref().unwrap();
        assert_eq!(w.start, Some(start));
        assert_eq!(w.total_turns, 5, "窗口起点之前的 3 轮不算");
        assert_eq!(w.turns[0].model_key, "claude-steady-1");
        assert!((b.rows[0].pct_per_turn_week.unwrap() - 3.0 * 0.1).abs() < 1e-9);
        // 周系数样本不够 ⇒ 周那一列不给
        let mut b2 = price::with_rows(&rows, || {
            message_budget(Platform::Claude, "x".into(), &parts, NOW, 1.0, true)
        });
        price::with_rows(&rows, || b2.attach_week(Platform::Claude, &parts, &readings, None, NOW));
        assert_eq!(b2.rows[0].pct_per_turn_week, None);
    }
}
