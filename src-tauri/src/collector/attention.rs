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

use std::collections::BTreeMap;

use serde::Serialize;

/// 未配对工具静默超过此值 → tool_pending（Claude Code 的 tool_result 要等权限批准后才写入,
/// ≈「在等你批权限」;与长时间工具有歧义,S3 按误报率调）。
pub const TOOL_PENDING_MS: i64 = 90_000;
/// 启发式「答完」信号的静默确认时长。
pub const HEURISTIC_SETTLE_MS: i64 = 60_000;

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
    pub phase: LivePhase,
    /// 会话最近事件时刻（毫秒）。
    pub last_event: i64,
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
    /// running | waiting | tool_pending
    pub state: &'static str,
    /// running = 最近事件;waiting = 模型答完时刻;tool_pending = 工具静默开始时刻。
    pub since: i64,
    pub last_event: i64,
    /// 已确认（仅 waiting / tool_pending;同一会话进入新一段等待自动复位）。
    pub acked: bool,
}

struct Entry {
    obs: LiveTurn,
    /// 确认时那一段等待的 `since`;派生出的 since 变了即视为新一段等待。
    acked_since: Option<i64>,
}

#[derive(Default)]
pub struct AttentionTable {
    entries: BTreeMap<LiveKey, Entry>,
    /// 上一次 emit 时的派生签名（running 的 since 随每个事件前移,不参与比较,防活跃期每轮都 emit）。
    last_sig: Vec<(String, String, &'static str, i64, bool)>,
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

    /// 某父会话下是否有近期活跃的子会话（Task 子代理在跑 → 父会话的长工具不是在等权限）。
    fn child_active(&self, agent: &str, session: &str, now: i64) -> bool {
        self.entries.iter().any(|((a, _), e)| {
            a == agent
                && e.obs.parent_id.as_deref() == Some(session)
                && e.obs.phase != LivePhase::Idle
                && now - e.obs.last_event < TOOL_PENDING_MS
        })
    }

    fn derive(&self, key: &LiveKey, e: &Entry, now: i64) -> Option<(&'static str, i64)> {
        let o = &e.obs;
        match o.phase {
            LivePhase::Idle => None,
            LivePhase::Busy => Some(("running", o.last_event)),
            LivePhase::Tools => {
                if now - o.last_event >= TOOL_PENDING_MS && !self.child_active(&key.0, &key.1, now) {
                    Some(("tool_pending", o.last_event))
                } else {
                    Some(("running", o.last_event))
                }
            }
            LivePhase::Done { exact } => {
                if exact || now - o.last_event >= HEURISTIC_SETTLE_MS {
                    Some(("waiting", o.last_event))
                } else {
                    Some(("running", o.last_event))
                }
            }
        }
    }

    /// 剔除：无状态、超过离开阈值无事件。
    pub fn prune(&mut self, now: i64, idle_ms: i64) {
        self.entries.retain(|_, e| e.obs.phase != LivePhase::Idle && now - e.obs.last_event <= idle_ms);
    }

    /// 当前派生快照（子会话不列出;按 since 先后,「不做优先级排序」）。
    pub fn items(&self, now: i64, idle_ms: i64) -> Vec<AttentionItem> {
        let mut out: Vec<AttentionItem> = self
            .entries
            .iter()
            .filter(|(_, e)| e.obs.parent_id.is_none() && now - e.obs.last_event <= idle_ms)
            .filter_map(|(key, e)| {
                let (state, since) = self.derive(key, e, now)?;
                Some(AttentionItem {
                    agent: key.0.clone(),
                    agent_label: super::store::agent_label(&key.0),
                    session_id: key.1.clone(),
                    project_key: e.obs.project_key.clone(),
                    title: e.obs.title.clone(),
                    state,
                    since,
                    last_event: e.obs.last_event,
                    acked: state != "running" && e.acked_since == Some(since),
                })
            })
            .collect();
        out.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| (&a.agent, &a.session_id).cmp(&(&b.agent, &b.session_id))));
        out
    }

    fn signature(items: &[AttentionItem]) -> Vec<(String, String, &'static str, i64, bool)> {
        items
            .iter()
            .map(|i| (i.agent.clone(), i.session_id.clone(), i.state, if i.state == "running" { 0 } else { i.since }, i.acked))
            .collect()
    }

    /// 每轮采集后调用：剔除过期 → 派生 → 与上次签名比较。返回 true = 需要 emit。
    pub fn tick(&mut self, now: i64, idle_ms: i64) -> bool {
        self.prune(now, idle_ms);
        let sig = Self::signature(&self.items(now, idle_ms));
        let changed = sig != self.last_sig;
        self.last_sig = sig;
        changed
    }

    /// 确认一个会话当前这一段等待。返回 true = 状态有变（需要 emit）。
    pub fn ack(&mut self, agent: &str, session: &str, now: i64, idle_ms: i64) -> bool {
        let key = (agent.to_string(), session.to_string());
        let Some(since) = self
            .entries
            .get(&key)
            .and_then(|e| self.derive(&key, e, now))
            .filter(|(state, _)| *state != "running")
            .map(|(_, since)| since)
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
        LiveTurn { project_key: "e:/p".into(), parent_id: None, title: Some("t".into()), phase, last_event: last }
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
    fn seed_keeps_newest_observation() {
        let mut t = AttentionTable::default();
        t.seed(key("s"), obs(LivePhase::Done { exact: true }, NOW + 10));
        t.seed(key("s"), obs(LivePhase::Busy, NOW));
        assert_eq!(one(&t, NOW + 20).map(|x| x.0), Some("waiting"));
    }
}
