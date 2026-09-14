//! boost 监控（订阅链路的**独立补充路由**）。
//!
//! 定位：高强度使用时给悬浮球更密集的额度监控——
//! 与主轮询线程**平行**的独立线程 + 独立数据通道，不插入主轮询代码：
//! - 评估源：只读主快照库（主轮询每轮落库后,这里 1s 内看到新 fetched_at）;
//! - 取数：复用主模块 `fetch_one`（自动获得 429 冷却闸/判死零网络/idle 全套保护）;
//! - 结果：只存内存槽,**不落库、不覆盖主快照**,独立事件 `subscription:boost`
//!   推给悬浮球（前端按 fetched_at 择新 merge）;
//! - 生命周期:配置持久化在 designPrefs（prefs.json）,运行时经命令下发,
//!   重启后由 orb 窗口装载恢复;boost 状态本身不持久化（重开按主快照重评）。
//!
//! 触发条件（每平台独立,OR 关系,任一命中即进入）:
//! - A 消耗激增:相邻两轮**主轮询**快照的 used_5h 差值 ≥ 阈值
//!   （语义 = 「一个常规轮询周期内的消耗增速」）;
//! - B 低余量:5h 剩余（100 − used_5h）≤ 阈值。
//!
//! 退出:激活后先积累 5 个**成功**
//! 样本（失败轮不计,窗口顺延）,此后每轮重评——A 以「最近 5 样本累计消耗
//! （最新 − 最旧）」为判据,B 以当前剩余为判据;**所有已启用条件均不再成立
//! → 退出**。5h 窗口重置（used 骤降）时差值为负,自然满足退出。
//!
//! 节奏:激活期间按 `interval_secs`（默认 60s,可 2/3 分钟或自定义）取数;
//! 进入即首取一次。总开关关闭时清空全部激活态与内存槽。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use super::model::{Platform, SubscriptionSnapshot};
use super::{fetch_one, Adapters, SubscriptionReader};

/// boost 配置（前端 `set_subscription_boost` 下发;字段 snake_case 对齐契约风格。
/// `serde（default)` 容错:缺字段回落**业务默认**（手工 impl Default——derive 的
/// 全零默认会把 spike 阈值落到 0/false,行为漂移）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BoostConfig {
    /// 总开关（默认关——显式授权行为,与 autoUpdate 同口径）。
    pub enabled: bool,
    /// 条件 A 消耗激增（总开关开后默认开）。
    pub spike_enabled: bool,
    /// 条件 A 阈值（%;一个常规轮询周期内的消耗增速)。
    pub spike_threshold_pct: u32,
    /// 条件 B 低余量（默认关）。
    pub low_enabled: bool,
    /// 条件 B 阈值（%;5h 剩余 ≤ 此值)。
    pub low_threshold_pct: u32,
    /// 激活期间取数间隔（秒;默认 60,可 120/180 或自定义)。
    pub interval_secs: u64,
}

impl Default for BoostConfig {
    fn default() -> Self {
        default_config()
    }
}

/// 前端缺省语义（与 designPrefs DEFAULTS 对齐）:总开关关、spike 开 10%、
/// low 关 30%、间隔 60s。
pub fn default_config() -> BoostConfig {
    BoostConfig {
        enabled: false,
        spike_enabled: true,
        spike_threshold_pct: 10,
        low_enabled: false,
        low_threshold_pct: 30,
        interval_secs: 60,
    }
}

/// 阈值与间隔的合法域（前端 sanitize 同款;这里兜底钳制,别信传输值）。
fn clamp_cfg(mut c: BoostConfig) -> BoostConfig {
    c.spike_threshold_pct = c.spike_threshold_pct.clamp(5, 50);
    c.low_threshold_pct = c.low_threshold_pct.clamp(10, 50);
    c.interval_secs = c.interval_secs.clamp(30, 240);
    c
}

/// 运行时配置（单写多读;写 = 命令线程,读 = boost 线程每秒 clone 一份判定用）。
static CONFIG: Mutex<Option<BoostConfig>> = Mutex::new(None);

/// 取配置槽 / 结果槽（poison 容忍,审计 P3-）：持锁线程 panic 后槽内容仍可用,
/// 不让 boost 线程与命令面连锁停摆。
fn config_slot() -> std::sync::MutexGuard<'static, Option<BoostConfig>> {
    CONFIG.lock().unwrap_or_else(|e| e.into_inner())
}

