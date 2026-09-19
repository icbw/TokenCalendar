//! 标定：从「两次读数之间的本地代价 vs 用量涨幅」归纳出该平台的换算系数
//!。
//!
//! 模型只有**一个**未知数：
//! ```text
//! 预计Δ用量% = 标定系数 s × 代价 （代价 = 官方价目下的美元当量,见 cost.rs）
//! ```
//! 一个未知数意味着两次读数就能估、几次之后就稳——把「哪个模型贵多少」交给**官方
//! 价目**（`price_model` 表,按模型带生效时间）,把「这个套餐一个窗口有多大」交给 s。
//! 套餐升级 / 官方调整限额,滑动窗口自动跟上。
//!
//! `s` 的单位是**百分点 / 美元当量**：「每 1 美元的 API 等价用量吃掉
//! 配额的百分之几」。本机：Claude Max ≈ 0.89（满 5h 窗 ≈ $112 的 API 等价用量）,
//! Codex edu/plus ≈ 8.88（≈ $11）——**出厂预设因此按平台分开**,见 `cost:prior_scale`。
//!
//! **出厂预设 → 校准是连续过渡**（用户口径）：
//! ```text
//! s = （出厂预设 × PRIOR_WEIGHT + Σ Δ用量 / Σ 代价 × n) / （PRIOR_WEIGHT + n)
//! ```
//! n = 有效样本数。n=0 时纯出厂预设,样本越多预设占比越小,没有「切换」这一刻。
//!
//! **读数是整数百分比**：两个平台的适配器都按 `as_f64` 解析,
//! 但平台实际给的就是整数。按本机 `scale`,**1% 配额 ≈ $1.08 等价用量**——这是读数的
//! 最小刻度。区间越短,「已记进 cost 的 token」与「平台计数器还没涨上来的读数」之间的
//! 滞后占比越大,于是短区间的样本隐含比值偏低。
//!
//! **但这属于数据筛选 / 加权的问题,不该用「拉长区间」去解**：
//! 区间长度同时也是监测节奏的量,拿它当筛子会把「多久看一次」和「哪些样本可信」搅在
//! 一起,而且靠**丢样本**噪声与既定原则相反（样本只增不删,见 AGENTS.md 与
//! `store:pairs_for_fit` 的注释）。现有的 `MIN_PAIR_SECS = 30` 维持不变;量化带来的
//! 偏差要在筛选层解决,方向
//! []。
//!
//! 样本筛选（不合格的样本比没有样本更糟）：
//! - **跨过 5h 窗口重置** → 丢。判据优先用读数自带的 `resets_at`：重置时它会整体
//!   前移,`resets5_1 > resets5_0` 就是直接证据。来源不给这个字段时（Claude 桌面端
//!   采样只有两个百分比）才退回「读数变小了」去推断。
//! - **Δ5h < 0** → 丢：没有重置证据也变小了 = 滚动过期占了主导。
//! - **Δ5h = 0 → 收**：读数是整数百分比,
//!   一段消耗不够动一个百分点时 Δ 就是 0,而它的 `cost` 是真实发生的。拟合用的是
//!   「和之比」、整数舍入近似零均值 ⇒ **把这种区间计回分母才更接近无偏**;一起丢掉
//!   会系统性抬高系数（Codex 偏高 10〜25%,Claude 1%）。
//!   **但只收「本来就不该动」的那些**：`代价 × 参照系数 ≤ 2 个刻度`。预计要涨好几个
//!   百分点却纹丝不动,那不是量化,是账目不对（读数陈旧 / token 记错了区间）——照丢。
//! - 间隔过长（> `MAX_PAIR_SECS`）：5h 是滚动窗口,间隔越长「过期掉的旧用量」越会
//!   吃掉新增,系统性低估 s → 丢;间隔过短（< `MIN_PAIR_SECS`）读数还没跟上 → 丢;
//! - 代价为 0：本地没花 token 却涨了 = 在线 / 网页用量,不能用来标定本地换算 → 丢;
//! - 未知模型占比过半：权重不可信 → 丢（但这种样本照常参与触发估算）;
//! - **隐含比值离谱**（Δ/代价不在参照系数的 `RATIO_MIN_FACTOR`〜`RATIO_MAX_FACTOR` 倍之间）：
//!   代价与涨幅严重不成比例,说明这个区间的账目本身不对——要么混进了不属于该区间的
//!   token（首轮回填 / 整会话重建 / 故障期累积的旧账）,要么涨幅几乎全来自在线用量。
//!   这种样本比没有样本更糟：拟合用的是「和之比」,单条巨额代价能把系数一把拉到近零
//!。
//!
//! **可信带的中心是自举出来的**：带宽本身（0.05〜20 倍）是留给套餐差异的,
//! 但它得围着**这个平台真实的量级**才对称。此前一律以一个共用常量（在 Claude Max 档上
//! 的 ≈1 %/美元）为心,而 Codex ≈9 —— 于是 Codex 的上界只到真值的 2.2 倍、
//! 下界低到真值的 0.6%,**上尾被系统性截掉**,方向与丢 Δ=0 恰好相反。所以 [`fit`] 先用
//! 出厂预设筛一遍得到量级,再以拟合值为心重筛,至多 `FIT_PASSES` 轮、变动小于
//! `FIT_CONVERGE` 即停。出厂预设本身也已改成 per-platform（`cost:prior_scale`）,
//! 于是第一遍就站在对的量级上;自举仍然保留——它管的是**套餐差异**,而预设管不了那个。
//!
//! 离群值：样本数够时对逐样本比值做两端截尾,再用「和之比」拟合（大样本自然占更大
//! 权重,个别噪声影响小）。**截尾只在 Δ>0 的样本里做**：Δ=0 的比值恒为 0、会整片堆在
//! 最低端,一起排序截尾等于专挑它们丢,又把刚掉的偏差请回来。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use super::cost::prior_scale;
use super::model::Platform;

