//! 取数时机：按**预计消耗**驱动,而不是按固定间隔。
//!
//! 链路：采集线程提交后把分模型 token 交过来 → cost.rs 折成加权代价 → calib.rs 的
//! 标定系数换成「距上次读数大约消耗了百分之几」→ 达到阈值就排一次取数。
//!
//! 三条规则：
//! - **复工即取**：本地 token 静默 `QUIET_SECS` 之后的第一笔,不看量直接排。
//!    5h 是滚动窗口,离开期间旧用量不断过期,读数在没有新消耗时也会漂;
//!    标定样本要求两次读数间隔 ≤ 30 分钟,复工先取一轮把样本起点重置到工作开始,
//!   否则第一条样本会横跨整段空闲期而作废（见 calib.rs）。
//! - **达阈即取**：预计消耗 ≥ 阈值（开「余量低时收紧」后,5h 剩余 ≤ `LOW_REMAINING_PCT`
//!   时阈值减半）。
//! - **最小间隔**：两次取数**尝试**之间至少 `MIN_GAP_SECS`。起点是尝试时刻而非上次成功时刻,
//!   否则取数连续失败时间隔判据永远成立,每个采集轮都会再打一次。
//!
//! 账目在取到**新读数**时才清零（失败轮不清）;清零前的累计代价与分模型明细交给 calib.rs
//! 落一条标定样本。没进展的轮（取数失败 / 判死 / 读数没推进）把应检时刻按 `backoff_secs`
//! 指数推后：需求仍挂着,但故障期间不会按采集频率空转重试,也不会因「应检时刻永远是过去」
//! 而把触发来源恒判成 token（那会让兜底轮的零请求探测失效）。

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use super::cost::{self, Tokens};
use super::model::Platform;

/// 两次取数**尝试**的最小间隔（秒）。
pub const MIN_GAP_SECS: i64 = 60;

/// 没进展时的退避封顶（秒;= 兜底档封顶,再久也没有意义——兜底轮会兜住）。
const MAX_BACKOFF_SECS: i64 = 1800;

/// 退避的最大翻倍次数（60 → 120 → … → 1920,与封顶取小）。
const BACKOFF_MAX_SHIFT: u32 = 5;

/// 第 `streak` 次没进展后的退避时长（秒）。
fn backoff_secs(streak: u32) -> i64 {
    let shift = streak.saturating_sub(1).min(BACKOFF_MAX_SHIFT);
    (MIN_GAP_SECS << shift).min(MAX_BACKOFF_SECS)
}

/// 「离开」判据：本地 token 静默这么久即认为用户离开了 agent 工作。
/// 同一个数同时是悬浮球待机（idle.rs,安静起点由 `note_local_tokens` 经 `nudge_standby`
/// 同步推进）与「复工即取一轮」的判据,两者是同一件事,不另设常量。
pub const QUIET_SECS: i64 = 600;

/// 「余量低」的判据：5h 剩余 ≤ 此值时阈值减半（开关默认开）。
pub const LOW_REMAINING_PCT: f64 = 20.0;

/// 阈值默认值与合法域（百分点;设置页 0.5 步进）。
///
/// 单位是「5h 窗口的百分点」,而两个平台满窗的 API 等价用量相差约一个数量级
/// （Claude Max ≈ $110,Codex ≈ $11〜14）,阈值过小会让 Codex 为几毛钱的用量就请求一次。
/// 读数新鲜度不依赖它：Codex 由 `codex_rollout:update_snapshot` 从本地 rollout 零请求推进,
/// 阈值只决定何时向服务端要一次权威读数。
///
/// 两平台共用一个阈值：语义是「最多容忍多少百分点的显示滞后」,按配额百分比而非金额定。
pub const DEFAULT_THRESHOLD_PCT: f64 = 5.0;
const MIN_THRESHOLD_PCT: f64 = 0.5;
const MAX_THRESHOLD_PCT: f64 = 10.0;

/// 运行时策略（前端 `set_subscription_fetch_policy` 下发;阈值存千分位整数避免浮点原子）。
static THRESHOLD_MILLI: AtomicU64 = AtomicU64::new((DEFAULT_THRESHOLD_PCT * 1000.0) as u64);
static TIGHTEN_WHEN_LOW: AtomicBool = AtomicBool::new(true);