fn current_cfg() -> BoostConfig {
    config_slot().clone().unwrap_or_else(default_config)
}

/// boost 结果内存槽（每平台至多一条成功快照;**不落库**——重启即失,
/// 回落主快照节奏,符合「补充路由」定位）。
static BOOST_SNAPS: LazyLock<Mutex<HashMap<Platform, SubscriptionSnapshot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn snap_slot() -> std::sync::MutexGuard<'static, HashMap<Platform, SubscriptionSnapshot>> {
    BOOST_SNAPS.lock().unwrap_or_else(|e| e.into_inner())
}

/// 滚动样本窗口长度（5 个成功样本 × interval ≈ 的「累计五分钟/五次」;
/// 间隔改 2/3 分钟时窗口随之拉长——判据始终是「最近 5 个成功样本」）。
const SAMPLE_WINDOW: usize = 5;

/// 单平台 boost 状态机（纯逻辑,不含 IO——单测直接覆盖）。
#[derive(Debug, Default, Clone)]
struct PlatformBoost {
    active: bool,
    /// 上次见到的主快照 fetched_at（检测主轮询出新数据;None = 尚无）。
    last_fetched: Option<i64>,
    /// 上次见到的主快照 used_5h（条件 A 的 diff 基线;boost 激活中也要跟随
    /// 更新,否则退出后首轮 diff 跨越 boost 期间全部消耗 → 假 spike 立即复发）。
    last_used: Option<f64>,
    /// boost 成功样本（used_5h,保尾 SAMPLE_WINDOW 个;失败轮不入列）。
    samples: Vec<f64>,
}

impl PlatformBoost {
    /// 见到主快照（boost 线程每秒读库）。仅在 fetched_at 变化时评估;
    /// 返回 true = 满足进入条件（调用方在**未激活**分支消费）。
    /// 激活中也照常调用——只更新基线,忽略返回值。
    fn observe_main(&mut self, cfg: &BoostConfig, snap: &SubscriptionSnapshot) -> bool {
        if snap.fetched_at == self.last_fetched {
            return false; // 无新数据（含重复读库与失败轮——失败轮 fetched_at 不推进）
        }
        self.last_fetched = snap.fetched_at;
        let Some(w) = snap.windows.iter().find(|w| w.kind == "5h") else {
            // 无 5h 数据（未成功过/idle/解绑）→ 基线作废,条件无从谈起
            self.last_used = None;
            return false;
        };
        let used = w.used_percent;
        let mut enter = false;
        // 条件 A:相邻两轮主轮询的消耗差（首轮无基线,只记不评）
        if let Some(prev) = self.last_used {
            if cfg.spike_enabled && used - prev >= f64::from(cfg.spike_threshold_pct) {
                enter = true;
            }
        }
        // 条件 B:低余量（不依赖 diff,首轮即可评）
        if cfg.low_enabled && 100.0 - used <= f64::from(cfg.low_threshold_pct) {
            enter = true;
        }
        self.last_used = Some(used);
        enter
    }

    /// boost 成功样本入列。返回 true = 应退出（所有已启用条件均不再成立）。
    fn on_sample(&mut self, cfg: &BoostConfig, used: f64) -> bool {
        self.samples.push(used);
        if self.samples.len() > SAMPLE_WINDOW {
            self.samples.remove(0);
        }
        if self.samples.len() < SAMPLE_WINDOW {
            return false; // 防抖:窗口未满不评退出
        }
        let mut keep = false;
        // A 保持判据:最近 5 样本累计消耗（最新 − 最旧;窗口重置时为负 → 不保持）
        if cfg.spike_enabled {
            let gain = self.samples[SAMPLE_WINDOW - 1] - self.samples[0];
            if gain >= f64::from(cfg.spike_threshold_pct) {
                keep = true;
            }
        }
        // B 保持判据:当前剩余仍低（低余量闲消耗也保持监控,重置后才退）
        if cfg.low_enabled && 100.0 - used <= f64::from(cfg.low_threshold_pct) {
            keep = true;
        }
        !keep
    }
}

fn emit_boost(app: &AppHandle) {
    let _ = app.emit("subscription:boost", true);
}