/// 出厂预设的「样本当量」：样本累计到这个数时,预设与各占一半。
const PRIOR_WEIGHT: f64 = 5.0;

/// 认为「已按校准」的样本数门槛（前端状态行用）。
pub const CALIBRATED_PAIRS: u32 = 5;

/// 可用于标定的配对间隔（秒）。
///
/// **下界保持 30,不要为了降噪去抬它**：短区间的样本确实偏低
/// （见模块头的量化说明）,但区间长度同时也是**监测节奏**的量——用户在快速轮次里、
/// 尤其用贵模型时,取数本来就密,抬下界等于把那段最该学到东西的时间整片丢掉。
/// 量化偏差属于筛选 / 加权层的问题,在那一层解决。
///
/// 上界 1800 是另一回事,有实质理由：5h 是滚动窗口,间隔越长「过期掉的旧用量」越会
/// 吃掉新增,系统性低估 `scale`。
///
/// 改这两个数**不需要**升 `cost:WEIGHT_VERSION`：它们是拟合时的准入判据,不参与
/// `cost` 的计算,库里一个数都不会变。
pub(super) const MIN_PAIR_SECS: i64 = 30;
pub(super) const MAX_PAIR_SECS: i64 = 1800;

/// 隐含比值（Δ用量% / 代价）的可信带,相对**参照系数**的倍数。越界的样本不参与拟合。
const RATIO_MIN_FACTOR: f64 = 0.05;
const RATIO_MAX_FACTOR: f64 = 20.0;

/// 额度读数的最小刻度（百分点）：两个平台给的都是整数百分比。
const READING_STEP_PCT: f64 = 1.0;

/// Δ=0 样本的代价上限,按刻度的倍数算：预计涨幅超过这么多个百分点却一动不动,
/// 就不是「量化吃掉了」而是账目不对。给到 2 是因为跨刻度边界时真实涨幅可以逼近
/// 一整格仍显示为 0,留一格余量。
const ZERO_DELTA_SLACK: f64 = 2.0;