pub fn threshold_pct() -> f64 {
    THRESHOLD_MILLI.load(Ordering::SeqCst) as f64 / 1000.0
}

pub fn tighten_when_low() -> bool {
    TIGHTEN_WHEN_LOW.load(Ordering::SeqCst)
}

/// 下发策略（越界钳制;返回钳制后的实际值供日志）。
pub fn set_policy(threshold_pct: f64, tighten: bool) -> (f64, bool) {
    let pct = threshold_pct.clamp(MIN_THRESHOLD_PCT, MAX_THRESHOLD_PCT);
    THRESHOLD_MILLI.store((pct * 1000.0).round() as u64, Ordering::SeqCst);
    TIGHTEN_WHEN_LOW.store(tighten, Ordering::SeqCst);
    (pct, tighten)
}

/// 生效的阈值（余量低且开关开 → 减半）。
pub fn effective_threshold(remaining_5h: Option<f64>) -> f64 {
    let base = threshold_pct();
    let low = remaining_5h.is_some_and(|r| r <= LOW_REMAINING_PCT);
    if low && tighten_when_low() { base / 2.0 } else { base }
}

/// 单平台账目（自上次成功取数以来）。
#[derive(Debug, Default, Clone)]
struct Track {
    /// 加权代价累计（cost.rs 口径）。
    cost: f64,
    /// 其中来自未知模型的代价（标定时用来判可信度）。
    unknown_cost: f64,
    /// 分模型 token 明细（落标定样本时一并存,留作将来拟合逐模型权重）。
    breakdown: BTreeMap<String, [i64; 4]>,
    /// 账目起点 = 上次成功取数时刻（0 = 从未）。
    since: i64,
    /// 上次收到本地 token 的时刻（0 = 从未）。
    last_token_at: i64,
    /// 上次**尝试**取数的时刻（0 = 从未;最小间隔的起点,成功与否都推进）。
    last_attempt_at: i64,
    /// 连续没进展的轮数（退避指数;取到新读数即清零）。
    fail_streak: u32,
    /// 已排定但尚未执行的应检时刻（0 = 无）。
    due: i64,
}

static TRACKS: LazyLock<Mutex<HashMap<Platform, Track>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn tracks() -> std::sync::MutexGuard<'static, HashMap<Platform, Track>> {
    // poison 容忍：持锁线程 panic 后调度表仍可用。
    TRACKS.lock().unwrap_or_else(|e| e.into_inner())
}

/// 取走的账目（成功取数时用来落标定样本）。
pub struct Account {
    pub cost: f64,
    pub unknown_cost: f64,
    pub since: i64,
    pub breakdown_json: String,
}

/// 纯逻辑：把一笔分模型 token 记进账,并算出应检时刻（None = 还不值得取数）。
fn plan(
    t: &mut Track,
    platform: Platform,
    usage: &BTreeMap<String, [i64; 4]>,
    now: i64,
    threshold: f64,
    scale: f64,
) -> Option<i64> {
    let edge = t.last_token_at == 0 || now - t.last_token_at >= QUIET_SECS;
    let mut added = 0.0;
    for (model, v) in usage {
        let tokens = Tokens { input: v[0], output: v[1], cache_read: v[2], cache_write: v[3] };
        if tokens.total() == 0 {
            continue;
        }
        // at = now：在线路的 token 按到达时刻取价
        let (c, known) = cost::cost_of(platform, model, &tokens, now);
        added += c;
        if !known {
            t.unknown_cost += c;
        }
        let slot = t.breakdown.entry(model.clone()).or_insert([0; 4]);
        for i in 0..4 {
            slot[i] += v[i];
        }
    }
    if added <= 0.0 {
        return None;
    }
    t.cost += added;
    t.last_token_at = now;
    // 预计消耗未达阈值,也不是复工那一笔 → 只记账不取数（账目跨空闲期照常累加）
    if !edge && t.cost * scale < threshold {
        return None;
    }
    // 不早于「上次尝试 + 最小间隔」,也不早于已排定的应检时刻（退避期内新 token
    // 不把重试拉回来——否则故障期间的退避会被下一笔 token 立刻抹掉）。
    let due = now.max(t.last_attempt_at + MIN_GAP_SECS).max(t.due);
    t.due = due;
    Some(due)
}