fn put_snap(app: &AppHandle, snap: SubscriptionSnapshot) {
    snap_slot().insert(snap.platform, snap);
    emit_boost(app);
}

fn clear_snap(app: &AppHandle, platform: Platform) {
    if snap_slot().remove(&platform).is_some() {
        emit_boost(app); // 退出回落也广播——前端摘掉 boosting 提示
        // 待机退档可能与 boost 叠加:主轮询可能已放慢到 30 分钟档,而回落读数
        // 走主快照——唤醒主轮询立刻补一轮新鲜数据（否则回落最旧可差 30 分钟）。
        super::wake();
    }
}

fn run(app: AppHandle, reader: SubscriptionReader, adapters: std::sync::Arc<Adapters>) {
    crate::dev_log!("[boost] thread started");
    let mut rt: HashMap<Platform, PlatformBoost> = HashMap::new();
    let mut next_fetch: HashMap<Platform, i64> = HashMap::new();
    // 总开关沿（关闭瞬间一次性清场,别每秒重复清/发事件）
    let mut was_enabled = false;

    loop {
        std::thread::sleep(Duration::from_secs(1));
        let cfg = current_cfg();
        if !cfg.enabled {
            if was_enabled {
                let cleared = {
                    let mut g = snap_slot();
                    std::mem::take(&mut *g)
                };
                rt.clear();
                next_fetch.clear();
                if !cleared.is_empty() {
                    emit_boost(&app);
                    super::wake(); // 同 clear_snap:回落读数要新鲜
                }
                was_enabled = false;
            }
            continue;
        }
        was_enabled = true;
        let now = chrono::Utc::now().timestamp();
        // 绑定表（每轮一次）:未绑定平台不得取数（审计 P1-）——unbind 会留下
        // Idle 占位行（store 对 status=Idle 走覆盖写）,仅凭 load_snapshot 为
        // Some 判「已绑定」会让激活中的平台在解绑后继续 fetch_one（low 条件
        // 持续成立时甚至永不退出）。
        let bound = {
            let store = reader.0.lock().unwrap_or_else(|e| e.into_inner());
            store.bound_platforms()
        };

        for platform in [Platform::Codex, Platform::Claude] {
            // 解绑即清场:摘激活态与内存槽（clear_snap 兼发事件让前端摘掉
            // boosting）;轨道一并移除,重绑后从零重建基线。
            if !bound.contains(&platform) {
                if let Some(entry) = rt.remove(&platform) {
                    next_fetch.remove(&platform);
                    if entry.active {
                        clear_snap(&app, platform);
                        crate::dev_log!("[boost] {} drop (unbound)", platform.as_str());
                    }
                }
                continue;
            }
            let main_snap = {
                let store = reader.0.lock().unwrap_or_else(|e| e.into_inner());
                store.load_snapshot(platform)
            };
            let Some(main_snap) = main_snap else { continue }; // 库里没有该平台行
            let entry = rt.entry(platform).or_default();
            if entry.active {
                // 激活中:主快照只用于跟随 diff 基线（忽略进入评估）
                entry.observe_main(&cfg, &main_snap);
                if now >= *next_fetch.get(&platform).unwrap_or(&0) {
                    // 复用主模块取数:429 冷却/判死/idle 全套保护现成。
                    // 失败轮 fetched_at 不推进 → 不入样本,窗口顺延,槽内旧数据保留。
                    // 取数互斥（审计 P3-）:主轮询正在取 → 让行（不阻塞、
                    // 不计样本）,下一 interval 再试;主快照那轮照常更新 diff 基线。
                    let Some(lease) = adapters.try_lock_fetch(platform) else {
                        next_fetch.insert(platform, now + cfg.interval_secs as i64);
                        crate::dev_log!("[boost] {} yield (main fetch in flight)", platform.as_str());
                        continue;
                    };
                    let snap = fetch_one(&adapters, platform);
                    drop(lease); // 互斥只覆盖取数本身;后续落槽/清场不占锁

                    if snap.fetched_at.is_some() {
                        let exit = snap
                            .windows
                            .iter()
                            .find(|w| w.kind == "5h")
                            .map(|w| entry.on_sample(&cfg, w.used_percent))
                            .unwrap_or(false);
                        if exit {
                            // 退出不落槽（审计 P2-）:先 put 后 clear 会让前端先
                            // 收到「有新数据」再收到「清空」两次事件,读数先跳后
                            // 回退,还会误触发一次 landing 动画。
                            entry.active = false;
                            entry.samples.clear();
                            clear_snap(&app, platform);
                            crate::dev_log!("[boost] {} exit (all triggers cleared)", platform.as_str());
                        } else {
                            put_snap(&app, snap);
                        }
                    }
                    next_fetch.insert(platform, now + cfg.interval_secs as i64);
                }
            } else {
                let enter = entry.observe_main(&cfg, &main_snap);
                if enter {
                    entry.active = true;
                    entry.samples.clear();
                    next_fetch.insert(platform, now); // 进入即首取
                    crate::dev_log!("[boost] {} enter (spike={} low={})",
                        platform.as_str(), cfg.spike_enabled, cfg.low_enabled);
                }
            }
        }
    }
}