/// 可信带中心自举的最多轮数,与「够稳了」的相对变动阈值。
const FIT_PASSES: usize = 4;
const FIT_CONVERGE: f64 = 0.01;

/// **样本准入判据的版本**。
///
/// 两条回溯路（`bootstrap` / `codex_rollout`）在**建样本时**就按判据筛过一道,不合格的
/// 区间根本没落库 ⇒ 判据一放宽,存量里缺的那些样本不会自己长出来。所以这个数一升,
/// 两路各自把**源还在的那一段**按新判据就地重建一次：先按新判据建出样本,再删掉
/// 「重建覆盖到的那一段」里的旧行。**区间边界由重建出来的样本自己给**——源已经消失、
/// 这次重建不到的区间落在边界之外,原样留在库里（读数本身永久留在 `desktop_sample`,
/// 是这条路能重建的前提）。
///
/// 与 `cost:WEIGHT_VERSION` 是两回事：那个管「`cost` 是按哪把尺子量的」,存量行按原始
/// token 就地重算即可;这个管「哪些区间算数」,而被判成不算数的区间当初压根没留下来。
///
/// 版本史：
/// - 1 = 起：Δ>0 才收;可信带以出厂预设为心。
/// - 2 = ：Δ=0 且「本来就不该动一格」的区间收进分母;
///   窗口重置改用读数自带的窗尾直接判;可信带的心由涨幅样本自举。
pub const ADMISSION_RULE_VERSION: i64 = 2;

/// 一条标定样本（= 相邻两次成功读数之间的观测）。
#[derive(Debug, Clone, Copy)]
pub struct Pair {
    pub t0: i64,
    pub t1: i64,
    /// 5h 窗口在两端的已用百分比。
    pub used5_0: f64,
    pub used5_1: f64,
    /// 5h 窗口在两端各自申报的重置时刻（`None` = 该来源不提供,见 `window_reset`）。
    /// 存原值而不存「是否重置过」：判据将来要改时,这两个数还能重新判一遍。
    pub resets5_0: Option<i64>,
    pub resets5_1: Option<i64>,
    /// 期间的本地加权代价（cost.rs 口径）。
    pub cost: f64,
    /// 其中来自未知模型的代价（占比过半即不参与标定）。
    pub unknown_cost: f64,
}

impl Pair {
    fn delta(&self) -> f64 {
        self.used5_1 - self.used5_0
    }

    /// 区间内是否跨过了 5h 窗口的重置。
    ///
    /// 窗口重置时平台申报的重置时刻会整体前移,所以 `resets5_1 > resets5_0` 是
    /// **直接证据**,不必靠「读数变小了」去推断——后者对「重置后又涨回同一个整数」
    /// （Δ=0）和「重置后涨得比之前还高」（Δ>0）都无能为力,而这两种区间的 Δ 与
    /// 这段消耗根本不是一回事。来源不给这个字段时返回 false,由 `delta < 0` 兜底。
    fn window_reset(&self) -> bool {
        matches!((self.resets5_0, self.resets5_1), (Some(a), Some(b)) if b > a)
    }

    /// 是否可用于标定（语义见模块头）。
    ///
    /// `center` 是判「离谱」时的参照系数：该平台当前的系数,还没有就是出厂预设。
    /// 它只当**尺度**用（带的两端、Δ=0 的代价上限都是它的倍数）,不参与拟合本身。
    pub fn usable(&self, center: f64) -> bool {
        let dt = self.t1 - self.t0;
        if dt < MIN_PAIR_SECS || dt > MAX_PAIR_SECS {
            return false;
        }
        if self.cost <= 0.0 || self.unknown_cost > self.cost / 2.0 {
            return false;
        }
        if self.window_reset() {
            return false; // 跨重置:两端读数不在同一个窗口里,Δ 没有意义
        }
        let delta = self.delta();
        if delta < 0.0 {
            return false; // 没有重置证据也变小了 = 滚动过期占了主导
        }
        if delta == 0.0 {
            // 消耗不够动一个刻度 ⇒ 有效样本,计回分母（语义见模块头）
            return self.cost * center <= READING_STEP_PCT * ZERO_DELTA_SLACK;
        }
        let ratio = delta / self.cost;
        ratio >= center * RATIO_MIN_FACTOR && ratio <= center * RATIO_MAX_FACTOR
    }
}

