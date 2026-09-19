//! 按官方价目折算的「代价」（取数时机估算的第一步;
//! 把价目本体搬进 [`super:price`],本模块只负责折算与世代）。
//!
//! 为什么要折算：限流窗口的消耗大致按**价值**计,不按 token 条数计——最贵的模型
//! 30 秒里产出的 token 不多,但可能已经吃掉可观的额度。所以先把各模型、各种类的
//! token 折成同一把尺子上的「代价」,再由 calib.rs 的标定系数换成百分点。
//!
//! **代价的单位是美元当量**：
//! `cost` = 这笔 token 按**官方 API 定价**算出来的美元数。于是
//! - 不再需要「基准模型」这个本身要跟着世代走的概念（旧口径是「千个基准模型输入
//!   token 当量」,基准模型某代可能就不存在了）;
//! - `scale` 的含义变成「每 1 美元当量吃掉配额的百分之几」,面板可以直接说
//!   「这段时间你消耗了**相当于** $X 的 API 用量」。
//!
//! > **语义红线（-bis,必须写进任何面板文案）**：它**不是账单**。用户付的是
//! > 固定订阅月费,不是按 token 计费。只能说「**相当于** $X 的 API 用量」「等价价值」,
//! > **不能说「你花了 $X」**。它衡量的是等价价值 / 机会成本。
//! > 另：缓存写在部分平台按保留时长（TTL）分档,而采集层只有**一个** `cache_write`
//! > 桶,无法区分 ⇒ 按常用档估算,面板需注明。
//!
//! 未知模型（新代号模型随时会出现）落回落价目,并标记 `unknown`——这种样本照常参与
//! **触发估算**（有总比没有强）,但不参与**标定**（价目不可信的样本会污染系数）。

use std::collections::BTreeMap;

use super::model::Platform;
use super::price;

/// **出厂价格数据集的订号**（此前叫「权重表世代」）。
///
/// 它只回答一个问题：「库里这行的 `cost` 是不是按**当前这份**价格数据算出来的」。
/// `price_seed.json` 每次真的变动（新模型 / 官方调价 / 正）就 +1。
///
/// 为什么需要它：库里 `usage_pair.cost` 是**派生值**——按当时的价目把分模型 token
/// 折出来的。价目一变,旧行的 `cost` 就是按旧尺子量的,和新样本一起做和之比拟合会把
/// 系数拖偏。但原始 token 一直存在 `usage_pair.breakdown` 里,所以订号一升,存量行
/// **就地重算**即可（`store:recompute_stale_costs`,**每个模型按它在该区间 `t1`
/// 时刻生效的价目**）——不是丢数据,是换把尺子重新量一遍。
///
/// 与**套餐变更**是两回事：套餐变了 `cost` 没错、是 `scale`（%/美元）变了,那条靠
/// `usage_pair.plan_type` 分代,旧样本留着但不参与当前拟合。
///
/// **改 [`prior_scale`] 不在此列,别顺手升版**：它不参与 `cost` 的计算（只当拟合的先验
/// 与样本准入带的基准）,存量行一个数都不会变。改了它只是让下一次 refit 用新的先验重算
/// ——**自动生效,零迁移**。
///
/// 订史：
/// - **1** 初版,硬编码 `match` 链的相对权重。
/// - **2**对表 models.dev 第一方目录重出厂价目。世代 1 把 `gpt-5.6`
///   一族（sol / terra / luna）用一条 `contains（"gpt-5")` 折成同一个权重,而它们的真实
///   输入价是 4 / 2 / 0.2 USD/Mtok——**相差 20 倍**;`codex-auto-review` 被当成 gpt-5
///   计价,高估约 6.4 倍。
/// - **3**价目搬进 `price_model` 表、按模型带生效时间、单位改美元当量;
///   顺带正 `-pro` 一族的缓存读（官方目录里根本没有这一项,世代 2 误套了通行比例 0.1）。
pub const WEIGHT_VERSION: u32 = 3;

/// 一笔 token 明细（采集批次里的四个口径）。
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
/// 新代号模型随时会出现。回落**取基准而不是取当代旗舰**是有意的：估高会让取数更频繁
/// （对限流不利）,估低只是让首次取数偏晚。这类样本照常参与触发估算,但标记 `unknown`。
///
/// 这里仍是硬编码常量而不是读库：它是"库里查不到"时的退路,拿库来兜库查不到的情形
/// 说不通。数值与 `price_seed.json` 里这两个键的首段一致（单测 `fallback_matches_seed` 钉住）。
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
/// 这里按目前已知的路由（起为 gpt-5.6-luna,依据 ccusage 维护的
/// `codex-auto-review-fallbacks.json`,其自述为 best-effort）折价,但**照样标记为
/// 不可信**：路由随时会改且我们无从校验,靠它主导的样本不该拿去标定系数。
/// 相比世代 1 按 gpt-5 计价（高估约 6.4 倍）已经是大幅改善,但精度到此为止。
///
/// （折价但不可信。）
const AUTO_REVIEW: &str = "codex-auto-review";
const AUTO_REVIEW_ROUTES_TO: &str = "gpt-5-6-luna";