/// 采集线程报告本地新增 token（每源一轮一次）。
/// 返回 Some（应检时刻) = 该平台需要取数,调用方据此把主轮询的应检时刻提前。
pub fn note_tokens(
    platform: Platform,
    usage: &BTreeMap<String, [i64; 4]>,
    now: i64,
    remaining_5h: Option<f64>,
) -> Option<i64> {
    let threshold = effective_threshold(remaining_5h);
    let scale = super::calib::scale(platform);
    let mut g = tracks();
    let t = g.entry(platform).or_default();
    plan(t, platform, usage, now, threshold, scale)
}

/// 该平台是否有已到期的取数需求（主轮询每轮问一次）。
pub fn due_now(platform: Platform, now: i64) -> bool {
    tracks().get(&platform).is_some_and(|t| t.due > 0 && now >= t.due)
}

/// 距上次读数的预计消耗。
pub fn estimated_pct(platform: Platform) -> f64 {
    let scale = super::calib::scale(platform);
    tracks().get(&platform).map_or(0.0, |t| t.cost * scale)
}

/// 纯逻辑：记一轮取数尝试（见 `note_attempt`）。
fn record_attempt(t: &mut Track, now: i64, advanced: bool) {
    t.last_attempt_at = now;
    if advanced {
        t.fail_streak = 0;
        return;
    }
    // 需求仍挂着（账目不清,下轮照样该取）,只是把重试推后。
    // 只推**已到期**的需求：兜底轮 / 手动刷新轮与尚未到期的 token 需求无关,
    // 拿失败去改它会把一个更晚的应检时刻提前（backoff 反而成了提前量）。
    if t.due > 0 && now >= t.due {
        t.fail_streak = t.fail_streak.saturating_add(1);
        t.due = now + backoff_secs(t.fail_streak);
    }
}

/// 一轮取数尝试落幕（主轮询每检一个平台调一次,含判死 / 零请求短路的轮）。
/// `advanced` = 拿到了**更新的读数**。没进展就把应检时刻按退避推后——
/// 否则应检时刻永远停在过去,故障期间每个采集轮都会重试一次,而且触发来源会被
/// 恒判成 token（兜底轮的零请求探测因此永久失效）。
pub fn note_attempt(platform: Platform, now: i64, advanced: bool) {
    let mut g = tracks();
    record_attempt(g.entry(platform).or_default(), now, advanced);
}

/// 成功取到新读数：取走账目（供落标定样本）并清零,记下新的账目起点。
pub fn take_account(platform: Platform, now: i64) -> Account {
    let mut g = tracks();
    let t = g.entry(platform).or_default();
    let breakdown_json = serde_json::to_string(&t.breakdown).unwrap_or_else(|_| "{}".into());
    let acc = Account { cost: t.cost, unknown_cost: t.unknown_cost, since: t.since, breakdown_json };
    t.cost = 0.0;
    t.unknown_cost = 0.0;
    t.breakdown.clear();
    t.due = 0;
    t.since = now;
    acc
}