/// 由样本拟合系数（纯逻辑,单测直接覆盖）。返回 （系数, 参与拟合的样本数)。
///
/// 两步：
/// 1. **只用 Δ>0 的样本把可信带的中心自举出来**——先以出厂预设为心筛一遍拿到量级,
///    再以拟合值为心重筛,至多 `FIT_PASSES` 轮（带宽是相对量,心歪了就会偏着截,
///    语义见模块头）;
/// 2. 以这个中心再筛一遍,这次**把 Δ=0 的样本一并收进分母**。
///
/// 两步分开是必要的,不是为了好看：Δ=0 的准入判据（`代价 × 中心 ≤ 2 个刻度`）本身
/// 依赖中心,而收进来的 Δ=0 又会把中心压低 ⇒ 若放在同一个迭代里,就成了「收得越多、
/// 心越低、收得更多」的正反馈。让中心只由涨幅样本决定,这条回路就断了——语义上也正是
/// 想问的那句话：**按涨幅样本给出的尺子,这个区间本来该不该动一格?**
///
/// 一条 Δ>0 的样本都没有时返回出厂预设：Δ=0 只说明「不够一格」,没有尺子就定不出量级。
pub fn fit(platform: Platform, pairs: &[Pair]) -> (f64, u32) {
    let prior = prior_scale(platform);
    let (mut center, mut n) = fit_at(prior, pairs, prior, false);
    if n == 0 {
        return (prior, 0);
    }
    for _ in 1..FIT_PASSES {
        let (c2, n2) = fit_at(prior, pairs, center, false);
        if n2 == 0 {
            break; // 新的带把样本全筛没了 ⇒ 守住上一轮的中心
        }
        let settled = n2 == n && (c2 - center).abs() <= center * FIT_CONVERGE;
        center = c2;
        n = n2;
        if settled {
            break;
        }
    }
    fit_at(prior, pairs, center, true)
}