/// 一笔 token 明细的**美元当量**。返回 （代价, 该模型的价目是否可信)。
///
/// `at` = 这笔 token 所属区间的时刻（在线路用 `now`,重算与冷启动用样本区间的 `t1`）。
/// 每个模型按它**在该时刻生效**的价目取价——一个区间里的不同模型各取各的时间线,
/// 所以「区间跨世代」这个问题不存在。
pub fn cost_of(platform: Platform, model_key: &str, t: &Tokens, at: i64) -> (f64, bool) {
    let (input, output, cache_read, cache_write, known) = resolve(platform, model_key, at);
    let usd = input * t.input as f64
        + output * t.output as f64
        + cache_read * t.cache_read as f64
        + cache_write * t.cache_write as f64;
    (usd / PER_MTOK, known)
}

/// 取该模型在该时刻的四项单价 + 是否可信。
fn resolve(platform: Platform, model_key: &str, at: i64) -> (f64, f64, f64, f64, bool) {
    let fb = match platform {
        Platform::Claude => CLAUDE_FALLBACK,
        Platform::Codex => CODEX_FALLBACK,
    };
    let auto_review =
        matches!(platform, Platform::Codex) && price::normalized(model_key).contains(AUTO_REVIEW);
    // 路由标签:折成当前路由的价,但不给 known——理由见 AUTO_REVIEW
    let lookup_key = if auto_review { AUTO_REVIEW_ROUTES_TO } else { model_key };
    match price::price_at(platform, lookup_key, at) {
        Some(r) => (r.usd_input, r.usd_output, r.usd_cache_read, r.usd_cache_write, !auto_review),
        None => (fb.input, fb.output, fb.cache_read, fb.cache_write, false),
    }
}

/// 把一份分模型明细折成 `（代价, 其中未知模型的代价)`,每个模型按 `at` 时刻的价目。
/// 与 demand.rs 的逐笔累加等价——重算存量样本走这条,保证两条路算出来的是同一个数。
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
/// **规范形状 = 裸 map** `{"model":[输入,输出,缓存读,缓存写]}`（demand.rs 一直这么写）。
/// 历史上 bootstrap.rs 另写过一层包装 `{"src":…,"models":{…}}`——`src` 现已是独立列,
/// 包装纯属冗余,存量行由 store 的就地迁移统一。这里**两种都认**：重算是"派生值可恢复"
/// 的唯一依靠,不能因为一半行形状不同就恢复不了。
pub fn parse_breakdown(json: &str) -> BTreeMap<String, [i64; 4]> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return BTreeMap::new();
    };
    let inner = v.get("models").unwrap_or(&v);
    serde_json::from_value(inner.clone()).unwrap_or_default()
}

/// 出厂预设标定系数（**百分点 / 美元当量**）。
///
/// 取值来自的**离线**：拿本机桌面端 `plan-usage-history.json` 的两周
/// 历史配 collector.db 的同期轮记录,219 条可用样本拟合出 ≈ 0.002 %/旧代价单位。
/// 把代价单位从「千个 Sonnet 5 输入 token 当量」换成美元当量,这是一次
/// **纯量纲换算**：旧单位 1 = 1000 × 2 USD/Mtok ÷ 1e6 = $0.002,故
/// `0.002 %/旧单位 ÷ 0.002 $/旧单位` = **1.0 %/美元**,估算值逐位不变（单测钉住）。
///
/// 好记的量级：**1% 的 Claude Max 5h 额度 ≈ 1 美元 API 当量**,即一个完整 5h 窗口
/// ≈ $100 的 API 等价用量。
///
/// 注意这是 **Max 档**账户的量级;Pro 档窗口更小,同样的 token 吃掉的百分比更大,真实
/// 系数约为其数倍——预设偏小意味着首次取数**偏晚而非偏频**（对限流更安全）,几条
/// 样本后就由 calib.rs 的混合公式接管。两个平台各有一条本地历史冷启动路
/// （Claude 走 bootstrap.rs,Codex 走 codex_rollout.rs）,首次启动就能把预设换成。
///
/// **起按平台分开,数值由填**（此前两个平台共用 `1.0`）。
///
/// 换成美元当量之后两边才第一次同义（旧口径里它是「每千个**该平台基准模型**输入 token
/// 当量」,基准模型不同 ⇒ 同一个数在两边其实是两个量）。但**同义不等于数值接近**：
/// 本机 Claude Max ≈ **0.89**、Codex edu/plus ≈ **8.88 %/美元**,相差近 10 倍
/// ——而这是**对的**,它就是「这个套餐一个窗口有多大」,月费量级本来就差一个数量级。
/// 共用 `1.0` 对 Codex 是低估约 9 倍;在 codex_rollout.rs 落地之前那就是 Codex 的
/// **全部**（样本恒为 0）。
///
/// 这**不是**把刚删掉的「基准模型」概念请回来：那个是把平台差异藏在一个常量的**含义**
/// 里（同一个数在两边指不同的东西）,这里是量纲统一之后**如实写出两个测出来的数**。
///
/// 只保留两位有效数字。再多就是假精度——同一平台换个套餐（Max / Pro、plus / pro）
/// 真实系数本就差几倍,而这个数只是「还没有任何本机样本时的起点」：两条回溯路
/// （Claude 走 bootstrap.rs,Codex 走 codex_rollout.rs）首次启动就会把它换成,
/// 之后它只剩 `fit` 里那 5 个样本当量的权重。预设偏小意味着首次取数**偏晚而非偏频**。
///
/// 改这两个数**不需要**升 [`WEIGHT_VERSION`]（不参与 `cost` 的计算）,也不需要升
/// `calib:ADMISSION_RULE_VERSION`（真库：换成先验之后,两个平台参与拟合的
/// 样本集一条不变,只是混合公式不再把值往 `1.0` 拽）。
const PRIOR_CLAUDE: f64 = 0.89;
const PRIOR_CODEX: f64 = 8.9;

