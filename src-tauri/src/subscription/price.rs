//! 官方价目快照对照表：价格从**编译进二进制的常量**升为**按模型带生效
//! 时间、可查询可展示的一等数据**。
//!
//! 库里存的是**官方公布的绝对价目（USD / Mtok）**四项——输入 / 输出 / 缓存写 / 缓存命中
//! ——按模型名精确对准,不做归一化。理由：相对权重是派生值
//! （`usd_x ÷ 基准模型 usd_input`）,而"永远不要只存派生值"是本项目的既定原则;
//! 存绝对价目还顺带消掉了「基准模型」这个本身要跟着世代走的概念,
//! 以及 Fable 5.1 缓存读那种"通行比例 + 例外"的双层规则。
//!
//! **一个模型的一段生效期 = 一行**。绝大多数模型终其生命周期只有一行;只有真的被官方
//! 降价过的模型才会有第二行。所以某模型降价只给它加一行,其余模型的历史继续有效
//! ——**版本化的粒度跟着真正会变的东西走**。
//!
//! 取价链路：
//! ```text
//! price_seed.json（生成产物,编译期嵌入,带全量历史生效期）
//!       │ 启动时 upsert 进库（幂等,按 （platform, match_key, effective_from)）
//!       ▼
//! subscriptions.db 的 price_model ──load_from──▶ 进程内索引（取数路径零 IO）
//! ```
//! 出厂种子由 `scripts/price-seed-gen.mjs` 从 models.dev 第一方目录产出并提交进仓,
//! **不做运行时取价**（重算存量样本必须可复现——同一个 `WEIGHT_VERSION`
//! 在任何机器任何时刻都要得出同一个数;联网取价会让它取决于"那台机器那一刻的目录版本"）。

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use super::model::Platform;

/// 出厂种子（生成产物,**不要手改**;维护契约见 `scripts/price-keys.json`）。
const SEED_JSON: &str = include_str!("price_seed.json");

/// 一个模型的一段生效期。四项 `usd_*` 均为 USD / Mtok 的官方 API 定价。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PriceRow {
    /// `"claude"` / `"codex"`（种子里是字符串,进索引时才折成 [`Platform`]）。
    pub platform: String,
    /// 小写子串模式（`"opus"` / `"gpt-5-6-sol"`…）,匹配时**最长键优先**。
    pub match_key: String,
    /// unix 秒;该价目开始生效（模型首次出现时 = 其上线时刻）。
    pub effective_from: i64,
    /// 面板展示名。
    pub display_name: String,
    pub usd_input: f64,
    pub usd_output: f64,
    pub usd_cache_read: f64,
    pub usd_cache_write: f64,
    /// **出处 + 上游核对信息**：这张表是可审计的官方价格快照,任何时候要能回答
    /// 「这个数从哪儿来」。"什么时候核对的"由
    /// `price_seed.json` 自己的 git 历史回答（末段）。
    #[serde(default)]
    pub source_note: String,
}

#[derive(serde::Deserialize)]
struct Seed {
    rows: Vec<PriceRow>,
}

/// 编译期嵌入的出厂种子（解析一次;种子写坏了在这里就会 panic——它是仓内的生成产物,
/// 不是外部输入,构建期的单测会先撞上）。
pub fn factory_seed() -> &'static [PriceRow] {
    static SEED: LazyLock<Vec<PriceRow>> = LazyLock::new(|| {
        serde_json::from_str::<Seed>(SEED_JSON)
            .expect("price_seed.json 解析失败（生成产物,跑 scripts/price-seed-gen.mjs 重生成）")
            .rows
    });
    &SEED
}

/// 同一个 `match_key` 的全部生效段,按 `effective_from` 升序。
#[derive(Debug, Clone)]
struct KeyGroup {
    match_key: String,
    segments: Vec<PriceRow>,
}

/// 进程内取价索引：每平台一组,**按 `match_key` 长度降序**——于是第一个 `contains`
/// 命中就是最长匹配,`gpt-5` 不会抢走 `gpt-5-mini`。
#[derive(Debug, Default)]
struct Index {
    by_platform: HashMap<Platform, Vec<KeyGroup>>,
    /// 是否已从库装载过（false = 仍在用编译期种子兜底）。
    from_db: bool,
}

