//! 按官方价目折算的「代价」：取数时机估算的第一步。价目本体在 [`super:price`],
//! 本模块只负责折算与价格数据集订号。
//!
//! 限流窗口的消耗大致按**价值**计,不按 token 条数计——最贵的模型 30 秒里产出的 token
//! 不多,但可能已经吃掉可观的额度。所以先把各模型、各种类的 token 折成同一把尺子上的
//! 「代价」,再由 calib.rs 的标定系数换成百分点。
//!
//! **代价的单位是美元当量**：`cost` = 这笔 token 按**官方 API 定价**算出来的美元数。
//! `scale` 的含义是「每 1 美元当量吃掉配额的百分之几」,面板可以直接说「这段时间你消耗了
//! **相当于** $X 的 API 用量」;也不需要一个会随模型世代消失的「基准模型」。
//!
//! > **语义红线（任何面板文案都要遵守）**：它**不是账单**。用户付的是固定订阅月费,
//! > 只能说「**相当于** $X 的 API 用量」「等价价值」,**不能说「你花了 $X」**。
//! > 另：缓存写在部分平台按保留时长（TTL）分档,而采集层只有**一个** `cache_write`
//! > 桶,无法区分 ⇒ 按常用档估算,面板需注明。
//!
//! 未知模型（新代号模型随时会出现）落回回落价目,并标记 `unknown`——这种样本照常参与
//! **触发估算**,但不参与**标定**（价目不可信的样本会污染系数）。

use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex};

use super::model::Platform;
use super::price;

