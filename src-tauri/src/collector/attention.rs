//! 注意力状态机：从各源采集批次的「会话现状观测」派生每个会话的
//! running / waiting / tool_pending,供 Timeline 看板项目行亮起。**内存表,不落库,不写游标**。
//!
//! 数据流：适配器在文件末尾 `TurnState:flush`（ZCode / CodeBuddy 在各自重建处）把观测写进
//! `Batch:live` → `Store:commit` 暂存 → 采集线程每轮结束 `take_live` 合入本表 → `tick` 按当前
//! 时刻派生、过期剔除,派生结果与上一轮不同才 emit `timeline:attention`（载荷恒 true,前端重查）。
//! 启动时表为空,由 `Store:recent_turn_states` 按游标更新时间在离开阈值内的会话播种一次
//! （JSONL 族 + DSH;ZCode / CodeBuddy 无会话级游标,等下一次源写入）。
//!
//! 各源「模型已答完、在等用户」信号：
//! Claude Code = assistant 行 `message.stop_reason` 非 `tool_use`;Codex = `task_complete` 闭轮;
//! DSH = `turn/end` 闭轮;ZCode = `turn_usage.completed_at` 已写;CodeBuddy = 末条 request
//! `state = complete`;WorkBuddy 无显式信号 → 启发式（末事件为 assistant message 且静默
//! ≥ `HEURISTIC_SETTLE_MS`,中途 assistant 文本之后仍可能继续调工具）。
//!
//! 自动退出（前台自动确认）:`attention_watch` 守护线程**只在有未确认提示时**
//! 醒着（`watch_needs`）,喂入前台窗口;进前台时刻取系统前台切换事件,不靠轮询。
//! 宗旨「等待要提醒,正在对话不保持」,且**宁可多亮、不漏提醒**：
//! - 只作用于 `waiting`;`tool_pending`（等批权限,agent 被阻塞）只靠源写入退出,从不自动确认。
//! - **到窗口面前即全部熄灭**（只熄当前会话会让单窗口宿主里的并行会话长亮）：
//!   宿主窗口在前台且人在（键鼠闲置 < `INPUT_IDLE_MS`）→ 该窗口下**所有**等待一视同仁（单窗口宿主 =
//!   同宿主全部会话,按标题消歧的宿主 = 该项目的会话）：进前台**之前**就亮的,停留 ≥ `FOREGROUND_DWELL_MS`
//!   后确认;在前台期间新亮的先**暂压**（`held`）,答完后人还在这个窗口里动过键鼠且已过 `HELD_ACK_MS`
//!   → 确认;答完就离开窗口 / 人走开 → 重新亮起,回来再按前一条确认。
//! - 宿主不确定（CLI / 未知 entrypoint）不参与（见 `agent_focus:watch_target`）。
//! - 快速退出：亮着的会话由探针盯源文件 mtime,变了即提前唤醒采集（`watch_list`）。

use std::collections::BTreeMap;

use serde::Serialize;

/// 未配对工具静默超过此值 → tool_pending（Claude Code 的 tool_result 要等权限批准后才写入,
/// ≈「在等你批权限」;与长时间工具有歧义,取值按误报率权衡）。
pub const TOOL_PENDING_MS: i64 = 90_000;
/// 启发式「答完」信号的静默确认时长。
pub const HEURISTIC_SETTLE_MS: i64 = 60_000;
/// 宿主窗口进前台后停留这么久才自动确认（Alt+Tab 路过不算看过）。
pub const FOREGROUND_DWELL_MS: i64 = 2_000;
/// 在前台期间答完的等待：答完后人还在该窗口里操作、且过了这么久 → 确认（没来得及看就离开的仍会亮起）。
pub const HELD_ACK_MS: i64 = 10_000;
/// 键鼠闲置超过此值 → 视为人不在（前台窗口不再暂压 / 不再确认）。
pub const INPUT_IDLE_MS: u64 = 120_000;

/// 会话现状（观测时刻的原始阶段,不含时间判定）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LivePhase {
    /// 无进行中的轮（用户中止 / 丢弃的待定输入 / 空会话）→ 不进表。
    Idle,
    /// 模型在处理（输入已发未回 / 工具结果已回、模型未续）。
    Busy,
    /// 有未配对的工具调用。
    Tools,
    /// 模型答完在等用户;`exact` = 源显式信号。
    Done { exact: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiveTurn {
    /// 原始目录键（前端经 `effective_key` 解析,与 pin 同口径）。
    pub project_key: String,
    /// 子会话的父会话（子会话不单独亮起,只用于压制父会话的 tool_pending）。
    pub parent_id: Option<String>,
    /// 【内容列】会话标题,只供 timeline 窗口本地渲染。
    pub title: Option<String>,
    /// 桌面宿主线索（Claude Code 的 `entrypoint`;其余源 None）。
    pub host: Option<String>,
    pub phase: LivePhase,
    /// 会话最近事件时刻（毫秒）。
    pub last_event: i64,
    /// 最近一次真实用户输入时刻（旧游标 / 无此概念的源 None）。
    pub last_input: Option<i64>,
    /// 快速探针：会话源文件路径 + 采集时的 mtime（None = 无单文件可探,如 ZCode 库）。
    pub watch: Option<(String, i64)>,
}

/// 前台窗口采样（`attention_watch` 喂入;人不在时喂 None）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForegroundWindow {
    /// 根窗口句柄（判定「同一个窗口」;标题变化不算换窗口）。
    pub window: isize,
    /// 进程映像名（小写,不含路径）。
    pub exe: String,
    pub title: String,
    /// 这个窗口进入前台的时刻（前台切换事件记录;0 = 未知,按「同窗口沿用、换窗口记为现在」推断）。
    pub since: i64,
    /// 最近一次键鼠输入的时刻（0 = 未知;判「答完之后人还在这个窗口里操作」）。
    pub input_at: i64,
}