fn build_index(rows: &[PriceRow], from_db: bool) -> Index {
    let mut grouped: HashMap<Platform, HashMap<String, Vec<PriceRow>>> = HashMap::new();
    for r in rows {
        let Some(p) = Platform::from_str(&r.platform) else { continue };
        grouped
            .entry(p)
            .or_default()
            .entry(r.match_key.clone())
            .or_default()
            .push(r.clone());
    }
    let mut by_platform = HashMap::new();
    for (p, keys) in grouped {
        let mut groups: Vec<KeyGroup> = keys
            .into_iter()
            .map(|(match_key, mut segments)| {
                segments.sort_by_key(|s| s.effective_from);
                KeyGroup { match_key, segments }
            })
            .collect();
        // 长键在前 ⇒ 查表取第一个命中即最长匹配
        groups.sort_by(|a, b| b.match_key.len().cmp(&a.match_key.len()));
        by_platform.insert(p, groups);
    }
    Index { by_platform, from_db }
}

static INDEX: LazyLock<RwLock<Index>> =
    LazyLock::new(|| RwLock::new(build_index(factory_seed(), false)));

fn index() -> std::sync::RwLockReadGuard<'static, Index> {
    INDEX.read().unwrap_or_else(|e| e.into_inner())
}

// 单测夹具：在当前**线程**上临时换一张价目表。
//
// 用线程局部而不是改全局索引,是因为 cargo 每个用例各跑一条线程——换全局会让
// 「某模型在 T 降价」这类夹具和读出厂种子的用例互相踩。release 构建里整段消失。
#[cfg(test)]
thread_local! {
    static OVERRIDE: std::cell::RefCell<Option<Index>> = const { std::cell::RefCell::new(None) };
}

/// 以给定价目表跑一段（仅单测;见 [`OVERRIDE`]）。
#[cfg(test)]
pub(super) fn with_rows<T>(rows: &[PriceRow], f: impl FnOnce() -> T) -> T {
    OVERRIDE.with(|o| *o.borrow_mut() = Some(build_index(rows, true)));
    let out = f();
    OVERRIDE.with(|o| *o.borrow_mut() = None);
    out
}

/// 最长键优先 + 按时刻取段的**纯查表**（语义见 [`price_at`]）。
fn pick<'a>(groups: &'a [KeyGroup], k: &str, at: i64) -> Option<&'a PriceRow> {
    let g = groups.iter().find(|g| k.contains(g.match_key.as_str()))?;
    g.segments
        .iter()
        .rev()
        .find(|s| s.effective_from <= at)
        .or_else(|| g.segments.first())
}

/// 从库装载索引（启动时调一次,**必须排在重算之前**）。
///
/// 库是唯一查询源（计算与展示都读它）,但单测与"库开不了"的退路仍走编译期种子
/// ——种子就是刚 upsert 进去的那份,两者一致;库里另外还可能有**更早版本留下的历史
/// 生效段**,那正是要留的（用户跳过若干版本再更新,中间的价格段一条不丢）。
pub fn load_from(store: &super::store::SubStore) {
    let rows = store.price_rows();
    if rows.is_empty() {
        crate::dev_log!("[subscription] price_model 空 —— 仍用编译期出厂种子");
        return;
    }
    let n = rows.len();
    let idx = build_index(&rows, true);
    let keys: usize = idx.by_platform.values().map(|g| g.len()).sum();
    *INDEX.write().unwrap_or_else(|e| e.into_inner()) = idx;
    crate::dev_log!("[subscription] price index loaded from db: {n} row(s) / {keys} key(s)");
}

/// 分隔符归一：`.` 一律当 `-`。同一个模型在不同来源里写作 `gpt-5.6-sol` 或
/// `gpt-5-6-sol`、`claude-fable-5-1` 或 `claude-fable-5.1`,不归一就会漏匹配到回落价。
pub fn normalized(model_key: &str) -> String {
    model_key.to_ascii_lowercase().replace('.', "-")
}

/// 该模型在 `at` 时刻生效的价目（`None` = 该平台没有能匹配上的键）。
///
/// 两步： 最长 `match_key` 命中; 组内取 `effective_from <= at` 的**最后**一段。
///
/// `at` 早于该键的首段时**取首段**而不是判未知：`effective_from` 来自上游目录的
/// `release_date`,而本机样本的时刻可能比它早几天（上游登记有滞后,时区也只按 UTC
/// 零点取整）。这种情形是日期精度的产物,不是"未知模型"——判未知会让该样本因
/// `unknown_cost` 过半而被踢出标定,那才是真的回归。
pub fn price_at(platform: Platform, model_key: &str, at: i64) -> Option<PriceRow> {
    let k = normalized(model_key);
    #[cfg(test)]
    if let Some(row) = OVERRIDE.with(|o| {
        o.borrow()
            .as_ref()
            .map(|idx| idx.by_platform.get(&platform).and_then(|g| pick(g, &k, at)).cloned())
    }) {
        return row;
    }
    let idx = index();
    pick(idx.by_platform.get(&platform)?, &k, at).cloned()
}