/// **出厂价格数据集的订号**：库里这行的 `cost` 是不是按**当前这份**价格数据算出来的。
/// `price_seed.json` 每次真的变动（新模型 / 官方调价 / 正）就 +1。
///
/// `usage_pair.cost` 是**派生值**,价目一变,旧行按旧尺子量的 `cost` 与新样本一起拟合会把
/// 系数拖偏。原始 token 一直存在 `usage_pair.breakdown` 里,所以订号一升,存量行
/// **就地重算**（`store:recompute_stale_costs`,**每个模型按它在该区间 `t1` 时刻生效的价目**）。
///
/// 与**套餐变更**是两回事：套餐变了 `cost` 没错、是 `scale`（%/美元）变了,那条靠
/// `usage_pair.plan_type` 分代,旧样本留着但不参与当前拟合。
///
/// **改 [`prior_scale`] 不升版**：它不参与 `cost` 的计算（只当拟合的先验与样本准入带的
/// 基准）,存量行不变,下一次 refit 自动用新先验。
pub const WEIGHT_VERSION: u32 = 3;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tokens {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

impl Tokens {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// 官方价目以 USD / **Mtok** 发布 ⇒ 折算时除以百万。
const PER_MTOK: f64 = 1_000_000.0;

/// 匹配不上时的回落价目（USD / Mtok,取该平台的**基准模型**：Claude = Sonnet 5、
/// Codex = gpt-5）。
///
/// 回落**取基准而不是取当代旗舰**是有意的：估高会让取数更频繁（对限流不利）,估低只是
/// 让首次取数偏晚。这类样本照常参与触发估算,但标记 `unknown`。
///
/// 硬编码而不读库：它是「库里查不到」时的退路。数值与 `price_seed.json` 里这两个键的
/// 首段一致（单测 `fallback_matches_seed` 钉住）。
#[derive(Clone, Copy)]
struct Fallback {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}
const CLAUDE_FALLBACK: Fallback =
    Fallback { input: 2.0, output: 10.0, cache_read: 0.2, cache_write: 2.5 };
const CODEX_FALLBACK: Fallback =
    Fallback { input: 1.25, output: 10.0, cache_read: 0.125, cache_write: 0.0 };

/// Codex 把自动 review 记成 `codex-auto-review`——这**不是模型名而是路由标签**,
/// 真正跑的模型由上游决定,官方没有公布对照表。
///
/// 这里按已知路由（gpt-5.6-luna,依据 ccusage 维护的 `codex-auto-review-fallbacks.json`,
/// 其自述为 best-effort）折价,但**照样标记为不可信**：路由随时会改且无从校验,
/// 靠它主导的样本不该拿去标定系数。
const AUTO_REVIEW: &str = "codex-auto-review";
const AUTO_REVIEW_ROUTES_TO: &str = "gpt-5-6-luna";

/// 一笔 token 明细的**美元当量**。返回 （代价, 该模型的价目是否可信)。
///
/// `at` = 这笔 token 所属区间的时刻（在线路用 `now`,重算与冷启动用样本区间的 `t1`）。
/// 每个模型按它**在该时刻生效**的价目取价,一个区间里的不同模型各取各的时间线,
/// 不存在「区间跨价目段」的问题。
pub fn cost_of(platform: Platform, model_key: &str, t: &Tokens, at: i64) -> (f64, bool) {
    let p = priced_at(platform, model_key, at);
    (p.usd_of(t), p.known)
}

/// 某个模型在某个时刻实际用来计价的那一组单价（含**回落**与**路由折价**之后的结果）。
///
/// 回落与 `codex-auto-review` 的折价都发生在这里,直接查 price 表看不;所以取价只有
/// 这一个入口,[`cost_of`] 与「这段用量按哪条价目算」的查询展示共用它,两条路才给出同一个数。
#[derive(Debug, Clone, PartialEq)]
pub struct Priced {
    /// 命中的价目段起点（`None` = 没命中任何键,四项单价来自回落常量）。
    pub effective_from: Option<i64>,
    /// 命中行的展示名;没命中则原样回 `model_key`。
    pub display_name: String,
    /// 实际命中的 `match_key`（`None` = 回落）。`codex-auto-review` 命中的是它
    /// **路由到**的那个键,所以这个字段同时也说明了折价按谁算。
    pub match_key: Option<String>,
    pub usd_input: f64,
    pub usd_output: f64,
    pub usd_cache_read: f64,
    pub usd_cache_write: f64,
    /// 价目是否可信（回落 / 路由标签 → false,语义见 [`AUTO_REVIEW`]）。
    pub known: bool,
}

impl Priced {
    pub fn usd_of(&self, t: &Tokens) -> f64 {
        (self.usd_input * t.input as f64
            + self.usd_output * t.output as f64
            + self.usd_cache_read * t.cache_read as f64
            + self.usd_cache_write * t.cache_write as f64)
            / PER_MTOK
    }
}

/// 取该模型在该时刻实际生效的一组单价（口径见 [`Priced`]）。
pub fn priced_at(platform: Platform, model_key: &str, at: i64) -> Priced {
    let fb = match platform {
        Platform::Claude => CLAUDE_FALLBACK,
        Platform::Codex => CODEX_FALLBACK,
    };
    let auto_review =
        matches!(platform, Platform::Codex) && price::normalized(model_key).contains(AUTO_REVIEW);
    // 路由标签:折成当前路由的价,但不给 known——理由见 AUTO_REVIEW
    let lookup_key = if auto_review { AUTO_REVIEW_ROUTES_TO } else { model_key };
    match price::price_at(platform, lookup_key, at) {
        Some(r) => Priced {
            effective_from: Some(r.effective_from),
            display_name: r.display_name,
            match_key: Some(r.match_key),
            usd_input: r.usd_input,
            usd_output: r.usd_output,
            usd_cache_read: r.usd_cache_read,
            usd_cache_write: r.usd_cache_write,
            known: !auto_review,
        },
        None => Priced {
            effective_from: None,
            display_name: model_key.to_string(),
            match_key: None,
            usd_input: fb.input,
            usd_output: fb.output,
            usd_cache_read: fb.cache_read,
            usd_cache_write: fb.cache_write,
            known: false,
        },
    }
}

/// 把一份分模型明细折成 `（代价, 其中未知模型的代价)`,每个模型按 `at` 时刻的价目。
/// 与 demand.rs 的逐笔累加等价——重算存量样本走这条,两条路必须算出同一个数。
pub fn cost_of_breakdown(
    platform: Platform,
    models: &BTreeMap<String, [i64; 4]>,
    at: i64,
) -> (f64, f64) {
    let mut total = 0.0;
    let mut unknown = 0.0;
    for (model, v) in models {
        let tokens = Tokens { input: v[0], output: v[1], cache_read: v[2], cache_write: v[3] };
        if tokens.total() == 0 {
            continue;
        }
        let (c, known) = cost_of(platform, model, &tokens, at);
        total += c;
        if !known {
            unknown += c;
        }
    }
    (total, unknown)
}

/// 解析 `usage_pair.breakdown`。
///
/// **规范形状 = 裸 map** `{"model":[输入,输出,缓存读,缓存写]}`（demand.rs 这么写）。
/// 另一种包装形状 `{"src":…,"models":{…}}` 由 store 的就地迁移统一;这里**两种都认**：
/// 重算是派生值可恢复的唯一依靠,不能因为部分行形状不同就恢复不了。
pub fn parse_breakdown(json: &str) -> BTreeMap<String, [i64; 4]> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return BTreeMap::new();
    };
    let inner = v.get("models").unwrap_or(&v);
    serde_json::from_value(inner.clone()).unwrap_or_default()
}