/// 该平台的出厂预设系数（%/美元当量）。语义与来历见 [`PRIOR_CLAUDE`]。
pub fn prior_scale(platform: Platform) -> f64 {
    match platform {
        Platform::Claude => PRIOR_CLAUDE,
        Platform::Codex => PRIOR_CODEX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 足够晚的时刻:出厂种子里所有模型都已生效。
    const NOW: i64 = 1_790_000_000; // 2026-09-21

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
        // 重算路径与 demand.rs 的逐笔累加必须等价,否则换世代会凭空改变样本
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

    /// 代价就是官方价目下的美元数——这是 S1 换单位之后最该直接钉住的一条。
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
        // 世代 1 的 bug：sol / terra / luna 共用一条 contains("gpt-5") ⇒ 权重全是 1.0。
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

    /// `-pro` 一族的缓存读:官方目录里**没有这一项**（世代 2 误套了通行比例 0.1）。
    /// 由生成器的 `--crosscheck` 发现,已在 `price-keys.json` 声明为已知偏差。
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

    /// 换单位是**纯量纲换算**：1 个旧单位恰好是 $0.002,`cost` 逐位等值。
    ///
    /// 这条钉的是 `cost` 的量纲,不是先验的数值——先验 2026-09-19 已按实测改成
    /// per-platform,拿它来钉换算关系会把两件不相干的事混成一笔。
    #[test]
    fn cost_survives_the_unit_change() {
        // 旧口径:cost_旧 = Σ(相对权重 × token) / 1000（Claude 以 Sonnet 5 为基准）
        // 新口径:cost_新 = cost_旧 × 0.002 美元/旧单位
        const OLD_UNIT_USD: f64 = 0.002; // 1 旧单位 = 1000 × 2 USD/Mtok ÷ 1e6

        // 100 万个 Sonnet 输入 token = 旧口径的 1000 个单位 ⇒ $2.00
        let t = Tokens { input: 1_000_000, ..Default::default() };
        let (c, _) = cost_of(Platform::Claude, "claude-sonnet-5", &t, NOW);
        assert!((c - 1000.0 * OLD_UNIT_USD).abs() < 1e-12);
        // 折成 Opus:同样的美元当量只需 40 万输入 token（价目 2.5 倍）
        let opus = Tokens { input: 400_000, ..Default::default() };
        let (co, _) = cost_of(Platform::Claude, "claude-opus-5", &opus, NOW);
        assert!((co - c).abs() < 1e-12);
    }

    /// 两个平台的先验相差近 10 倍是**如实记录**,不是笔误——一并钉住量级,
    /// 免得将来有人"顺手统一"回一个数（见 `PRIOR_CLAUDE` 的注释）。
    #[test]
    fn priors_are_per_platform_and_an_order_of_magnitude_apart() {
        let (claude, codex) = (prior_scale(Platform::Claude), prior_scale(Platform::Codex));
        assert!(codex / claude > 5.0, "Codex 的 5h 窗比 Claude 小一个数量级");
        // 满窗折合的美元当量:Claude 约百元量级,Codex 约十元量级
        assert!((100.0 / claude - 112.0).abs() < 5.0, "Claude 满 5h 窗 ≈ $110");
        assert!((100.0 / codex - 11.2).abs() < 1.0, "Codex 满 5h 窗 ≈ $11");
    }

    /// 按时刻取价的**端到端**验收：某模型在 T 降价,T 前按旧价、T 后按新价,
    /// 而**不含该模型的明细一字不变**（设计 §5 S1 验收条款）。
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
}