/// 某平台的全部价目行（按 match_key 与生效期排序）。
///
/// 这是「让价格成为可查询、可展示的数据」的读口。S1 只把它做出来并用单测
/// 钉住排序与完整性,**命令面留给 S2**（`get_price_models` / `get_price_at`）
/// ⇒ 非 test 构建里暂时没有调用点。
#[allow(dead_code)]
pub fn rows_for(platform: Platform) -> Vec<PriceRow> {
    #[cfg(test)]
    if let Some(out) = OVERRIDE.with(|o| {
        o.borrow().as_ref().map(|idx| collect_rows(idx, platform))
    }) {
        return out;
    }
    collect_rows(&index(), platform)
}

#[allow(dead_code)]
fn collect_rows(idx: &Index, platform: Platform) -> Vec<PriceRow> {
    let mut out: Vec<PriceRow> = idx
        .by_platform
        .get(&platform)
        .into_iter()
        .flat_map(|g| g.iter().flat_map(|g| g.segments.iter().cloned()))
        .collect();
    out.sort_by(|a, b| {
        a.match_key.cmp(&b.match_key).then(a.effective_from.cmp(&b.effective_from))
    });
    out
}

/// 单测夹具：「某模型在 T 降价到十分之一」+ 一个全程不变价的对照模型。
/// cost.rs 与 store.rs 的验收用例都用它,所以放在模块级而不是 tests 里。
#[cfg(test)]
pub(super) fn two_segment_fixture(t: i64) -> Vec<PriceRow> {
    let base = |match_key: &str, effective_from: i64, usd_input: f64| PriceRow {
        platform: "claude".into(),
        match_key: match_key.into(),
        effective_from,
        display_name: match_key.into(),
        usd_input,
        usd_output: usd_input * 5.0,
        usd_cache_read: usd_input * 0.1,
        usd_cache_write: usd_input * 1.25,
        source_note: "夹具".into(),
    };
    vec![
        base("claude-widget-1", t - 10_000_000, 10.0),
        base("claude-widget-1", t, 1.0),
        base("claude-steady-1", t - 10_000_000, 3.0),
    ]
}