/// 解绑 / 关闭时清账（复绑从零重建,不拿旧账误触发）。
pub fn prune(bound: &[Platform]) {
    tracks().retain(|p, _| bound.contains(p));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 5 %/美元当量,即「$0.4 的 API 等价用量 ≈ 2% 配额」。
    /// 比出厂预设（`PRIOR_SCALE` = 1 %/美元）刻意放大 5 倍,让夹具用整数 token 量
    /// 恰好压在阈值上：5 万 Sonnet 输入 = $0.1 = 0.5%,四笔正好 2%。
    const SCALE: f64 = 5.0;

    fn usage(model: &str, input: i64) -> BTreeMap<String, [i64; 4]> {
        BTreeMap::from([(model.to_string(), [input, 0, 0, 0])])
    }

    #[test]
    fn first_tokens_after_quiet_fetch_immediately() {
        let mut t = Track::default();
        let due = plan(&mut t, Platform::Claude, &usage("claude-sonnet-5", 1_000), 10_000, 2.0, SCALE);
        assert_eq!(due, Some(10_000), "从未取过:复工即取");
    }

    #[test]
    fn short_gap_is_judged_by_threshold_only() {
        // 停手 5 分钟 < QUIET_SECS:不算复工,照旧只看累计消耗,且账目跨空闲期不清零
        let mut t = Track { since: 1_000, last_token_at: 1_000, last_attempt_at: 1_000, ..Default::default() };
        assert_eq!(
            plan(&mut t, Platform::Claude, &usage("claude-sonnet-5", 150_000), 1_030, 2.0, SCALE),
            None,
            "1.5% 不到阈值"
        );
        let after_gap = 1_030 + 300;
        assert_eq!(
            plan(&mut t, Platform::Claude, &usage("claude-sonnet-5", 60_000), after_gap, 2.0, SCALE),
            Some(after_gap),
            "空闲 5 分钟后再来 0.6%:与之前累加过阈值 → 取数"
        );
    }

    #[test]
    fn long_gap_fetches_regardless_of_volume() {
        let mut t = Track { since: 1_000, last_token_at: 1_000, last_attempt_at: 1_000, ..Default::default() };
        let after_gap = 1_000 + QUIET_SECS;
        assert_eq!(
            plan(&mut t, Platform::Claude, &usage("claude-sonnet-5", 100), after_gap, 2.0, SCALE),
            Some(after_gap),
            "静默满 10 分钟后的第一笔:不看量直接取"
        );
    }

    #[test]
    fn small_usage_only_accrues_until_threshold() {
        let mut t = Track { since: 1_000, last_token_at: 1_000, last_attempt_at: 1_000, ..Default::default() };
        // 每笔 5 万 sonnet 输入 = $0.1 → 预计 0.5%,不到 2%
        for round in 1..=3 {
            let now = 1_000 + round * 30;
            assert_eq!(
                plan(&mut t, Platform::Claude, &usage("claude-sonnet-5", 50_000), now, 2.0, SCALE),
                None,
                "第 {round} 笔仍未达阈值"
            );
        }
        // 第 4 笔累计到 $0.4 = 2% → 排程,且不早于上次取数 + 60 秒
        let due = plan(&mut t, Platform::Claude, &usage("claude-sonnet-5", 50_000), 1_120, 2.0, SCALE);
        assert_eq!(due, Some(1_120));
    }

    #[test]
    fn expensive_model_reaches_threshold_far_sooner() {
        let mut t = Track { since: 1_000, last_token_at: 1_000, last_attempt_at: 1_000, ..Default::default() };
        // 8 万 Opus 输入 = $0.4 = 2%（Sonnet 要 20 万 token 才到;单价 2.5 倍）
        let due = plan(&mut t, Platform::Claude, &usage("claude-opus-5", 80_000), 1_100, 2.0, SCALE);
        assert_eq!(due, Some(1_100), "贵模型少量 token 即达阈值");
    }

    #[test]
    fn min_gap_counts_from_the_attempt_not_the_last_success() {
        // 取数连续失败:since 停在 1_000 不动,但尝试时刻照常推进 → 间隔判据仍然生效
        let mut t = Track { since: 1_000, last_token_at: 1_000, last_attempt_at: 5_000, ..Default::default() };
        let due = plan(&mut t, Platform::Claude, &usage("claude-opus-5", 100_000), 5_010, 2.0, SCALE);
        assert_eq!(due, Some(5_060), "距上次**尝试**不足 60 秒 → 推到间隔满足");
    }

    #[test]
    fn stalled_round_backs_off_instead_of_retrying_every_round() {
        let mut t = Track { since: 1_000, last_token_at: 1_000, last_attempt_at: 1_000, ..Default::default() };
        // 达阈排一轮
        assert_eq!(
            plan(&mut t, Platform::Claude, &usage("claude-opus-5", 100_000), 5_000, 2.0, SCALE),
            Some(5_000)
        );
        // 第一次没进展:应检时刻退到 +60,需求与账目都还在
        record_attempt(&mut t, 5_000, false);
        assert_eq!(t.due, 5_060);
        assert!(t.cost > 0.0, "失败轮不清账");
        // 退避期内又来一笔 token → 不能把重试拉回当下
        assert_eq!(
            plan(&mut t, Platform::Claude, &usage("claude-opus-5", 100_000), 5_010, 2.0, SCALE),
            Some(5_060),
            "退避时刻不被新 token 抹掉"
        );
        // 连续没进展 → 指数推后
        record_attempt(&mut t, 5_060, false);
        assert_eq!(t.due, 5_060 + 120);
        // 拿到新读数 → 退避计数清零
        record_attempt(&mut t, 9_000, true);
        assert_eq!(t.fail_streak, 0);
        assert_eq!(t.last_attempt_at, 9_000);
    }

    #[test]
    fn a_stalled_round_never_pulls_an_unripe_due_earlier() {
        // 兜底轮失败时,尚未到期的 token 需求（+500 秒）不该被改成 +60 秒
        let mut t = Track { since: 1_000, last_token_at: 1_000, last_attempt_at: 1_000, due: 5_500, ..Default::default() };
        record_attempt(&mut t, 5_000, false);
        assert_eq!(t.due, 5_500, "未到期的需求不受本轮失败影响");
        assert_eq!(t.fail_streak, 0);
    }

    #[test]
    fn a_stalled_fallback_round_schedules_nothing() {
        // 没有挂起需求的轮（纯兜底轮）没取到数 → 只推进尝试时刻,不凭空排程
        let mut t = Track { since: 1_000, last_token_at: 1_000, last_attempt_at: 1_000, ..Default::default() };
        record_attempt(&mut t, 4_000, false);
        assert_eq!(t.due, 0);
        assert_eq!(t.fail_streak, 0);
        assert_eq!(t.last_attempt_at, 4_000);
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        assert_eq!(backoff_secs(1), 60);
        assert_eq!(backoff_secs(2), 120);
        assert_eq!(backoff_secs(5), 960);
        assert_eq!(backoff_secs(6), MAX_BACKOFF_SECS, "1920 与封顶取小");
        assert_eq!(backoff_secs(99), MAX_BACKOFF_SECS);
    }

    #[test]
    fn min_gap_delays_a_burst() {
        let mut t = Track { since: 1_000, last_token_at: 1_000, last_attempt_at: 1_000, ..Default::default() };
        let due = plan(&mut t, Platform::Claude, &usage("claude-opus-5", 100_000), 1_020, 2.0, SCALE);
        assert_eq!(due, Some(1_060), "距上次取数不足 60 秒 → 推到间隔满足");
    }

    #[test]
    fn unknown_model_cost_is_flagged() {
        let mut t = Track { since: 1_000, last_token_at: 1_000, last_attempt_at: 1_000, ..Default::default() };
        plan(&mut t, Platform::Claude, &usage("sol-preview", 50_000), 1_030, 2.0, SCALE);
        assert!(t.unknown_cost > 0.0 && (t.unknown_cost - t.cost).abs() < 1e-9, "未知模型代价全额记为不可信");
    }

    #[test]
    fn threshold_tightens_only_when_low_and_enabled() {
        set_policy(2.0, true);
        assert_eq!(effective_threshold(Some(50.0)), 2.0);
        assert_eq!(effective_threshold(Some(LOW_REMAINING_PCT)), 1.0, "剩余 ≤ 20% → 减半");
        assert_eq!(effective_threshold(None), 2.0, "拿不到剩余量按原阈值");
        set_policy(2.0, false);
        assert_eq!(effective_threshold(Some(5.0)), 2.0, "开关关掉不收紧");
        set_policy(99.0, true);
        assert_eq!(threshold_pct(), MAX_THRESHOLD_PCT, "越界钳制");
        set_policy(DEFAULT_THRESHOLD_PCT, true);
    }

    #[test]
    fn take_account_clears_and_restarts() {
        let p = Platform::Codex;
        note_tokens(p, &usage("gpt-5-codex", 10_000), 5_000, None);
        let acc = take_account(p, 5_100);
        assert!(acc.cost > 0.0);
        assert!(acc.breakdown_json.contains("gpt-5-codex"));
        assert_eq!(estimated_pct(p), 0.0, "取数后账目清零");
        assert!(!due_now(p, 9_999));
        prune(&[]);
    }
}