/// 给定出厂预设与可信带中心的一轮拟合。`with_flat` = 是否把 Δ=0 的样本收进分母。
fn fit_at(prior: f64, pairs: &[Pair], center: f64, with_flat: bool) -> (f64, u32) {
    // Δ>0 与 Δ=0 分开：截尾是给「比值离群」用的,而 Δ=0 的比值恒为 0、会整片堆在
    // 最低端 —— 混在一起排序截尾等于专挑它们丢（语义见模块头）。
    let (mut grew, flat): (Vec<&Pair>, Vec<&Pair>) = pairs
        .iter()
        .filter(|p| p.usable(center))
        .partition(|p| p.delta() > 0.0);
    let flat: Vec<&Pair> = if with_flat { flat } else { vec![] };
    if grew.is_empty() && flat.is_empty() {
        return (prior, 0);
    }
    // 两端截尾（样本够多才截;每端 10%,至少留一条）
    if grew.len() >= 10 {
        grew.sort_by(|a, b| {
            (a.delta() / a.cost)
                .partial_cmp(&(b.delta() / b.cost))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let cut = grew.len() / 10;
        grew = grew[cut..grew.len() - cut].to_vec();
    }
    // Δ=0 的样本只进分母（它们的 Δ 恒为 0,进不进分子都一样）
    let sum_delta: f64 = grew.iter().map(|p| p.delta()).sum();
    let sum_cost: f64 = grew.iter().chain(flat.iter()).map(|p| p.cost).sum();
    let n = (grew.len() + flat.len()) as f64;
    let fitted = if sum_cost > 0.0 { sum_delta / sum_cost } else { prior };
    let blended = (prior * PRIOR_WEIGHT + fitted * n) / (PRIOR_WEIGHT + n);
    (blended, n as u32)
}

/// 运行时系数缓存（写 = 启动装载 / 每落一条新样本;读 = 取数时机估算,零 IO）。
static SCALES: LazyLock<Mutex<HashMap<Platform, (f64, u32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn slot() -> std::sync::MutexGuard<'static, HashMap<Platform, (f64, u32)>> {
    SCALES.lock().unwrap_or_else(|e| e.into_inner())
}

/// 当前系数（未装载过 → 出厂预设）。
pub fn scale(platform: Platform) -> f64 {
    slot().get(&platform).map_or_else(|| prior_scale(platform), |(s, _)| *s)
}

/// 当前有效样本数（前端状态行 / 诊断）。
pub fn sample_count(platform: Platform) -> u32 {
    slot().get(&platform).map_or(0, |(_, n)| *n)
}

/// 用库里**当前世代**的样本重算并缓存（启动装载与每落一条新样本后的唯一入口）。
/// 世代 = 当前权重表版本 + 当前套餐,筛选口径见 `store:pairs_for_fit`;
/// 被筛掉的样本留在库里当档案,只是不参与这一版系数的推断。
pub fn refit_from_store(store: &super::store::SubStore, platform: Platform) {
    let plan = store.load_snapshot(platform).map(|s| s.plan_type).unwrap_or_default();
    refit(platform, &store.pairs_for_fit(platform, &plan));
}

/// 由给定样本重算并缓存（纯逻辑入口;线上一律走 `refit_from_store`）。
pub fn refit(platform: Platform, pairs: &[Pair]) {
    let (s, n) = fit(platform, pairs);
    slot().insert(platform, (s, n));
    crate::dev_log!(
        "[subscription] {} calibration refit: scale={:.5} %/cost, usable={}",
        platform.as_str(),
        s,
        n
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单测一律拿 Claude 的先验当基准量：判据本身与平台无关,只与「带心是多少」有关,
    /// 所以夹具一律写成先验的倍数——硬写数值会在先验一改时整片掉出可信带
    /// （2026-09-19 把预设改成 per-platform 时就靠这一条零改动通过）。
    const PLAT: Platform = Platform::Claude;

    fn prior() -> f64 {
        prior_scale(PLAT)
    }

    fn pair(cost: f64, delta: f64) -> Pair {
        Pair {
            t0: 0,
            t1: 300,
            used5_0: 10.0,
            used5_1: 10.0 + delta,
            resets5_0: None,
            resets5_1: None,
            cost,
            unknown_cost: 0.0,
        }
    }

    /// 隐含比值 = `prior()` 的 `times` 倍的一条样本。
    ///
    /// 夹具一律用这个而不是硬写 delta：可信带的两端是 `prior()` 的倍数
    /// （`RATIO_MIN_FACTOR`〜`RATIO_MAX_FACTOR`）,硬写的数值一旦代价换了量纲就会
    /// 整片掉出带外——PHASE15 S1 把代价单位改成美元当量时就撞上过这件事。
    fn pair_at(cost: f64, times_prior: f64) -> Pair {
        pair(cost, cost * prior() * times_prior)
    }

    #[test]
    fn no_samples_means_factory_default() {
        assert_eq!(fit(PLAT, &[]), (prior(), 0));
    }

    #[test]
    fn prior_gives_way_to_measurements_gradually() {
        // 实测远高于预设:一条样本只搬动一小段,样本多了才接近实测值
        let measured = prior() * 4.0;
        let one = fit(PLAT, &[pair(100.0, 100.0 * measured)]).0;
        let many = fit(PLAT, &vec![pair(100.0, 100.0 * measured); 60]).0;
        assert!(one > prior() && one < measured, "一条样本:介于两者之间");
        assert!(many > measured * 0.9, "样本多了基本按实测走");
        assert!(fit(PLAT, &vec![pair(100.0, 100.0 * measured); 60]).1 >= 48, "截尾后仍保留绝大多数");
    }

    #[test]
    fn unusable_pairs_are_dropped() {
        let reset = Pair { used5_1: 2.0, ..pair(100.0, 0.0) }; // Δ 为负 = 窗口重置
        let online = Pair { cost: 0.0, ..pair(0.0, 5.0) }; // 本地零代价却涨了 = 在线用量
        let too_long = Pair { t1: MAX_PAIR_SECS + 31, ..pair(100.0, 1.0) };
        let unknown_heavy = Pair { unknown_cost: 80.0, ..pair(100.0, 1.0) };
        for p in [reset, online, too_long, unknown_heavy] {
            assert!(!p.usable(prior()));
        }
        assert_eq!(fit(PLAT, &[reset, online, too_long, unknown_heavy]), (prior(), 0));
    }

    #[test]
    fn outliers_are_trimmed() {
        let mut pairs = vec![pair_at(100.0, 5.0); 20];
        // 带内但偏高 / 偏低的两条（带外的会更早被 usable 挡掉,见下一个用例）
        pairs[0] = pair_at(100.0, RATIO_MAX_FACTOR * 0.975);
        pairs[1] = pair_at(100.0, RATIO_MIN_FACTOR * 2.0);
        // 以出厂预设为心的那一轮:20 条全在带内,两端各截 10%
        assert_eq!(fit_at(prior(), &pairs, prior(), true).1, 16);
        // 自举之后带心移到实测量级（≈5 倍预设）,偏低那条落到带外 ⇒ 19 条,各截 1
        let (s, n) = fit(PLAT, &pairs);
        assert_eq!(n, 17, "带心自举后重筛,再两端各截 10%");
        let plain = prior() * 5.0;
        assert!((s - (prior() * PRIOR_WEIGHT + plain * 17.0) / (PRIOR_WEIGHT + 17.0)).abs() < 1e-9);
    }

    /// 代价与涨幅严重不成比例的样本不参与拟合——首轮回填 / 故障期旧账会把几个月的
    /// token 记进一个区间,和之比拟合下单条就能把系数拉到近零（2026-09-18 审计）。
    #[test]
    fn implausible_ratio_pairs_are_dropped() {
        // 几个月的 token 记进一次读数区间 ⇒ 隐含比值远低于带下限
        let backfill = pair_at(500_000.0, RATIO_MIN_FACTOR * 0.1);
        // 涨幅几乎全来自在线用量 ⇒ 隐含比值远高于带上限
        let online_heavy = pair_at(1.0, RATIO_MAX_FACTOR * 10.0);
        assert!(!backfill.usable(prior()) && !online_heavy.usable(prior()));
        assert_eq!(fit(PLAT, &[backfill, online_heavy]), (prior(), 0), "全被挡掉 = 回到出厂预设");
        // 带内的极端值仍然照收（Pro 档的真实系数本就是 Max 档的数倍）
        let pro_tier = pair(100.0, 100.0 * prior() * 8.0);
        assert!(pro_tier.usable(prior()), "8 倍预设仍在可信带内");
        // 一条离谱样本混进 20 条正常样本:系数不应被它带跑
        let mut pairs = vec![pair_at(100.0, 5.0); 20];
        pairs[0] = backfill;
        let (s, n) = fit(PLAT, &pairs);
        assert_eq!(n, 17, "19 条可用,两端各截 1");
        assert!(s > prior() * 3.0, "仍按正常样本拟合（约 5 倍预设）,没被拉到近零:{s}");
    }

    /// Δ=0 但代价小到「本来就不该动一格」⇒ 有效样本,计回分母（PHASE15 §9-8 定案）。
    /// 一起丢掉会系统性抬高系数,因为离开分母的是**真实发生过**的那部分代价。
    #[test]
    fn a_flat_reading_below_one_step_counts_in_the_denominator() {
        let flat = pair(0.5, 0.0);
        assert!(flat.usable(prior()), "预计涨幅 0.5 个百分点,没动是正常的");
        let grew = vec![pair(1.0, 4.0); 10];
        let without = fit(PLAT, &grew);
        let with: Vec<Pair> = grew.iter().copied().chain(vec![flat; 5]).collect();
        let with = fit(PLAT, &with);
        assert_eq!(with.1, without.1 + 5, "5 条 Δ=0 全部进了样本数");
        assert!(with.0 < without.0, "计回分母 ⇒ 系数变低（不再被系统性抬高）:{with:?}");
    }

    /// 但「预计要涨好几格却纹丝不动」不是量化,是账目不对 ⇒ 照丢。
    #[test]
    fn a_flat_reading_that_should_have_moved_is_still_dropped() {
        let suspicious = pair(READING_STEP_PCT * ZERO_DELTA_SLACK * 3.0, 0.0);
        assert!(!suspicious.usable(prior()));
        // 判据随带心缩放:同一条样本,平台量级大 10 倍时预计涨幅也大 10 倍 ⇒ 更该被丢
        assert!(!suspicious.usable(prior() * 10.0));
        // 而量级小 10 倍时它确实可能一格都不到
        assert!(suspicious.usable(prior() / 10.0));
    }

    /// Δ=0 的样本不参与截尾：它们的比值恒为 0,混在一起排序就会被整片挑走,
    /// 等于把刚修掉的偏差请回来。
    #[test]
    fn flat_samples_are_not_eaten_by_the_trim() {
        let mut pairs = vec![pair(1.0, 4.0); 10];
        pairs.extend(vec![pair(0.2, 0.0); 4]);
        let (_, n) = fit(PLAT, &pairs);
        assert_eq!(n, 12, "涨幅样本两端各截 1（10 → 8）,4 条 Δ=0 一条不少");
    }

    /// 重置时刻前移 = 跨过了窗口重置,哪怕读数看着是涨的也不能用
    /// ——「涨了 3 个百分点」里有多少是重置前的,根本不知道。
    #[test]
    fn a_reset_is_judged_by_resets_at_not_by_the_reading_going_down() {
        let crossed = Pair { resets5_0: Some(1_000), resets5_1: Some(19_000), ..pair(1.0, 3.0) };
        assert!(!crossed.usable(prior()), "读数在涨,但窗口已经翻篇");
        // 重置后又涨回同一个整数 ⇒ 看起来是「不够一格」,其实是重置
        let flat_after_reset =
            Pair { resets5_0: Some(1_000), resets5_1: Some(19_000), ..pair(0.5, 0.0) };
        assert!(!flat_after_reset.usable(prior()));
        // 同一个窗口内（重置时刻没动）则照常可用
        let same_window = Pair { resets5_0: Some(19_000), resets5_1: Some(19_000), ..pair(1.0, 3.0) };
        assert!(same_window.usable(prior()));
    }

    /// 可信带的中心自举：带宽是**相对**量,心停在出厂预设上时,真实量级远高于预设的
    /// 平台（Codex 实测 ≈9 倍）上尾会被系统性截掉。
    #[test]
    fn the_band_recentres_on_the_measured_scale() {
        let mut pairs = vec![pair(1.0, prior() * 9.0); 10];
        // 比值 25 倍预设:对真实量级(9)只是 2.8 倍,却在「以预设为心」的带外(> 20)
        pairs.extend(vec![pair(1.0, prior() * 25.0); 4]);
        let one_pass = fit_at(prior(), &pairs, prior(), true);
        let bootstrapped = fit(PLAT, &pairs);
        assert_eq!(one_pass.1, 8, "以预设为心:4 条全被挡在带外,剩 10 条截尾成 8");
        assert_eq!(bootstrapped.1, 12, "自举之后 14 条都进带,截尾成 12");
        assert!(bootstrapped.0 > one_pass.0 * 1.4, "上尾不再被整片截掉:{bootstrapped:?}");
    }

    /// 只有 Δ=0 的样本时定不出量级——「不够一格」不告诉你一格有多大。
    #[test]
    fn flat_samples_alone_cannot_calibrate() {
        assert_eq!(fit(PLAT, &vec![pair(0.5, 0.0); 20]), (prior(), 0));
    }
}