/// 索引是否已从库装载（诊断用;false = 编译期种子兜底）。
#[allow(dead_code)]
pub fn is_loaded_from_db() -> bool {
    index().from_db
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 出厂种子必须覆盖两个平台,且每条价目都是正数（生成器坏了在这里就拦住）。
    #[test]
    fn factory_seed_is_sane() {
        let rows = factory_seed();
        assert!(rows.len() >= 20, "种子只有 {} 行,像是生成器出问题了", rows.len());
        for p in ["claude", "codex"] {
            assert!(rows.iter().any(|r| r.platform == p), "缺 {p} 的价目");
        }
        for r in rows {
            assert!(r.usd_input > 0.0 && r.usd_output > 0.0, "{} 输入/输出价必须为正", r.match_key);
            assert!(r.usd_cache_read >= 0.0 && r.usd_cache_write >= 0.0);
            assert!(r.effective_from > 1_500_000_000, "{} 生效时刻不像 unix 秒", r.match_key);
            assert!(!r.display_name.is_empty() && !r.source_note.is_empty(), "{} 缺展示名/出处", r.match_key);
            assert_eq!(r.match_key, normalized(&r.match_key), "match_key 必须是归一后的样子");
        }
    }

    #[test]
    fn longest_key_wins() {
        let at = 1_760_000_000; // 2025-10 之后,足够晚
        let mini = price_at(Platform::Codex, "gpt-5-mini", at).unwrap();
        assert_eq!(mini.match_key, "gpt-5-mini", "gpt-5 不许抢走 gpt-5-mini");
        assert_eq!(price_at(Platform::Codex, "gpt-5", at).unwrap().match_key, "gpt-5");
        // 分隔符归一:两种写法必须落到同一条
        let now = 1_790_000_000;
        assert_eq!(
            price_at(Platform::Codex, "gpt-5.6-sol", now).unwrap().match_key,
            price_at(Platform::Codex, "gpt-5-6-sol", now).unwrap().match_key
        );
        assert_eq!(price_at(Platform::Claude, "claude-fable-5.1", now).unwrap().match_key, "claude-fable-5-1");
    }

    #[test]
    fn unknown_model_has_no_price() {
        assert!(price_at(Platform::Claude, "claude-terra-9", 1_790_000_000).is_none());
        assert!(price_at(Platform::Codex, "claude-opus-5", 1_790_000_000).is_none(), "跨平台不串");
    }

    /// 时刻早于上游登记的上线日期 → 取首段,**不判未知**（语义见 `price_at` 注释）。
    #[test]
    fn timestamps_before_the_first_segment_clamp_to_it() {
        let row = price_at(Platform::Claude, "claude-fable-5-1", 1_000_000).unwrap();
        let first = factory_seed()
            .iter()
            .filter(|r| r.match_key == "claude-fable-5-1")
            .min_by_key(|r| r.effective_from)
            .unwrap();
        assert_eq!(row.effective_from, first.effective_from);
        assert_eq!(row.usd_input, first.usd_input);
    }

    /// 按时刻取价：同一个键的两段,分界前后各取各的（**S1 验收的核心夹具**）。
    #[test]
    fn segments_are_picked_by_time() {
        const T: i64 = 1_800_000_000; // 降价生效时刻
        let rows = two_segment_fixture(T);
        with_rows(&rows, || {
            // 分界之前按旧价
            let before = price_at(Platform::Claude, "claude-widget-1", T - 1).unwrap();
            assert_eq!((before.usd_input, before.usd_output), (10.0, 50.0));
            // 分界当刻与之后按新价（effective_from <= at ⇒ 当刻已生效）
            for at in [T, T + 1, T + 86_400] {
                let after = price_at(Platform::Claude, "claude-widget-1", at).unwrap();
                assert_eq!((after.usd_input, after.usd_output), (1.0, 5.0), "at={at}");
            }
            // 同一张表里没降价的那个键:两个时刻取到的是同一条
            let a = price_at(Platform::Claude, "claude-steady-1", T - 1).unwrap();
            let b = price_at(Platform::Claude, "claude-steady-1", T + 86_400).unwrap();
            assert_eq!((a.usd_input, a.effective_from), (3.0, b.effective_from));
        });
    }

    /// 出厂种子折回世代 2 的相对权重必须逐项相同——**零回归的锚**。
    /// 生成器侧有同样的对拍（`--crosscheck`,基准是从 cost.rs 机械抽出的冻结副本）,
    /// 这里再钉一遍,免得有人手改种子绕过生成器。
    #[test]
    fn seed_reproduces_the_v2_relative_weights() {
        let now = 1_790_000_000;
        let w = |p: Platform, model: &str, unit: f64| {
            let r = price_at(p, model, now).unwrap();
            (r.usd_input / unit, r.usd_output / unit)
        };
        // Claude 基准 = Sonnet 5 输入价 2.0
        assert_eq!(w(Platform::Claude, "claude-sonnet-5", 2.0), (1.0, 5.0));
        assert_eq!(w(Platform::Claude, "claude-opus-5", 2.0), (2.5, 12.5));
        assert_eq!(w(Platform::Claude, "claude-sonnet-4-5-20250929", 2.0), (1.5, 7.5));
        assert_eq!(w(Platform::Claude, "claude-fable-5-1", 2.0), (5.0, 25.0));
        assert_eq!(w(Platform::Claude, "claude-haiku-4-5", 2.0), (0.5, 2.5));
        // Codex 基准 = gpt-5 输入价 1.25
        assert_eq!(w(Platform::Codex, "gpt-5", 1.25), (1.0, 8.0));
        assert_eq!(w(Platform::Codex, "gpt-5.6-sol", 1.25), (3.2, 16.0));
        assert_eq!(w(Platform::Codex, "gpt-5.6-terra", 1.25), (1.6, 9.6));
        assert_eq!(w(Platform::Codex, "gpt-5.6-luna", 1.25), (0.16, 0.96));
        assert_eq!(w(Platform::Codex, "gpt-6-astra", 1.25), (8.0, 40.0));
        // 缓存两项:Fable 5.1 的缓存读是自身输入价的 0.025,全系只有它这样
        let f51 = price_at(Platform::Claude, "claude-fable-5-1", now).unwrap();
        let f5 = price_at(Platform::Claude, "claude-fable-5", now).unwrap();
        assert!((f51.usd_cache_read / f51.usd_input - 0.025).abs() < 1e-12);
        assert!((f5.usd_cache_read / f5.usd_input - 0.1).abs() < 1e-12);
        // OpenAI 在 gpt-5.6 之前不对缓存写计价（上游根本没有这一项）
        assert_eq!(price_at(Platform::Codex, "gpt-5", now).unwrap().usd_cache_write, 0.0);
        assert_eq!(price_at(Platform::Codex, "gpt-5.3-codex", now).unwrap().usd_cache_write, 0.0);
        let sol = price_at(Platform::Codex, "gpt-5.6-sol", now).unwrap();
        assert!((sol.usd_cache_write / sol.usd_input - 1.25).abs() < 1e-12);
    }

    #[test]
    fn rows_for_lists_everything_sorted() {
        let claude = rows_for(Platform::Claude);
        let codex = rows_for(Platform::Codex);
        assert_eq!(claude.len() + codex.len(), factory_seed().len());
        assert!(claude.windows(2).all(|w| w[0].match_key <= w[1].match_key));
        assert!(claude.iter().all(|r| r.platform == "claude"));
    }
}