/// 出厂预设标定系数（**百分点 / 美元当量**）,取值来自本机两周历史的离线拟合。
///
/// 好记的量级：**1% 的 Claude Max 5h 额度 ≈ 1 美元 API 当量**,即一个完整 5h 窗口
/// ≈ $100 的 API 等价用量。
///
/// 两平台按美元当量同义,但数值相差近 10 倍（Claude Max ≈ 0.89、Codex plus/edu ≈ 8.9）
/// 是**对的**：它就是「这个套餐一个窗口有多大」,两边月费本来就差一个数量级。
/// 不要统一成一个数,那会让 Codex 低估约 9 倍。
///
/// 只保留两位有效数字：同一平台换档真实系数差几倍,这个数只是「还没有任何本机样本时的
/// 起点」。两条回溯路（Claude 走 bootstrap.rs,Codex 走 codex_rollout.rs）首次启动就会
/// 把它换成,之后它只剩 `calib:PRIOR_WEIGHT` 个样本当量的权重。预设偏小意味着
/// 首次取数**偏晚而非偏频**（对限流更安全）。
///
/// 改这两个数**不需要**升 [`WEIGHT_VERSION`]（不参与 `cost` 的计算）,也不需要升
/// `calib:ADMISSION_RULE_VERSION`（参与拟合的样本集不变,只是混合公式的先验变了）。
///
/// **这两个数各自属于一档套餐**（[`BASE_PLAN_CLAUDE`] / [`BASE_PLAN_CODEX`]）,
/// 别的档由 [`plan_table`] 的倍率表折算。
const PRIOR_CLAUDE: f64 = 0.89;
const PRIOR_CODEX: f64 = 8.9;

/// 出厂预设所在的那一档（倍率 1.0 的基准）。
///
/// - Claude = **Max 5x**（`rateLimitTier = default_claude_max_5x`）;
/// - Codex = **Plus / Edu**（官方对照表里 Standard Business 与 Plus 同额;账户的
///   `plan_type` 在 plus / edu 之间跳,两档同量级,同归这一档）。
const BASE_PLAN_CLAUDE: &str = "max_5x";
const BASE_PLAN_CODEX: &str = "plus";

/// 套餐 → 倍率对照表。
///
/// 倍率是**推出来的,不是测出来的**：由官方公布的限额比取倒数——配额大 N 倍的档,
/// 同样 1 美元当量只吃掉 1/N 的百分点 ⇒ `scale` 就是基准档的 1/N。仍是估计;真实数
/// 由回溯路的样本在首次启动后接管（预设只值 `calib:PRIOR_WEIGHT` 个样本当量）。
///
/// 要防的是**低估**方向：配额更小的档真实系数更大,预设偏小 ⇒ 取数偏晚 ⇒ 「界面上还有
/// 余量、其实已经用完」。高估只会让取数偏频。所以**同名档拿不准时一律取较小的那一档**
/// （Pro 5x 而不是 Pro 20x）。
///
/// > ⚠️ **同一个词在两个平台意思相反**：Claude 的 Pro 是**小**档（Max 之下）,
/// > ChatGPT 的 Pro 是**大**档（Plus 之上）。两张表因此方向相反,别互相抄。
///
/// 匹配按**子串、从特殊到一般**——同一个函数既吃 `plan_type`（"max" / "plus"）
/// 也吃 Claude 凭据里的 `rateLimitTier`（"default_claude_max_5x"）。表里没有的档
/// （Claude Team / Enterprise、ChatGPT Free / Go / Enterprise：官方没给可比的限额）
/// 回落 1.0 = 按基准档算。
fn plan_table(platform: Platform) -> &'static [(&'static str, f64)] {
    match platform {
        // 官方：Max 5x = 5 × Pro,Max 20x = 20 × Pro（同一句话里给出）⇒ 以 Max 5x 为
        // 基准,Pro = 5 倍、Max 20x = 1/4 倍。
        Platform::Claude => &[
            ("max_20x", 0.25),
            ("max20x", 0.25),
            ("max_5x", 1.0),
            ("max", 1.0), // 凭据 `subscriptionType` 只说 "max",分不出 5x / 20x → 按 5x 算
            ("pro", 5.0),
        ],
        // 官方 Codex 对照表（每 5 小时消息条数估计,逐模型行的比例一致）：
        // Plus = Standard Business = 1,Pro 5x = 5,Pro 20x = 20。
        Platform::Codex => &[
            ("pro_20x", 0.05),
            ("pro20x", 0.05),
            ("pro_5x", 0.2),
            ("pro", 0.2), // API 的 `plan_type` 只说 "pro" → 按 5x 算（取较小档,偏高估）
            ("plus", 1.0),
            ("business", 1.0),
            ("edu", 1.0), // 官方表里没有,但出厂预设的实测账户就在这一档
        ],
    }
}

/// 表里查一档（大小写无关的子串匹配;查不到 = None,由调用方决定回落）。
fn plan_lookup(platform: Platform, plan: &str) -> Option<f64> {
    let p = plan.trim().to_ascii_lowercase();
    if p.is_empty() {
        return None;
    }
    plan_table(platform).iter().find(|(k, _)| p.contains(k)).map(|(_, m)| *m)
}