struct Foreground {
    win: ForegroundWindow,
    /// 这个窗口本次进入前台的时刻。
    since: i64,
}

/// （agent_key, session_id)
pub type LiveKey = (String, String);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AttentionItem {
    pub agent: String,
    /// agent 展示名（与看板格子同口径）。
    pub agent_label: String,
    pub session_id: String,
    pub project_key: String,
    pub title: Option<String>,
    /// 桌面宿主线索（`focus_agent_window` 选目标进程用;前端只透传）。
    pub host: Option<String>,
    /// running | waiting | tool_pending
    pub state: &'static str,
    /// running = 最近事件;waiting = 模型答完时刻;tool_pending = 工具静默开始时刻。
    pub since: i64,
    pub last_event: i64,
    /// 已确认（仅 waiting / tool_pending;同一会话进入新一段等待自动复位）。
    pub acked: bool,
    /// 暂压（仅 waiting）：宿主窗口在前台期间新答完的会话,先不亮;留在窗口里继续操作 → 确认,离开窗口 → 亮起。
    pub held: bool,
}

struct Entry {
    obs: LiveTurn,
    /// 确认时那一段等待的 `since`;派生出的 since 变了即视为新一段等待。
    acked_since: Option<i64>,
}

type Signature = Vec<(String, String, &'static str, i64, bool, bool)>;

#[derive(Default)]
pub struct AttentionTable {
    entries: BTreeMap<LiveKey, Entry>,
    /// 上一次 emit 时的派生签名（running 的 since 随每个事件前移,不参与比较,防活跃期每轮都 emit）。
    last_sig: Signature,
    fg: Option<Foreground>,
    /// 最近一次喂入 None（人不在 / 无前台窗口）的时刻：人回来后从此刻重新计停留,不沿用切换事件的旧时刻。
    absent_at: i64,
    /// `remove` 移除的伪等待 （agent, session, since),只为落盘;随离开阈值过期。
    removed: Vec<(String, String, i64)>,
}

impl AttentionTable {
    /// 合入一批观测（同键覆盖,保留确认标记）。
    pub fn apply(&mut self, live: BTreeMap<LiveKey, LiveTurn>) {
        for (key, obs) in live {
            let acked_since = self.entries.get(&key).and_then(|e| e.acked_since);
            self.entries.insert(key, Entry { obs, acked_since });
        }
    }

    /// 播种（启动时）:只在表内没有该会话、或新观测更近时写入。
    pub fn seed(&mut self, key: LiveKey, obs: LiveTurn) {
        if self.entries.get(&key).map_or(true, |e| e.obs.last_event < obs.last_event) {
            self.entries.insert(key, Entry { obs, acked_since: None });
        }
    }

    /// 已确认标记快照 （agent, session, acked_since),落盘用（重启后不重复提醒）。
    /// 含被 `remove` 移除的伪等待（否则重启播种会把它重新亮起）。
    pub fn acks(&self) -> Vec<(String, String, i64)> {
        let live = self.entries.iter().filter_map(|((a, s), e)| e.acked_since.map(|t| (a.clone(), s.clone(), t)));
        live.chain(self.removed.iter().cloned()).collect()
    }

    /// 启动播种后恢复上次运行的确认标记：只写表内已有条目;派生 since 对不上（期间进入了新一段等待）
    /// 的标记自然失效,照常亮起。
    pub fn restore_acks(&mut self, acks: Vec<(String, String, i64)>) {
        for (agent, session, since) in acks {
            if let Some(e) = self.entries.get_mut(&(agent, session)) {
                e.acked_since.get_or_insert(since);
            }
        }
    }

    /// 某父会话下是否有近期活跃的子会话（Task 子代理在跑 → 父会话的长工具不是在等权限）。
    fn child_active(&self, agent: &str, session: &str, now: i64) -> bool {
        self.entries.iter().any(|((a, _), e)| {
            a == agent
                && e.obs.parent_id.as_deref() == Some(session)
                && e.obs.phase != LivePhase::Idle
                && now - e.obs.last_event < TOOL_PENDING_MS
        })
    }

    /// 派生 （state, since, lit_at);`lit_at` = 按源时间戳这段提示应亮起的时刻（判「亮在进前台之前」用）。
    fn derive(&self, key: &LiveKey, e: &Entry, now: i64) -> Option<(&'static str, i64, i64)> {
        let t = e.obs.last_event;
        match e.obs.phase {
            LivePhase::Idle => None,
            LivePhase::Busy => Some(("running", t, t)),
            LivePhase::Tools => {
                if now - t >= TOOL_PENDING_MS && !self.child_active(&key.0, &key.1, now) {
                    Some(("tool_pending", t, t + TOOL_PENDING_MS))
                } else {
                    Some(("running", t, t))
                }
            }
            LivePhase::Done { exact: true } => Some(("waiting", t, t)),
            LivePhase::Done { exact: false } => {
                if now - t >= HEURISTIC_SETTLE_MS {
                    Some(("waiting", t, t + HEURISTIC_SETTLE_MS))
                } else {
                    Some(("running", t, t))
                }
            }
        }
    }

    /// 会话是否属于当前前台窗口（宿主进程对得上;按标题消歧的宿主还要标题含项目目录尾段）。
    fn in_foreground(&self, key: &LiveKey, e: &Entry) -> bool {
        let Some(fg) = self.fg.as_ref() else { return false };
        e.obs.parent_id.is_none()
            && e.obs.phase != LivePhase::Idle
            && crate::agent_focus::watch_target(&key.0, e.obs.host.as_deref())
                .is_some_and(|t| crate::agent_focus::window_matches(t, &fg.win.exe, &fg.win.title, &e.obs.project_key))
    }

    /// 前台相关判据的共同前提：waiting + 属于前台窗口——到了 agent 窗口面前,该窗口下的等待一视同仁
    /// （单窗口宿主 = 同宿主全部会话;窗口内看的是哪个会话无从判定,应用自己的侧栏接手提示）。
    /// 按标题消歧的宿主（一窗一项目）若前台组内出现多个项目键 = 同名目录撞标题,分不清是哪个项目,整组不动。
    fn foreground_member(&self, key: &LiveKey, e: &Entry, state: &str, now: i64, idle_ms: i64) -> bool {
        if state != "waiting" || !self.in_foreground(key, e) {
            return false;
        }
        let by_title = crate::agent_focus::watch_target(&key.0, e.obs.host.as_deref()).is_some_and(|t| t.by_title);
        !by_title
            || !self.entries.iter().any(|(k, o)| {
                now - o.obs.last_event <= idle_ms && o.obs.project_key != e.obs.project_key && self.in_foreground(k, o)
            })
    }

    /// 剔除：无状态、超过离开阈值无事件。
    pub fn prune(&mut self, now: i64, idle_ms: i64) {
        self.entries.retain(|_, e| e.obs.phase != LivePhase::Idle && now - e.obs.last_event <= idle_ms);
        self.removed.retain(|(_, _, since)| now - since <= idle_ms);
    }

    /// 当前派生快照（子会话不列出;按 since 先后,不做优先级排序）。
    pub fn items(&self, now: i64, idle_ms: i64) -> Vec<AttentionItem> {
        let fg_since = self.fg.as_ref().map(|f| f.since);
        let mut out: Vec<AttentionItem> = self
            .entries
            .iter()
            .filter(|(_, e)| e.obs.parent_id.is_none() && now - e.obs.last_event <= idle_ms)
            .filter_map(|(key, e)| {
                let (state, since, lit_at) = self.derive(key, e, now)?;
                let acked = state != "running" && e.acked_since == Some(since);
                let held = !acked
                    && fg_since.is_some_and(|s| lit_at >= s)
                    && self.foreground_member(key, e, state, now, idle_ms);
                Some(AttentionItem {
                    agent: key.0.clone(),
                    agent_label: super::store::agent_label(&key.0),
                    session_id: key.1.clone(),
                    project_key: e.obs.project_key.clone(),
                    title: e.obs.title.clone(),
                    host: e.obs.host.clone(),
                    state,
                    since,
                    last_event: e.obs.last_event,
                    acked,
                    held,
                })
            })
            .collect();
        out.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| (&a.agent, &a.session_id).cmp(&(&b.agent, &b.session_id))));
        out
    }

    fn signature(items: &[AttentionItem]) -> Signature {
        items
            .iter()
            .map(|i| (i.agent.clone(), i.session_id.clone(), i.state, if i.state == "running" { 0 } else { i.since }, i.acked, i.held))
            .collect()
    }

    /// 重算签名并与上次比较。返回 true = 需要 emit。
    fn resync(&mut self, now: i64, idle_ms: i64) -> bool {
        let sig = Self::signature(&self.items(now, idle_ms));
        let changed = sig != self.last_sig;
        self.last_sig = sig;
        changed
    }

    /// 每轮采集后调用：剔除过期 → 派生 → 与上次签名比较。返回 true = 需要 emit。
    pub fn tick(&mut self, now: i64, idle_ms: i64) -> bool {
        self.prune(now, idle_ms);
        self.resync(now, idle_ms)
    }

    /// 喂入前台窗口（None = 没有 / 人不在）：维护进前台时刻 → 确认该窗口下停留够久的「进前台前就亮」的等待,
    /// 以及「在前台期间答完、之后人仍在窗口里操作」的等待。
    /// 返回 true = 派生有变（需要 emit）。
    pub fn set_foreground(&mut self, win: Option<ForegroundWindow>, now: i64, idle_ms: i64) -> bool {
        let prev = self.fg.take();
        let absent_at = self.absent_at;
        self.fg = win.map(|win| {
            let same = prev.as_ref().filter(|p| p.win.window == win.window).map(|p| p.since);
            let base = if win.since > 0 { win.since } else { same.unwrap_or(now) };
            let since = match (&prev, same) {
                (_, Some(kept)) => base.max(kept),
                // 人走开后回来、窗口没换过：切换事件的时刻早于离开,从现在重新计停留
                (None, _) if base < absent_at => now,
                _ => base,
            };
            Foreground { since, win }
        });
        if self.fg.is_none() {
            self.absent_at = now;
        }
        if let Some((fg_since, input_at)) = self.fg.as_ref().map(|f| (f.since, f.win.input_at)) {
            let due: Vec<(LiveKey, i64)> = self
                .entries
                .iter()
                .filter(|(_, e)| now - e.obs.last_event <= idle_ms)
                .filter_map(|(k, e)| {
                    let (state, since, lit_at) = self.derive(k, e, now)?;
                    let seen = if lit_at < fg_since {
                        now - fg_since >= FOREGROUND_DWELL_MS
                    } else {
                        now - lit_at >= HELD_ACK_MS && input_at >= lit_at
                    };
                    (e.acked_since != Some(since) && seen && self.foreground_member(k, e, state, now, idle_ms)).then(|| (k.clone(), since))
                })
                .collect();
            for (k, since) in due {
                if let Some(e) = self.entries.get_mut(&k) {
                    e.acked_since = Some(since);
                }
            }
        }
        self.resync(now, idle_ms)
    }

    /// 守护线程要做什么：（前台采样, 文件探针)。只看**未确认**的提示——前台暂压 / 确认只作用于未确认的
    /// waiting（且宿主可判定）;探针只为让未确认的提示尽快熄灭。都 false → 守护线程挂起,零开销。
    pub fn watch_needs(&self, now: i64, idle_ms: i64) -> (bool, bool) {
        let mut needs = (false, false);
        for (k, e) in self.entries.iter().filter(|(_, e)| e.obs.parent_id.is_none() && now - e.obs.last_event <= idle_ms) {
            let Some((state, since, _)) = self.derive(k, e, now) else { continue };
            if state == "running" || e.acked_since == Some(since) {
                continue;
            }
            if state == "waiting" && crate::agent_focus::watch_target(&k.0, e.obs.host.as_deref()).is_some() {
                needs.0 = true;
            }
            if e.obs.watch.is_some() {
                needs.1 = true;
            }
        }
        needs
    }

    /// 快速探针清单：未确认的 waiting / tool_pending（含暂压）且有源文件的会话 → （路径, 采集时 mtime)。
    /// 文件再被写入 ≈ 用户回复了 / 批了权限,探针据此提前唤醒采集,不必等下一个采集周期。
    pub fn watch_list(&self, now: i64, idle_ms: i64) -> Vec<(String, i64)> {
        self.entries
            .iter()
            .filter(|(_, e)| e.obs.parent_id.is_none() && now - e.obs.last_event <= idle_ms)
            .filter(|(k, e)| {
                self.derive(k, e, now).is_some_and(|(state, since, _)| state != "running" && e.acked_since != Some(since))
            })
            .filter_map(|(_, e)| e.obs.watch.clone())
            .collect()
    }

    /// 聚焦前查会话的宿主线索与原始项目键（找目标进程 / 同进程多窗口按项目目录名消歧）。
    pub fn lookup(&self, agent: &str, session: &str) -> Option<(Option<String>, String)> {
        self.entries.get(&(agent.to_string(), session.to_string())).map(|e| (e.obs.host.clone(), e.obs.project_key.clone()))
    }

    /// 移除一个会话条目（聚焦时找不到宿主窗口 = agent 已关、文件停在答完之后的伪等待）。
    /// 返回 true = 表有变（需要 emit）。源再写入时 `apply` 会重新建立条目。
    pub fn remove(&mut self, agent: &str, session: &str, now: i64, idle_ms: i64) -> bool {
        let key = (agent.to_string(), session.to_string());
        let since = self.entries.get(&key).and_then(|e| self.derive(&key, e, now)).map(|(_, since, _)| since);
        if self.entries.remove(&key).is_none() {
            return false;
        }
        if let Some(since) = since {
            self.removed.retain(|(a, s, _)| (a.as_str(), s.as_str()) != (agent, session));
            self.removed.push((key.0, key.1, since));
        }
        self.resync(now, idle_ms)
    }

    /// 确认一个会话当前这一段等待。返回 true = 状态有变（需要 emit）。
    pub fn ack(&mut self, agent: &str, session: &str, now: i64, idle_ms: i64) -> bool {
        let key = (agent.to_string(), session.to_string());
        let Some(since) = self
            .entries
            .get(&key)
            .and_then(|e| self.derive(&key, e, now))
            .filter(|(state, _, _)| *state != "running")
            .map(|(_, since, _)| since)
        else {
            return false;
        };
        let Some(e) = self.entries.get_mut(&key) else { return false };
        if e.acked_since == Some(since) {
            return false;
        }
        e.acked_since = Some(since);
        self.last_sig = Self::signature(&self.items(now, idle_ms));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_789_000_000_000;
    const IDLE: i64 = 30 * 60_000;

    fn obs(phase: LivePhase, last: i64) -> LiveTurn {
        LiveTurn {
            project_key: "e:/p".into(),
            parent_id: None,
            title: Some("t".into()),
            host: None,
            phase,
            last_event: last,
            last_input: None,
            watch: None,
        }
    }

    fn key(s: &str) -> LiveKey {
        ("claude-code".into(), s.into())
    }

    fn one(t: &AttentionTable, now: i64) -> Option<(&'static str, i64, bool)> {
        t.items(now, IDLE).first().map(|i| (i.state, i.since, i.acked))
    }

    #[test]
    fn phases_derive_states_over_time() {
        let mut t = AttentionTable::default();
        t.apply(BTreeMap::from([(key("s"), obs(LivePhase::Busy, NOW))]));
        assert_eq!(one(&t, NOW + 1_000), Some(("running", NOW, false)));
        t.apply(BTreeMap::from([(key("s"), obs(LivePhase::Done { exact: true }, NOW + 5_000))]));
        assert_eq!(one(&t, NOW + 6_000), Some(("waiting", NOW + 5_000, false)), "显式信号立即亮起");
        t.apply(BTreeMap::from([(key("s"), obs(LivePhase::Tools, NOW + 10_000))]));
        assert_eq!(one(&t, NOW + 10_000 + TOOL_PENDING_MS - 1).map(|x| x.0), Some("running"));
        assert_eq!(one(&t, NOW + 10_000 + TOOL_PENDING_MS).map(|x| x.0), Some("tool_pending"));
    }

    #[test]
    fn heuristic_done_needs_settle() {
        let mut t = AttentionTable::default();
        t.apply(BTreeMap::from([(key("s"), obs(LivePhase::Done { exact: false }, NOW))]));
        assert_eq!(one(&t, NOW + HEURISTIC_SETTLE_MS - 1).map(|x| x.0), Some("running"));
        assert_eq!(one(&t, NOW + HEURISTIC_SETTLE_MS).map(|x| x.0), Some("waiting"));
    }

    #[test]
    fn ack_holds_until_next_waiting_period() {
        let mut t = AttentionTable::default();
        t.apply(BTreeMap::from([(key("s"), obs(LivePhase::Done { exact: true }, NOW))]));
        assert!(t.tick(NOW, IDLE));
        assert!(t.ack("claude-code", "s", NOW, IDLE));
        assert!(!t.tick(NOW + 1_000, IDLE), "ack 已同步签名,下一轮不重复 emit");
        assert_eq!(one(&t, NOW + 1_000), Some(("waiting", NOW, true)));
        assert!(!t.ack("claude-code", "s", NOW, IDLE), "重复确认无变化");
        // 用户回复 → 模型再答完:新一段等待自动复位
        t.apply(BTreeMap::from([(key("s"), obs(LivePhase::Busy, NOW + 60_000))]));
        t.apply(BTreeMap::from([(key("s"), obs(LivePhase::Done { exact: true }, NOW + 90_000))]));
        assert_eq!(one(&t, NOW + 91_000), Some(("waiting", NOW + 90_000, false)));
        assert!(!t.ack("claude-code", "missing", NOW, IDLE));
    }

    #[test]
    fn expiry_idle_and_subagents() {
        let mut t = AttentionTable::default();
        let mut child = obs(LivePhase::Busy, NOW + TOOL_PENDING_MS);
        child.parent_id = Some("s".into());
        t.apply(BTreeMap::from([
            (key("s"), obs(LivePhase::Tools, NOW)),
            (key("sub"), child),
            (key("gone"), obs(LivePhase::Idle, NOW)),
        ]));
        let now = NOW + TOOL_PENDING_MS + 1_000;
        let items = t.items(now, IDLE);
        assert_eq!(items.len(), 1, "子会话与无状态会话不列出");
        assert_eq!(items[0].state, "running", "子代理活跃 → 父会话长工具不算等权限");
        assert!(t.tick(now, IDLE));
        // 超过离开阈值无事件 → 剔除
        assert!(t.tick(NOW + IDLE + TOOL_PENDING_MS + 1, IDLE));
        assert!(t.items(NOW + IDLE + TOOL_PENDING_MS + 1, IDLE).is_empty());
    }

    #[test]
    fn running_activity_does_not_re_emit() {
        let mut t = AttentionTable::default();
        t.apply(BTreeMap::from([(key("s"), obs(LivePhase::Busy, NOW))]));
        assert!(t.tick(NOW, IDLE));
        t.apply(BTreeMap::from([(key("s"), obs(LivePhase::Busy, NOW + 30_000))]));
        assert!(!t.tick(NOW + 30_000, IDLE));
    }

    #[test]
    fn remove_drops_entry_until_source_writes_again() {
        let mut t = AttentionTable::default();
        let mut o = obs(LivePhase::Done { exact: true }, NOW);
        o.host = Some("claude-vscode".into());
        t.apply(BTreeMap::from([(key("s"), o)]));
        assert!(t.tick(NOW, IDLE));
        assert_eq!(t.lookup("claude-code", "s"), Some((Some("claude-vscode".into()), "e:/p".into())));
        assert!(t.remove("claude-code", "s", NOW, IDLE), "移除亮着的条目 = 有变");
        assert!(t.items(NOW, IDLE).is_empty());
        assert_eq!(t.lookup("claude-code", "s"), None);
        assert!(!t.remove("claude-code", "s", NOW, IDLE), "重复移除无变");
        assert!(!t.tick(NOW + 1_000, IDLE), "remove 已同步签名");
        // 源再写入 → 重新建立
        t.apply(BTreeMap::from([(key("s"), obs(LivePhase::Done { exact: true }, NOW + 5_000))]));
        assert_eq!(one(&t, NOW + 6_000).map(|x| x.0), Some("waiting"));
    }

    #[test]
    fn acks_survive_restart_until_next_waiting_period() {
        let mut t = AttentionTable::default();
        t.apply(BTreeMap::from([
            (key("seen"), obs(LivePhase::Done { exact: true }, NOW)),
            (key("gone"), obs(LivePhase::Done { exact: true }, NOW)),
            (key("moved"), obs(LivePhase::Done { exact: true }, NOW)),
        ]));
        t.ack("claude-code", "seen", NOW, IDLE);
        t.ack("claude-code", "moved", NOW, IDLE);
        t.remove("claude-code", "gone", NOW, IDLE);
        let saved = t.acks();
        // 重启:表清空 → 播种 → 恢复标记
        let mut t = AttentionTable::default();
        t.seed(key("seen"), obs(LivePhase::Done { exact: true }, NOW));
        t.seed(key("gone"), obs(LivePhase::Done { exact: true }, NOW));
        t.seed(key("moved"), obs(LivePhase::Done { exact: true }, NOW + 50_000));
        t.seed(key("fresh"), obs(LivePhase::Done { exact: true }, NOW));
        t.restore_acks(saved);
        let acked = |s: &str| t.items(NOW + 60_000, IDLE).into_iter().find(|i| i.session_id == s).map(|i| i.acked);
        assert_eq!(acked("seen"), Some(true), "确认过的不再提醒");
        assert_eq!(acked("gone"), Some(true), "移除过的伪等待不再提醒");
        assert_eq!(acked("moved"), Some(false), "期间进入新一段等待 → 照常亮起");
        assert_eq!(acked("fresh"), Some(false));
        t.prune(NOW + 50_000 + IDLE + 1, IDLE);
        assert!(t.acks().is_empty(), "随离开阈值过期");
    }

    #[test]
    fn seed_keeps_newest_observation() {
        let mut t = AttentionTable::default();
        t.seed(key("s"), obs(LivePhase::Done { exact: true }, NOW + 10));
        t.seed(key("s"), obs(LivePhase::Busy, NOW));
        assert_eq!(one(&t, NOW + 20).map(|x| x.0), Some("waiting"));
    }

    // ---- 前台自动确认 / 暂压 ----

    fn desktop(phase: LivePhase, last: i64, input: Option<i64>) -> LiveTurn {
        let mut o = obs(phase, last);
        o.host = Some("claude-desktop".into());
        o.last_input = input;
        o
    }

    fn claude_window(window: isize) -> Option<ForegroundWindow> {
        Some(ForegroundWindow { window, exe: "claude.exe".into(), title: "Claude".into(), since: 0, input_at: 0 })
    }

    fn state_of(t: &AttentionTable, s: &str, now: i64) -> Option<(bool, bool)> {
        t.items(now, IDLE).into_iter().find(|i| i.session_id == s).map(|i| (i.acked, i.held))
    }

    #[test]
    fn foreground_acks_waiting_lit_before_entering_after_dwell() {
        let mut t = AttentionTable::default();
        t.apply(BTreeMap::from([(key("s"), desktop(LivePhase::Done { exact: true }, NOW, Some(NOW - 30_000)))]));
        t.tick(NOW, IDLE);
        // 答完之后才切进窗口
        assert!(!t.set_foreground(claude_window(1), NOW + 10_000, IDLE), "刚进前台:不压（亮在之前）也不确认（停留不够）");
        assert_eq!(state_of(&t, "s", NOW + 10_000), Some((false, false)));
        assert!(!t.set_foreground(claude_window(1), NOW + 10_000 + FOREGROUND_DWELL_MS - 1, IDLE));
        assert!(t.set_foreground(claude_window(1), NOW + 10_000 + FOREGROUND_DWELL_MS, IDLE), "停留够 → 确认");
        assert_eq!(state_of(&t, "s", NOW + 13_000), Some((true, false)));
    }

    #[test]
    fn alt_tab_passing_through_does_not_ack() {
        let mut t = AttentionTable::default();
        t.apply(BTreeMap::from([(key("s"), desktop(LivePhase::Done { exact: true }, NOW, Some(NOW - 1)))]));
        t.set_foreground(claude_window(1), NOW + 1_000, IDLE);
        t.set_foreground(Some(ForegroundWindow { window: 9, exe: "explorer.exe".into(), title: "x".into(), since: 0, input_at: 0 }), NOW + 2_000, IDLE);
        t.set_foreground(claude_window(1), NOW + 3_500, IDLE);
        t.set_foreground(claude_window(1), NOW + 4_000, IDLE);
        assert_eq!(state_of(&t, "s", NOW + 4_000), Some((false, false)), "换窗口重新计停留");
    }

    fn claude_active(window: isize, input_at: i64) -> Option<ForegroundWindow> {
        let mut w = claude_window(window);
        w.as_mut().unwrap().input_at = input_at;
        w
    }

    #[test]
    fn waiting_lit_while_in_foreground_is_held_then_relights_on_leave() {
        let mut t = AttentionTable::default();
        t.apply(BTreeMap::from([(key("s"), desktop(LivePhase::Busy, NOW, Some(NOW)))]));
        t.set_foreground(claude_window(1), NOW, IDLE);
        // 在窗口里看着它答完,之后没再动键鼠（可能人刚走开）→ 只暂压,不确认
        t.apply(BTreeMap::from([(key("s"), desktop(LivePhase::Done { exact: true }, NOW + 20_000, Some(NOW)))]));
        assert!(t.tick(NOW + 21_000, IDLE));
        assert!(!t.set_foreground(claude_active(1, NOW + 19_000), NOW + 60_000, IDLE), "答完后无操作:保持暂压");
        assert_eq!(state_of(&t, "s", NOW + 60_000), Some((false, true)));
        // 离开（或人走开 → 喂 None）→ 恢复亮起
        assert!(t.set_foreground(None, NOW + 61_000, IDLE));
        assert_eq!(state_of(&t, "s", NOW + 61_000), Some((false, false)));
        // 回来：从回来那一刻重新计停留（切换事件的旧时刻不算）,够了 → 确认
        let mut back = claude_window(1);
        back.as_mut().unwrap().since = NOW;
        assert!(!t.set_foreground(back.clone(), NOW + 70_000, IDLE));
        assert!(t.set_foreground(back, NOW + 72_000, IDLE));
        assert_eq!(state_of(&t, "s", NOW + 72_000), Some((true, false)));
    }

    #[test]
    fn held_waiting_is_acked_once_user_keeps_working_in_the_window() {
        let mut t = AttentionTable::default();
        t.apply(BTreeMap::from([(key("s"), desktop(LivePhase::Busy, NOW, Some(NOW)))]));
        t.set_foreground(claude_window(1), NOW, IDLE);
        t.apply(BTreeMap::from([(key("s"), desktop(LivePhase::Done { exact: true }, NOW + 20_000, Some(NOW)))]));
        t.tick(NOW + 21_000, IDLE);
        assert!(!t.set_foreground(claude_active(1, NOW + 22_000), NOW + 20_000 + HELD_ACK_MS - 1, IDLE), "时间不够");
        assert!(t.set_foreground(claude_active(1, NOW + 22_000), NOW + 20_000 + HELD_ACK_MS, IDLE), "答完后还在操作 → 确认");
        assert_eq!(state_of(&t, "s", NOW + 31_000), Some((true, false)));
        // 离开窗口不再亮起
        assert!(!t.set_foreground(None, NOW + 40_000, IDLE));
    }

    #[test]
    fn all_sessions_of_the_foreground_window_are_acked() {
        let mut t = AttentionTable::default();
        // 单窗口宿主里并行的会话（含无输入记录的旧游标）：到窗口面前一起熄灭
        t.apply(BTreeMap::from([
            (key("a"), desktop(LivePhase::Done { exact: true }, NOW + 10_000, Some(NOW))),
            (key("b"), desktop(LivePhase::Done { exact: true }, NOW + 12_000, Some(NOW + 5_000))),
            (key("c"), desktop(LivePhase::Done { exact: true }, NOW + 12_000, None)),
        ]));
        t.set_foreground(claude_window(1), NOW + 20_000, IDLE);
        t.set_foreground(claude_window(1), NOW + 23_000, IDLE);
        for s in ["a", "b", "c"] {
            assert_eq!(state_of(&t, s, NOW + 23_000), Some((true, false)), "{s}");
        }
    }

    #[test]
    fn unknown_hosts_are_left_alone() {
        let mut t = AttentionTable::default();
        // CLI（entrypoint 未知）会话:不参与
        t.apply(BTreeMap::from([(key("cli"), obs(LivePhase::Done { exact: true }, NOW))]));
        t.set_foreground(claude_window(1), NOW + 1_000, IDLE);
        t.set_foreground(claude_window(1), NOW + 5_000, IDLE);
        assert_eq!(state_of(&t, "cli", NOW + 5_000), Some((false, false)));
    }

    #[test]
    fn tool_pending_is_never_auto_acked() {
        let mut t = AttentionTable::default();
        t.apply(BTreeMap::from([(key("s"), desktop(LivePhase::Tools, NOW, Some(NOW)))]));
        let now = NOW + TOOL_PENDING_MS + 1_000;
        t.set_foreground(claude_window(1), now, IDLE);
        t.set_foreground(claude_window(1), now + 10_000, IDLE);
        let it = t.items(now + 10_000, IDLE).remove(0);
        assert_eq!((it.state, it.acked, it.held), ("tool_pending", false, false));
    }

    #[test]
    fn vscode_matches_by_project_title() {
        let mut t = AttentionTable::default();
        let mut o = obs(LivePhase::Done { exact: true }, NOW);
        o.host = Some("claude-vscode".into());
        o.project_key = "e:/projects/tokencalendar".into();
        o.last_input = Some(NOW - 1);
        t.apply(BTreeMap::from([(key("s"), o)]));
        let vs = |title: &str| Some(ForegroundWindow { window: 7, exe: "code.exe".into(), title: title.into(), since: 0, input_at: 0 });
        t.set_foreground(vs("main.rs - OtherProj - Visual Studio Code"), NOW + 1_000, IDLE);
        t.set_foreground(vs("main.rs - OtherProj - Visual Studio Code"), NOW + 5_000, IDLE);
        assert_eq!(state_of(&t, "s", NOW + 5_000), Some((false, false)), "别的项目窗口不算");
        let other =
            Some(ForegroundWindow { window: 8, exe: "code.exe".into(), title: "AGENTS.md - TokenCalendar - Visual Studio Code".into(), since: 0, input_at: 0 });
        t.set_foreground(other.clone(), NOW + 6_000, IDLE);
        t.set_foreground(other, NOW + 8_000, IDLE);
        assert_eq!(state_of(&t, "s", NOW + 8_000), Some((true, false)));
    }

    #[test]
    fn event_since_survives_sampling_gaps() {
        // 守护线程挂起期间窗口已在前台:切换事件给出的进前台时刻早于答完 → 仍按「在前台期间答完」暂压
        let mut t = AttentionTable::default();
        t.apply(BTreeMap::from([(key("s"), desktop(LivePhase::Done { exact: true }, NOW + 20_000, Some(NOW)))]));
        let mut w = claude_window(1);
        w.as_mut().unwrap().since = NOW;
        t.set_foreground(w, NOW + 25_000, IDLE);
        assert_eq!(state_of(&t, "s", NOW + 25_000), Some((false, true)));
    }

    #[test]
    fn other_projects_and_agents_are_not_touched() {
        let mut t = AttentionTable::default();
        let vs = |project: &str, input: i64| {
            let mut o = obs(LivePhase::Done { exact: true }, NOW);
            o.host = Some("claude-vscode".into());
            o.project_key = project.into();
            o.last_input = Some(input);
            o
        };
        let mut codex = obs(LivePhase::Done { exact: true }, NOW);
        codex.project_key = "e:/projects/tokencalendar".into();
        codex.last_input = Some(NOW - 1);
        t.apply(BTreeMap::from([
            (key("mine"), vs("e:/projects/tokencalendar", NOW - 10)),
            // 子串相似的别的项目,且输入更晚（若混进组会抢走「当前会话」）
            (key("calendar"), vs("e:/work/calendar", NOW - 1)),
            // 同一项目、另一个 agent
            (("codex".to_string(), "cx".to_string()), codex),
        ]));
        let w = Some(ForegroundWindow { window: 3, exe: "code.exe".into(), title: "a.rs - TokenCalendar - Visual Studio Code".into(), since: 0, input_at: 0 });
        t.set_foreground(w.clone(), NOW + 1_000, IDLE);
        t.set_foreground(w, NOW + 4_000, IDLE);
        assert_eq!(state_of(&t, "mine", NOW + 4_000), Some((true, false)));
        assert_eq!(state_of(&t, "calendar", NOW + 4_000), Some((false, false)), "别的项目不受影响");
        assert_eq!(state_of(&t, "cx", NOW + 4_000), Some((false, false)), "同项目的别的 agent 不受影响");
    }

    #[test]
    fn same_folder_name_in_different_paths_is_ambiguous() {
        let mut t = AttentionTable::default();
        let vs = |project: &str, input: i64| {
            let mut o = obs(LivePhase::Done { exact: true }, NOW);
            o.host = Some("claude-vscode".into());
            o.project_key = project.into();
            o.last_input = Some(input);
            o
        };
        t.apply(BTreeMap::from([(key("a"), vs("e:/a/app", NOW - 10)), (key("b"), vs("d:/b/app", NOW - 1))]));
        let w = Some(ForegroundWindow { window: 3, exe: "code.exe".into(), title: "x.rs - app - Visual Studio Code".into(), since: 0, input_at: 0 });
        t.set_foreground(w.clone(), NOW + 1_000, IDLE);
        t.set_foreground(w, NOW + 4_000, IDLE);
        assert_eq!(state_of(&t, "a", NOW + 4_000), Some((false, false)));
        assert_eq!(state_of(&t, "b", NOW + 4_000), Some((false, false)), "标题分不清是哪个 app,整组不动");
    }

    #[test]
    fn watch_needs_and_list_cover_unacked_lit_sessions_only() {
        let mut t = AttentionTable::default();
        assert_eq!(t.watch_needs(NOW, IDLE), (false, false), "空表 → 守护线程挂起");
        let mut w = desktop(LivePhase::Done { exact: true }, NOW, Some(NOW - 1));
        w.watch = Some(("c:/a.jsonl".into(), 5));
        let mut r = obs(LivePhase::Busy, NOW);
        r.watch = Some(("c:/b.jsonl".into(), 6));
        t.apply(BTreeMap::from([(key("w"), w), (key("r"), r)]));
        assert_eq!(t.watch_needs(NOW + 1, IDLE), (true, true));
        assert_eq!(t.watch_list(NOW + 1, IDLE), vec![("c:/a.jsonl".to_string(), 5)]);
        t.ack("claude-code", "w", NOW + 1, IDLE);
        assert_eq!(t.watch_needs(NOW + 1, IDLE), (false, false), "确认后不再采样 / 探针");
        assert!(t.watch_list(NOW + 1, IDLE).is_empty());
        // CLI 会话（宿主不可判定）只需要探针
        let mut cli = obs(LivePhase::Done { exact: true }, NOW);
        cli.watch = Some(("c:/c.jsonl".into(), 7));
        t.apply(BTreeMap::from([(key("cli"), cli)]));
        assert_eq!(t.watch_needs(NOW + 1, IDLE), (false, true));
    }
}