/// setup 接线（`subscription:spawn` 末尾调用）:共享主模块的只读连接与
/// 适配器句柄（token 缓存/RateGate 同源——boost 与主轮询不各持一套限流闸）。
pub fn spawn(
    app: AppHandle,
    reader: SubscriptionReader,
    adapters: std::sync::Arc<Adapters>,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("subscription-boost".into())
        .spawn(move || run(app, reader, adapters))
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------- 命令面（snake_case,契约风格） ----------

/// 当前 boost 快照（前端 orb 窗口初查口;仅激活中平台有值）。
#[tauri::command]
pub fn get_subscription_boost() -> Result<Vec<SubscriptionSnapshot>, String> {
    let g = snap_slot();
    let mut out: Vec<SubscriptionSnapshot> = g.values().cloned().collect();
    out.sort_by_key(|s| s.platform.as_str());
    Ok(out)
}

/// 下发 boost 配置（clamp 后生效;持久化由前端 designPrefs 承担,
/// 重启后 orb 窗口装载时再调本命令恢复——与 set_subscription_poll_secs 同款）。
#[tauri::command]
pub fn set_subscription_boost(config: BoostConfig) -> Result<(), String> {
    // 幂等早退（审计 P2-）：orb 窗口对任意 designPrefs 广播都重下发配置,
    // 值未变时跳过写（boost 线程每秒自会读到同一份配置）。
    let next = clamp_cfg(config);
    let mut g = config_slot();
    if g.as_ref() == Some(&next) {
        return Ok(());
    }
    *g = Some(next);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription::model::{FetchStatus, QuotaWindow};

    fn cfg() -> BoostConfig {
        default_config() // spike 开 10 / low 关 30 / 60s
    }

    fn snap(fetched: i64, used_5h: f64) -> SubscriptionSnapshot {
        SubscriptionSnapshot {
            platform: Platform::Codex,
            plan_type: "pro".into(),
            windows: vec![QuotaWindow {
                kind: "5h".into(),
                used_percent: used_5h,
                resets_at: None,
            }],
            fetched_at: Some(fetched),
            status: FetchStatus::Ok,
        }
    }

    #[test]
    fn spike_enter_after_two_main_rounds() {
        let c = cfg();
        let mut pb = PlatformBoost::default();
        assert!(!pb.observe_main(&c, &snap(100, 5.0)), "首轮无基线,只记");
        assert!(pb.observe_main(&c, &snap(400, 18.0)), "+13% ≥ 10% → 进入");
    }

    #[test]
    fn low_enter_on_first_snapshot() {
        let mut c = cfg();
        c.spike_enabled = false;
        c.low_enabled = true; // 阈值 30
        let mut pb = PlatformBoost::default();
        assert!(pb.observe_main(&c, &snap(100, 85.0)), "剩余 15% ≤ 30% → 进入");
    }

    #[test]
    fn repeated_snapshot_never_reenters() {
        let c = cfg();
        let mut pb = PlatformBoost::default();
        assert!(!pb.observe_main(&c, &snap(100, 5.0)));
        assert!(!pb.observe_main(&c, &snap(100, 5.0)), "fetched_at 未变 → 不重评");
    }

    #[test]
    fn failed_round_clears_baseline_only() {
        let c = cfg();
        let mut pb = PlatformBoost::default();
        assert!(!pb.observe_main(&c, &snap(100, 5.0)));
        // 失败轮:无窗口无 fetched_at → 基线作废,不进入
        let dead = SubscriptionSnapshot {
            platform: Platform::Codex,
            plan_type: "unknown".into(),
            windows: vec![],
            fetched_at: None,
            status: FetchStatus::NetworkFailed,
        };
        assert!(!pb.observe_main(&c, &dead));
        assert!(!pb.observe_main(&c, &snap(400, 18.0)), "基线已清 → 不算 spike");
    }

    #[test]
    fn active_entry_keeps_baseline_following_main() {
        // 激活中主快照照常 observe(基线跟随),退出后首轮 diff 不会跨越 boost 期
        let c = cfg();
        let mut pb = PlatformBoost::default();
        assert!(!pb.observe_main(&c, &snap(100, 5.0)));
        assert!(pb.observe_main(&c, &snap(400, 20.0))); // 进入
        pb.observe_main(&c, &snap(700, 30.0)); // 激活中主轮询推进基线
        // 退出后下一轮 +2%(32):若基线没跟随(还是 5)会假 spike,+2 不触发
        assert!(!pb.observe_main(&c, &snap(1000, 32.0)));
    }

    #[test]
    fn window_underfull_never_exits() {
        let c = cfg();
        let mut pb = PlatformBoost::default();
        pb.active = true;
        for i in 0..SAMPLE_WINDOW - 1 {
            assert!(!pb.on_sample(&c, 50.0 + i as f64), "窗口未满不评退出");
        }
    }

    #[test]
    fn low_gain_window_exits() {
        let c = cfg();
        let mut pb = PlatformBoost::default();
        pb.active = true;
        for i in 0..SAMPLE_WINDOW - 1 {
            assert!(!pb.on_sample(&c, 50.0 + i as f64), "窗口未满不评退出");
        }
        // 第 5 个样本:gain = 54 − 50 = 4 < 10 → 退出
        assert!(pb.on_sample(&c, 54.0));
    }

    #[test]
    fn sustained_surge_keeps_boost() {
        let c = cfg();
        let mut pb = PlatformBoost::default();
        pb.active = true;
        for i in 0..SAMPLE_WINDOW + 2 {
            assert!(!pb.on_sample(&c, 50.0 + 3.0 * i as f64), "+3%/轮滚动 gain=12 ≥ 10 → 保持");
        }
    }

    #[test]
    fn reset_drop_exits_via_negative_gain() {
        let c = cfg();
        let mut pb = PlatformBoost::default();
        pb.active = true;
        for i in 0..SAMPLE_WINDOW {
            pb.on_sample(&c, 60.0 + i as f64);
        }
        // 5h 窗口重置:used 骤降 → gain 负 → 退出
        assert!(pb.on_sample(&c, 3.0));
    }

    #[test]
    fn low_trigger_keeps_even_when_gain_low() {
        let mut c = cfg();
        c.spike_enabled = false;
        c.low_enabled = true;
        c.low_threshold_pct = 30;
        let mut pb = PlatformBoost::default();
        pb.active = true;
        // 剩余 20%（used 80）低消耗:gain 不够,但 low 仍成立 → 保持
        for i in 0..SAMPLE_WINDOW + 1 {
            assert!(!pb.on_sample(&c, 80.0 + 0.1 * i as f64));
        }
        // 重置后剩余高:low 不再成立 → 退出
        assert!(pb.on_sample(&c, 2.0));
    }

    #[test]
    fn disabled_triggers_exit_immediately_after_window() {
        let mut c = cfg();
        c.spike_enabled = false; // low 也关:两条件全关 → 满窗即退
        c.low_enabled = false;
        let mut pb = PlatformBoost::default();
        pb.active = true;
        for _ in 0..SAMPLE_WINDOW - 1 {
            assert!(!pb.on_sample(&c, 50.0));
        }
        assert!(pb.on_sample(&c, 50.0));
    }

    #[test]
    fn clamp_cfg_bounds() {
        let c = clamp_cfg(BoostConfig {
            enabled: true,
            spike_threshold_pct: 1,
            low_threshold_pct: 90,
            interval_secs: 5,
            ..default_config()
        });
        assert_eq!(c.spike_threshold_pct, 5);
        assert_eq!(c.low_threshold_pct, 50);
        assert_eq!(c.interval_secs, 30);
    }
}