/// 基准档的名字（倍率 1.0 的那一档;回落时按它算）。
fn base_plan(platform: Platform) -> &'static str {
    match platform {
        Platform::Claude => BASE_PLAN_CLAUDE,
        Platform::Codex => BASE_PLAN_CODEX,
    }
}

/// 当前套餐（进程内单一源）——预设要按档折算,而 [`prior_scale`] 的调用方
/// （`calib:scale`）拿不到 store。两格分开存：
/// - `plan` = 快照里的 `plan_type`（两个平台都有,但 Claude 侧只到 "max"）;
/// - `tier` = Claude 凭据的 `rateLimitTier`（**只有它分得出 5x / 20x**）。
///
/// 取值时 `tier` 优先,但**只在它能在表里查到**时才算数——拿不准就退回 `plan`,
/// 再拿不准退回基准档。
#[derive(Default, Clone)]
struct PlanHint {
    plan: String,
    tier: String,
}

static PLAN: LazyLock<Mutex<HashMap<Platform, PlanHint>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn plan_slot() -> std::sync::MutexGuard<'static, HashMap<Platform, PlanHint>> {
    PLAN.lock().unwrap_or_else(|e| e.into_inner())
}

/// 记下快照里的套餐名（`store` 每次读写快照时调用——快照行就是套餐的单一源）。
pub fn set_plan(platform: Platform, plan: &str) {
    let mut slot = plan_slot();
    let hint = slot.entry(platform).or_default();
    if hint.plan != plan {
        hint.plan = plan.to_string();
    }
}

/// 记下 Claude 凭据里的 `rateLimitTier`（取数时顺手带上;它是 5x / 20x 的唯一来源）。
pub fn set_plan_tier(platform: Platform, tier: &str) {
    let mut slot = plan_slot();
    let hint = slot.entry(platform).or_default();
    if hint.tier != tier {
        hint.tier = tier.to_string();
    }
}

/// 两个套餐名是不是同一**倍率类**——窗口一样大,标定样本就可比,该放进同一代。
///
/// **两边都得在表里查得到才算数**。查不到的档走「回落基准档」,那是占位值而不是判断,
/// 拿它当依据会把互不相干的档（Claude Team、ChatGPT Go…）通通归成基准档那一类。
/// 所以只要有一边查不到,就退回按名字精确比。
pub fn same_plan_class(platform: Platform, a: &str, b: &str) -> bool {
    match (plan_lookup(platform, a), plan_lookup(platform, b)) {
        // 两边都是表里的字面常量,相等就是逐位相等
        (Some(x), Some(y)) => (x - y).abs() <= f64::EPSILON,
        _ => a.trim().eq_ignore_ascii_case(b.trim()),
    }
}

/// 两格合一的取值规则（纯函数,可单测——**不要**在单测里动进程全局:
/// `calib` 的单测与本模块同一个测试二进制,并行跑会互相踩）。
fn resolve_multiplier(platform: Platform, tier: &str, plan: &str) -> f64 {
    plan_lookup(platform, tier)
        .or_else(|| plan_lookup(platform, plan))
        // 两格都认不出（"unknown" / 官方改了命名 / 表里没收的档）→ 按基准档算,
        // 即原样用 PRIOR_* 那个值。
        .unwrap_or_else(|| plan_lookup(platform, base_plan(platform)).unwrap_or(1.0))
}

fn current_multiplier(platform: Platform) -> f64 {
    let hint = plan_slot().get(&platform).cloned().unwrap_or_default();
    resolve_multiplier(platform, &hint.tier, &hint.plan)
}

/// 该平台**当前套餐**的出厂预设系数（%/美元当量）。
/// 基准档的值见 [`PRIOR_CLAUDE`],按档折算的倍率见 [`plan_table`]。
pub fn prior_scale(platform: Platform) -> f64 {
    let base = match platform {
        Platform::Claude => PRIOR_CLAUDE,
        Platform::Codex => PRIOR_CODEX,
    };
    base * current_multiplier(platform)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 足够晚的时刻:出厂种子里所有模型都已生效。
    const NOW: i64 = 1_790_000_000;

    #[test]
    fn breakdown_parses_both_historical_shapes() {
        let bare = r#"{"claude-opus-5":[100,10,0,0]}"#;
        let wrapped = r#"{"src":"desktop","models":{"claude-opus-5":[100,10,0,0]}}"#;
        assert_eq!(parse_breakdown(bare), parse_breakdown(wrapped), "包装层不影响解析结果");
        assert_eq!(parse_breakdown(bare)["claude-opus-5"], [100, 10, 0, 0]);
        assert!(parse_breakdown("not json").is_empty());
        assert!(parse_breakdown("{}").is_empty());
    }

    #[test]
    fn recomputed_cost_matches_incremental_accumulation() {
        // 重算路径与 demand.rs 的逐笔累加必须等价,否则升修订号会凭空改变样本
        let models = BTreeMap::from([
            ("claude-opus-5".to_string(), [100_000i64, 2_000, 50_000, 1_000]),
            ("sol-preview".to_string(), [10_000i64, 0, 0, 0]),
        ]);
        let (total, unknown) = cost_of_breakdown(Platform::Claude, &models, NOW);
        let mut ref_total = 0.0;
        let mut ref_unknown = 0.0;
        for (m, v) in &models {
            let t = Tokens { input: v[0], output: v[1], cache_read: v[2], cache_write: v[3] };
            let (c, known) = cost_of(Platform::Claude, m, &t, NOW);
            ref_total += c;
            if !known {
                ref_unknown += c;
            }
        }
        assert!((total - ref_total).abs() < 1e-12);
        assert!((unknown - ref_unknown).abs() < 1e-12);
        assert!(unknown > 0.0, "未知代号模型照常标记");
    }

    /// 代价就是官方价目下的美元数。
    #[test]
    fn cost_is_the_official_dollar_equivalent() {
        // 100 万个 Sonnet 5 输入 token,官方 2 USD/Mtok ⇒ 正好 $2
        let t = Tokens { input: 1_000_000, ..Default::default() };
        let (c, known) = cost_of(Platform::Claude, "claude-sonnet-5", &t, NOW);
        assert!(known && (c - 2.0).abs() < 1e-12, "得到 {c}");
        // Opus 5 是 5/25 ⇒ 10 万输入 + 1 万输出 = 0.5 + 0.25 = $0.75
        let t = Tokens { input: 100_000, output: 10_000, ..Default::default() };
        let (c, _) = cost_of(Platform::Claude, "claude-opus-5", &t, NOW);
        assert!((c - 0.75).abs() < 1e-12, "得到 {c}");
        // gpt-5.6-sol 是 4/20/0.4/5 ⇒ 每项 100 万 token = 4+20+0.4+5 = $29.4
        let t = Tokens { input: 1_000_000, output: 1_000_000, cache_read: 1_000_000, cache_write: 1_000_000 };
        let (c, _) = cost_of(Platform::Codex, "gpt-5.6-sol", &t, NOW);
        assert!((c - 29.4).abs() < 1e-12, "得到 {c}");
    }

    #[test]
    fn expensive_models_cost_more_per_token() {
        let t = Tokens { input: 10_000, output: 1_000, ..Default::default() };
        let (opus, known_o) = cost_of(Platform::Claude, "claude-opus-5", &t, NOW);
        let (sonnet, known_s) = cost_of(Platform::Claude, "claude-sonnet-5", &t, NOW);
        let (fable, known_f) = cost_of(Platform::Claude, "claude-fable-5-1", &t, NOW);
        assert!(known_o && known_s && known_f, "三个都在价目表里");
        assert!((opus / sonnet - 2.5).abs() < 1e-9, "Opus 每 token 是 Sonnet 的 2.5 倍");
        assert!((fable / sonnet - 5.0).abs() < 1e-9, "Fable 是 5 倍");
    }

    #[test]
    fn fable_cache_reads_are_cheaper_than_the_common_ratio() {
        let t = Tokens { cache_read: 100_000, ..Default::default() };
        let (fable, _) = cost_of(Platform::Claude, "claude-fable-5-1", &t, NOW);
        let (opus, _) = cost_of(Platform::Claude, "claude-opus-5", &t, NOW);
        // Fable 输入价是 Opus 的 2 倍,但缓存读单价 0.25 vs 0.5 ⇒ 缓存读代价反而只有一半
        assert!((fable / opus - 0.5).abs() < 1e-9);
    }

    /// 按 100 万输入 token 取该平台的相对输入权重,便于直接对账官方价目。
    fn input_weight(platform: Platform, model: &str, baseline_usd: f64) -> (f64, bool) {
        let t = Tokens { input: 1_000_000, ..Default::default() };
        let (c, known) = cost_of(platform, model, &t, NOW);
        (c / baseline_usd, known)
    }

    #[test]
    fn codex_5_6_family_is_priced_apart() {
        // sol / terra / luna 输入价各不相同,不能共用一条 contains("gpt-5")。
        // 官方输入价 4 / 2 / 0.2 USD,基准 gpt-5 = 1.25 ⇒ 3.2 / 1.6 / 0.16。
        for (model, want) in [("gpt-5.6-sol", 3.2), ("gpt-5.6-terra", 1.6), ("gpt-5.6-luna", 0.16)] {
            let (w, known) = input_weight(Platform::Codex, model, 1.25);
            assert!(known, "{model} 在出厂价目表里");
            assert!((w - want).abs() < 1e-9, "{model} 权重 {w} != {want}");
        }
    }

    #[test]
    fn longest_key_wins_over_the_family_prefix() {
        // "gpt-5.6-sol" 同时命中 "gpt-5-6-sol" / "gpt-5-6" / "gpt-5",必须取最长的那条
        let (sol, _) = input_weight(Platform::Codex, "gpt-5.6-sol", 1.25);
        let (gpt5, _) = input_weight(Platform::Codex, "gpt-5", 1.25);
        assert!((sol / gpt5 - 3.2).abs() < 1e-9, "不能落到 gpt-5 的价");
        // gpt-5 不许抢走 gpt-5-mini / gpt-5-nano
        let (mini, known_m) = input_weight(Platform::Codex, "gpt-5-mini", 1.25);
        assert!(known_m && (mini - 0.2).abs() < 1e-9, "gpt-5-mini 官方 0.25 USD ⇒ 权重 0.2");
        // 分隔符归一:两种写法必须等价
        let t = Tokens { input: 1_000, output: 100, cache_read: 500, cache_write: 50 };
        assert_eq!(
            cost_of(Platform::Codex, "gpt-5.6-luna", &t, NOW),
            cost_of(Platform::Codex, "gpt-5-6-luna", &t, NOW)
        );
        assert_eq!(
            cost_of(Platform::Claude, "claude-fable-5.1", &t, NOW),
            cost_of(Platform::Claude, "claude-fable-5-1", &t, NOW)
        );
    }

    #[test]
    fn openai_before_5_6_does_not_charge_cache_writes() {
        // 官方价目里 gpt-5 / gpt-5.3-codex 根本没有缓存写这一项;gpt-5.6 起才有
        let w = Tokens { cache_write: 100_000, ..Default::default() };
        assert_eq!(cost_of(Platform::Codex, "gpt-5", &w, NOW).0, 0.0);
        assert_eq!(cost_of(Platform::Codex, "gpt-5.3-codex", &w, NOW).0, 0.0);
        assert!(cost_of(Platform::Codex, "gpt-5.6-sol", &w, NOW).0 > 0.0);
        // Anthropic 全系都计价
        assert!(cost_of(Platform::Claude, "claude-sonnet-5", &w, NOW).0 > 0.0);
    }

    /// `-pro` 一族的缓存读:官方目录里**没有这一项**,不能套通行比例 0.1。
    /// 已在 `scripts/price-keys.json` 声明为已知偏差。
    #[test]
    fn pro_models_have_no_cache_read_price() {
        let r = Tokens { cache_read: 1_000_000, ..Default::default() };
        for m in ["gpt-5-pro", "gpt-5.2-pro", "gpt-5.4-pro", "gpt-5.5-pro"] {
            let (c, known) = cost_of(Platform::Codex, m, &r, NOW);
            assert!(known, "{m} 在表里");
            assert_eq!(c, 0.0, "{m} 的缓存读官方不计价");
        }
        // 非 -pro 的同代模型照常计价,证明不是整表漏了
        assert!(cost_of(Platform::Codex, "gpt-5.5", &r, NOW).0 > 0.0);
    }

    #[test]
    fn auto_review_is_priced_by_routing_but_never_trusted() {
        let t = Tokens { input: 1_000_000, ..Default::default() };
        let (review, known) = cost_of(Platform::Codex, "codex-auto-review", &t, NOW);
        let (luna, _) = cost_of(Platform::Codex, "gpt-5.6-luna", &t, NOW);
        assert!((review - luna).abs() < 1e-12, "按当前已知路由折价");
        assert!(!known, "路由标签不是模型名,价目不可校验 ⇒ 不参与标定");
    }

    #[test]
    fn sonnet_generations_are_priced_apart() {
        // Sonnet 5 降到 2/10,4.5 / 4.6 仍是 3/15 ⇒ 不能一条 contains("sonnet") 打发
        let (s5, _) = input_weight(Platform::Claude, "claude-sonnet-5", 2.0);
        let (s45, known) = input_weight(Platform::Claude, "claude-sonnet-4-5-20250929", 2.0);
        assert!(known && (s5 - 1.0).abs() < 1e-9 && (s45 - 1.5).abs() < 1e-9);
    }

    #[test]
    fn only_fable_5_1_has_the_cheap_cache_read() {
        // 5.1 的缓存读 0.25（自身输入价的 0.025）;Fable 5 与 Mythos 5 都是通行的 0.1
        let t = Tokens { cache_read: 100_000, ..Default::default() };
        let cheap = cost_of(Platform::Claude, "claude-fable-5-1", &t, NOW).0;
        let plain = cost_of(Platform::Claude, "claude-fable-5", &t, NOW).0;
        let mythos = cost_of(Platform::Claude, "claude-mythos-5", &t, NOW).0;
        assert!((plain / cheap - 4.0).abs() < 1e-9, "Fable 5 的缓存读是 5.1 的 4 倍");
        assert!((mythos - plain).abs() < 1e-12);
    }

    #[test]
    fn unknown_model_falls_back_to_baseline_and_flags() {
        let t = Tokens { input: 1_000, ..Default::default() };
        let (c, known) = cost_of(Platform::Claude, "claude-terra-9", &t, NOW);
        assert!(!known, "未知代号模型标记为不可信");
        assert_eq!(c, cost_of(Platform::Claude, "claude-sonnet-5", &t, NOW).0, "按基准价目记");
    }

    /// 回落常量必须等于出厂种子里基准模型的首段——两处写同一个数,得钉住。
    #[test]
    fn fallback_matches_seed() {
        let seg = |key: &str| {
            price::factory_seed()
                .iter()
                .filter(|r| r.match_key == key)
                .min_by_key(|r| r.effective_from)
                .unwrap()
                .clone()
        };
        for (fb, key) in [(CLAUDE_FALLBACK, "sonnet-5"), (CODEX_FALLBACK, "gpt-5")] {
            let s = seg(key);
            assert_eq!(
                (fb.input, fb.output, fb.cache_read, fb.cache_write),
                (s.usd_input, s.usd_output, s.usd_cache_read, s.usd_cache_write),
                "回落价目与种子里的 {key} 不一致"
            );
        }
    }

    #[test]
    fn cache_tokens_are_discounted() {
        let read = Tokens { cache_read: 100_000, ..Default::default() };
        let input = Tokens { input: 100_000, ..Default::default() };
        let (rc, _) = cost_of(Platform::Claude, "claude-sonnet-5", &read, NOW);
        let (ic, _) = cost_of(Platform::Claude, "claude-sonnet-5", &input, NOW);
        assert!((rc / ic - 0.1).abs() < 1e-9, "Sonnet 5 缓存读 0.2 vs 输入 2.0");
    }

    /// 两个平台的先验相差近 10 倍是**如实记录**,不是笔误——一并钉住量级,
    /// 免得有人「顺手统一」回一个数（见 `PRIOR_CLAUDE` 的注释）。
    #[test]
    fn priors_are_per_platform_and_an_order_of_magnitude_apart() {
        let (claude, codex) = (prior_scale(Platform::Claude), prior_scale(Platform::Codex));
        assert!(codex / claude > 5.0, "Codex 的 5h 窗比 Claude 小一个数量级");
        // 满窗折合的美元当量:Claude 约百元量级,Codex 约十元量级
        assert!((100.0 / claude - 112.0).abs() < 5.0, "Claude 满 5h 窗 ≈ $110");
        assert!((100.0 / codex - 11.2).abs() < 1.0, "Codex 满 5h 窗 ≈ $11");
    }

    /// 按时刻取价的**端到端**验收：某模型在 T 降价,T 前按旧价、T 后按新价,
    /// 而**不含该模型的明细一字不变**。
    #[test]
    fn a_price_cut_only_moves_rows_containing_that_model() {
        const T: i64 = 1_800_000_000;
        let rows = price::two_segment_fixture(T);
        price::with_rows(&rows, || {
            let t = Tokens { input: 1_000_000, ..Default::default() };
            // 降价的模型：T 前 $10,T 后 $1
            assert!((cost_of(Platform::Claude, "claude-widget-1", &t, T - 1).0 - 10.0).abs() < 1e-12);
            assert!((cost_of(Platform::Claude, "claude-widget-1", &t, T).0 - 1.0).abs() < 1e-12);
            // 没降价的模型：两个时刻一字不变
            let before = cost_of(Platform::Claude, "claude-steady-1", &t, T - 1);
            let after = cost_of(Platform::Claude, "claude-steady-1", &t, T + 86_400);
            assert_eq!(before, after);
            assert!((before.0 - 3.0).abs() < 1e-12);
            // 混合明细：只有含降价模型的那部分变
            let models = BTreeMap::from([
                ("claude-steady-1".to_string(), [1_000_000i64, 0, 0, 0]),
                ("claude-widget-1".to_string(), [1_000_000i64, 0, 0, 0]),
            ]);
            let (b, _) = cost_of_breakdown(Platform::Claude, &models, T - 1);
            let (a, _) = cost_of_breakdown(Platform::Claude, &models, T);
            assert!((b - 13.0).abs() < 1e-12, "旧价 3 + 10");
            assert!((a - 4.0).abs() < 1e-12, "新价 3 + 1");
        });
    }

    /// 基准档的倍率必须是 1.0——否则 `PRIOR_CLAUDE` / `PRIOR_CODEX` 记的实测值
    /// 就不再是它自己那一档的了。
    #[test]
    fn base_plans_are_the_unit_of_the_table() {
        assert_eq!(plan_lookup(Platform::Claude, BASE_PLAN_CLAUDE), Some(1.0));
        assert_eq!(plan_lookup(Platform::Codex, BASE_PLAN_CODEX), Some(1.0));
    }

    /// 官方限额比：Claude Max 5x = 5 × Pro、Max 20x = 20 × Pro;
    /// Codex Pro 5x = 5 × Plus、Pro 20x = 20 × Plus。倍率取倒数。
    #[test]
    fn plan_multipliers_follow_the_published_limit_ratios() {
        let claude = |plan| resolve_multiplier(Platform::Claude, "", plan);
        let codex = |plan| resolve_multiplier(Platform::Codex, "", plan);
        assert_eq!(claude("pro"), 5.0);
        assert_eq!(claude("max"), 1.0);
        assert_eq!(claude("max_20x"), 0.25);
        assert_eq!(codex("plus"), 1.0);
        assert_eq!(codex("edu"), 1.0);
        assert_eq!(codex("pro"), 0.2);
        // 同一个词在两个平台方向相反（Claude Pro 是小档,ChatGPT Pro 是大档）,
        // 这一行就是钉子
        assert!(claude("pro") > codex("pro"));
    }

    /// 凭据里的 `rateLimitTier` 原样进表也要认得出来（子串匹配,从特殊到一般）。
    #[test]
    fn credential_tier_strings_resolve_without_preprocessing() {
        let tier = |t| resolve_multiplier(Platform::Claude, t, "max");
        assert_eq!(tier("default_claude_max_5x"), 1.0);
        assert_eq!(tier("DEFAULT_CLAUDE_MAX_20X"), 0.25);
        assert_eq!(resolve_multiplier(Platform::Claude, "default_claude_pro", "pro"), 5.0);
    }

    /// 倍率类：表里同倍率的两个档算同类;倍率不同的不算;**有一边表里没有就退回按名字比**
    /// （回落值是占位不是判断,拿它归类会把互不相干的档并到基准档那一类）。
    #[test]
    fn same_multiplier_means_same_class_but_only_for_plans_in_the_table() {
        let codex = |a, b| same_plan_class(Platform::Codex, a, b);
        assert!(codex("edu", "plus"), "官方对照表里 Business 与 Plus 同额,edu 按实测同档");
        assert!(codex("plus", "business"));
        assert!(!codex("edu", "pro"), "Pro 5x 是 Plus 的 5 倍,不同类");
        assert!(!codex("edu", "team"), "team 不在表里 ⇒ 退回按名字比");
        assert!(codex("team", "TEAM"), "都不在表里时按名字比,大小写无关");
        assert!(!codex("team", "enterprise"));
        // Claude：max 与 max_5x 是同一档,max_20x 不是
        assert!(same_plan_class(Platform::Claude, "max", "default_claude_max_5x"));
        assert!(!same_plan_class(Platform::Claude, "max", "max_20x"));
        assert!(!same_plan_class(Platform::Claude, "max", "pro"));
    }

    /// 表里没有的档 / 空串 → 1.0（= 按基准档算）。
    #[test]
    fn unknown_plans_fall_back_to_the_base_plan() {
        for p in ["", "unknown", "team", "enterprise", "go", "free"] {
            assert_eq!(resolve_multiplier(Platform::Codex, "", p), 1.0, "codex {p}");
        }
        for p in ["", "unknown", "team", "enterprise"] {
            assert_eq!(resolve_multiplier(Platform::Claude, "", p), 1.0, "claude {p}");
        }
    }

    /// `tier` 优先于 `plan`,但**只在它查得到**时——查不到要退回 `plan` 而不是基准档。
    /// （走纯函数,不动进程全局:`calib` 的单测同二进制并行跑,会互相踩。）
    #[test]
    fn tier_wins_only_when_it_resolves() {
        let m = |tier, plan| resolve_multiplier(Platform::Claude, tier, plan);
        assert_eq!(m("default_claude_max_20x", "max"), 0.25, "tier 分得出 20x");
        assert_eq!(m("some_new_tier_name", "pro"), 5.0, "认不出的 tier → 退回 plan");
        assert_eq!(m("", "pro"), 5.0, "没有 tier（Codex 侧就没有）→ 只看 plan");
        assert_eq!(m("", "unknown"), 1.0, "两格都认不出 → 基准档");
    }

    /// 预设按当前套餐折算：同一个基准实测值 × 倍率。
    #[test]
    fn prior_scale_is_the_base_measurement_times_the_plan_multiplier() {
        let claude_pro = PRIOR_CLAUDE * resolve_multiplier(Platform::Claude, "", "pro");
        let codex_pro = PRIOR_CODEX * resolve_multiplier(Platform::Codex, "", "pro");
        assert!((claude_pro - 4.45).abs() < 1e-12, "Claude Pro 窗口只有 Max 5x 的 1/5");
        assert!((codex_pro - 1.78).abs() < 1e-12, "ChatGPT Pro 窗口是 Plus 的 5 倍");
        // 没设过任何套餐（单测里的常态）→ 基准档,即原样的 PRIOR_CLAUDE
        assert_eq!(prior_scale(Platform::Claude), PRIOR_CLAUDE);
        assert_eq!(prior_scale(Platform::Codex), PRIOR_CODEX);
    }
}
