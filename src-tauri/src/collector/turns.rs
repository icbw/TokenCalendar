//! 轮累加器（JSONL 族共用）：把「用户输入 → 模型调用 / 工具调用循环 → 下一次输入」
//! 累计成轮级事实（turn_raw / turn_part 行）,随游标持久化,跨批次正确。
//!
//! 口径：
//! - **三层计数**：turns（轮）/ model_calls（一轮内有 token 的响应次数,同一响应拆成多行时
//!   按响应 id 去重）/ tool_calls（工具调用起始数）;steps = model_calls。
//! - **四段时间**（毫秒）：wall = 轮首 → 轮内最后事件（系统注入输入之后的事件不延长 wall）;
//!   model = Σ（响应时间 − 前一事件时间）;tool = Σ（工具结果时间 − 工具调用时间）;
//!   gap = 输入 − 上一轮最后事件（同会话,原始值不截断,首轮 NULL）。JSONL 族的
//!   model / tool 由相邻事件时间戳推算,是**估算值**;ttft 一律 NULL（仅 ZCode 有真值）。
//! - **中止轮**：用户输入后没拿到任何响应就被下一次输入顶掉 → model_calls = 0、aborted = 1;
//!   适配器见到源的显式中止事件（Codex turn_aborted、DSH interrupted）调 `abort`。
//!   **S4-R :中止（用户主动）与错误（API / 工具失败,`error`）分列,中止不计 error_count。**
//! - **待定输入**：适配器可把一次输入标为「待定」（Claude:同文件已出现过 `origin`
//!   字段而本行缺 `origin`——本地斜杠命令 / 命令输出 / 中断标记）。待定轮拿到响应即转为普通轮;
//!   零调用就被顶掉则**丢弃**（不写 turn_raw、不计中止错误、轮号复用、gap 基准不前移）。
//! - **token 守恒**：`response` 同时写 daily_usage（`Batch:add_usage`）与 turn_part,
//!   两边同一份 （day, model, tokens, turns) ⇒ daily_project 折叠项目后与 daily_usage 恒等。
//!   `turn_mark` = 该响应是否计入 request_count（pending 模式的那一次）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::attention::{LivePhase, LiveTurn};
use super::store::{Batch, SessionRow, Tokens, TurnPart, TurnRow};

/// 未知项目 / 缺目录源的 project_key（不猜）。
pub const UNKNOWN_PROJECT: &str = "unknown";
/// 近期响应 id 去重窗口（同一响应的流式分块行一般连续出现,留余量防交错）。
const RECENT_RESPONSES: usize = 32;

/// 工作目录 → project_key：盘符小写、反斜杠转正斜杠、去尾斜杠、去 `\\?\` 前缀;
/// 空串 → unknown。只做形态归一,不做大小写折叠与 git remote 推断。
pub fn normalize_project(dir: &str) -> String {
    let mut s = dir.trim().replace('\\', "/");
    if let Some(rest) = s.strip_prefix("//?/") {
        s = rest.to_string();
    }
    while s.len() > 1 && s.ends_with('/') && !(s.len() == 3 && s.as_bytes()[1] == b':') {
        s.pop();
    }
    if s.len() == 3 && s.ends_with(":/") {
        s.pop();
    }
    // Git Bash 工具把 cwd 写成 `/e/Work/X`（同一会话里与 `E:\Work\X` 混用）,
    // 折成盘符形式,否则同一项目会裂出第三个键。只在 Windows 上做:类 Unix 系统 `/e/...` 是真实路径。
    if cfg!(windows) {
        let b = s.as_bytes();
        if b.len() >= 2 && b[0] == b'/' && b[1].is_ascii_alphabetic() && (b.len() == 2 || b[2] == b'/') {
            s = format!("{}:{}", (b[1] as char).to_ascii_lowercase(), &s[2..]);
        }
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        s = format!("{}{}", (b[0] as char).to_ascii_lowercase(), &s[1..]);
    }
    if s.is_empty() { UNKNOWN_PROJECT.to_string() } else { s }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RecentResponse {
    id: String,
    /// 该响应是否已计过一次 model_call（首个带 token 的行）。
    #[serde(default)]
    counted: bool,
    /// 已入账的 [input, output, total, cache_read, cache_write]（取各行最大值,增量入账）。
    tokens: [i64; 5],
}

/// 进行中的一轮（游标持久化）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TurnAcc {
    pub seq: i64,
    pub started_at: i64,
    pub start_day: String,
    pub project_key: String,
    #[serde(default)]
    pub gap_ms: Option<i64>,
    /// 是否由真实用户输入开启（false = 子会话提示 / 无输入的隐式轮,不做中止判定）。
    #[serde(default)]
    pub user_input: bool,
    /// 待定输入（见文件头「待定输入」）:零调用零工具时由 `TurnState:drop_tentative` 决定丢弃。
    #[serde(default)]
    pub tentative: bool,
    pub last_event: i64,
    pub wall_end: i64,
    #[serde(default)]
    pub wall_frozen: bool,
    /// 源自带的轮耗时（Codex task_complete.duration_ms）,优先于时间戳差。
    #[serde(default)]
    pub explicit_wall: Option<i64>,
    /// model_ms 推算锚点 = 最近一个事件时间。
    pub anchor: i64,
    #[serde(default)]
    pub open_tools: BTreeMap<String, i64>,
    #[serde(default)]
    pub model_calls: i64,
    #[serde(default)]
    pub tool_calls: i64,
    #[serde(default)]
    pub error_count: i64,
    #[serde(default)]
    pub retry_count: i64,
    /// 用户中止（S4-R,与 error_count 分列）。
    #[serde(default)]
    pub aborted: bool,
    /// 模型已答完、在等用户（0 = 否;1 = 启发式,源无显式信号;2 = 源显式信号,
    /// 如 Claude `stop_reason` 非 tool_use）。任何后续响应 / 工具 / 错误事件清零。
    #[serde(default)]
    pub model_done: u8,
    #[serde(default)]
    pub model_ms: i64,
    #[serde(default)]
    pub tool_ms: i64,
    #[serde(default)]
    pub first_day: Option<String>,
    #[serde(default)]
    pub first_model: Option<String>,
    #[serde(default)]
    pub parts: Vec<TurnPart>,
}

/// 单个会话文件的轮状态（嵌进各源游标,serde default 兼容旧游标）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TurnState {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub project_key: String,
    #[serde(default)]
    pub last_model: String,
    /// 【内容列】会话标题（仅本地可视化;rank 高者优先,如 Claude customTitle > aiTitle）。
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub title_rank: u8,
    #[serde(default)]
    pub seq: i64,
    /// 原始层 turn_seq 的文件命名空间（同一会话可跨多个文件:Codex 续写分段
    /// `<原会话>_<新段>.jsonl`、Claude 续聊新文件首行沿用旧 sessionId——各文件轮号都从 1 起,
    /// 不隔离会互相覆盖）。物化 turn 表时按开始时间重排为 1..n。
    #[serde(default)]
    pub seq_base: i64,
    #[serde(default)]
    pub prev_end: Option<i64>,
    #[serde(default)]
    pub open: Option<TurnAcc>,
    /// 待定输入的丢弃开关（适配器置位;Claude = 本文件出现过 `origin` 字段）。
    #[serde(default)]
    pub drop_tentative: bool,
    /// 最近闭合的一轮是否为用户中止（Codex / DSH 显式闭轮后据此判断是否在等用户）。
    #[serde(default)]
    pub last_aborted: bool,
    /// 源自己记录的会话项目（见 `set_session_project`;None = 会话行取当前轮目录、首写胜）。
    #[serde(default)]
    pub session_project: Option<String>,
    /// 桌面宿主线索（Claude Code = 行内 `entrypoint`,最近一次见到的值;其余源 None = 宿主固定
    /// 由 agent 决定）。只供 `focus_agent_window` 选目标进程,不进任何口径。
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    recent: Vec<RecentResponse>,
}

impl TurnState {
    /// 会话标识只取首个（文件 = 会话单元,中途出现的其他 sessionId 不改归属）。
    pub fn set_session(&mut self, id: &str) {
        if self.session_id.is_empty() && !id.is_empty() {
            self.session_id = id.to_string();
        }
    }

    /// 把本文件折进根会话——Claude 续聊 / fork 副本文件的 `sessionId` 已被改写成新 id,
    /// 适配器按已计行判定它是某根会话的续篇后强制覆盖归属（不受「只取首个」约束）。
    pub fn fold_into(&mut self, root: &str) {
        if !root.is_empty() {
            self.session_id = root.to_string();
        }
    }

    /// 以文件名设定轮号命名空间（FNV-1a 低 31 位 × 100000;每文件轮数 < 100000）。
    pub fn set_file_scope(&mut self, file_name: &str) {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in file_name.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        self.seq_base = ((h & 0x7fff_ffff) as i64) * 100_000;
    }

    pub fn set_parent(&mut self, parent: &str) {
        if !parent.is_empty() && parent != self.session_id {
            self.parent_id = Some(parent.to_string());
        }
    }

    /// 记宿主线索（最近一次见到的值胜;空串忽略）。
    pub fn set_host(&mut self, host: &str) {
        if !host.is_empty() && self.host.as_deref() != Some(host) {
            self.host = Some(host.to_string());
        }
    }

    /// 工作目录（按轮归属:开着的轮随之更新,会话表记首轮目录）。
    pub fn set_project(&mut self, dir: &str) {
        let key = normalize_project(dir);
        if key == UNKNOWN_PROJECT {
            return;
        }
        if let Some(open) = self.open.as_mut() {
            open.project_key = key.clone();
        }
        self.project_key = key;
    }

    /// 源自己记录的会话项目（Codex `threads.cwd` = 当前工作区）。会话行以它为准、后写胜;
    /// 轮仍按各自目录归属。
    pub fn set_session_project(&mut self, dir: &str) {
        let key = normalize_project(dir);
        if key != UNKNOWN_PROJECT {
            self.session_project = Some(key);
        }
    }

    pub fn set_title(&mut self, title: &str, rank: u8) {
        let t = title.trim();
        if !t.is_empty() && rank >= self.title_rank {
            self.title = Some(t.to_string());
            self.title_rank = rank;
        }
    }

    pub fn set_model(&mut self, model: &str) {
        if !model.is_empty() {
            self.last_model = model.to_string();
        }
    }

    fn project(&self) -> String {
        if self.project_key.is_empty() { UNKNOWN_PROJECT.to_string() } else { self.project_key.clone() }
    }

    /// 当前轮是否为应丢弃的待定轮（零调用零工具）。
    fn open_is_dropped(&self) -> bool {
        self.drop_tentative
            && self.open.as_ref().map_or(false, |o| o.tentative && o.model_calls == 0 && o.tool_calls == 0)
    }

    /// 把当前轮标为待定输入（紧跟 `begin` 调用）。
    pub fn mark_tentative(&mut self) {
        if let Some(open) = self.open.as_mut() {
            open.tentative = true;
        }
    }

    /// 开新轮（先闭合旧轮）。`user_input` = 真实用户输入;旧轮若也是用户轮且没拿到任何
    /// 响应 → 中止轮（aborted = 1,不计 error）;旧轮是应丢弃的待定轮 → 丢弃并复用其轮号
    /// （此前文件末尾若已落过该行,新轮同键整行覆盖）。
    pub fn begin(&mut self, batch: &mut Batch, agent: &str, ts: i64, user_input: bool) {
        let dropped = self.open_is_dropped();
        if let Some(open) = self.open.as_mut().filter(|_| !dropped) {
            if open.user_input && open.model_calls == 0 {
                open.aborted = true;
            }
        }
        self.close(batch, agent);
        self.seq += 1;
        let key_seq = self.seq_base + self.seq;
        let start_day = super::millis_to_local_day_hour(ts).map(|(d, _)| d).unwrap_or_default();
        self.open = Some(TurnAcc {
            seq: key_seq,
            started_at: ts,
            start_day,
            project_key: self.project(),
            gap_ms: self.prev_end.map(|e| (ts - e).max(0)),
            user_input,
            last_event: ts,
            wall_end: ts,
            anchor: ts,
            ..TurnAcc::default()
        });
    }

    fn ensure(&mut self, batch: &mut Batch, agent: &str, ts: i64) -> &mut TurnAcc {
        if self.open.is_none() {
            self.begin(batch, agent, ts, false);
        }
        self.open.as_mut().expect("open turn")
    }

    /// 普通事件：推进最近事件时间与 wall 终点（不计时间段）。
    pub fn touch(&mut self, ts: i64) {
        if let Some(open) = self.open.as_mut() {
            open.last_event = open.last_event.max(ts);
            if !open.wall_frozen {
                open.wall_end = open.wall_end.max(ts);
            }
            open.anchor = open.anchor.max(ts);
        }
    }

    /// 系统注入输入（Claude task-notification / compact summary）:其后事件不再延长 wall。
    pub fn freeze_wall(&mut self, ts: i64) {
        self.touch(ts);
        if let Some(open) = self.open.as_mut() {
            open.wall_frozen = true;
        }
    }

    /// 一次带 token 的响应（含同一响应 id 的后续分块行）。同时写 daily_usage 与 turn_part。
    ///
    /// - 同一 `response_id` 的后续行只入账各分项增量（流式分块行 usage 相同或递增）;
    /// - model_calls 在该响应**首次带 token** 时计 1;
    /// - `turn_mark`（调用方 pending 状态）只在真正入账 token 时生效,返回实际落下的 mark
    ///   （1 = 调用方应清 pending）,保证 daily_usage.request_count 与 turn_part 同源。
    #[allow(clippy::too_many_arguments)]
    pub fn response(
        &mut self,
        batch: &mut Batch,
        agent: &str,
        ts: i64,
        hour: Option<u8>,
        model: &str,
        tokens: Tokens,
        response_id: Option<&str>,
        turn_mark: i64,
    ) -> i64 {
        let raw = [tokens.input, tokens.output, tokens.total, tokens.cache_read, tokens.cache_write];
        let mut delta = raw;
        let mut already_counted = false;
        if let Some(id) = response_id {
            match self.recent.iter_mut().find(|r| r.id == id) {
                Some(prev) => {
                    for i in 0..5 {
                        delta[i] = (raw[i] - prev.tokens[i]).max(0);
                        prev.tokens[i] = prev.tokens[i].max(raw[i]);
                    }
                    already_counted = prev.counted;
                }
                None => {
                    self.recent.push(RecentResponse { id: id.to_string(), counted: false, tokens: raw });
                    if self.recent.len() > RECENT_RESPONSES {
                        self.recent.remove(0);
                    }
                }
            }
        }
        let has_tokens = delta.iter().any(|v| *v != 0);
        self.set_model(model);
        let Some(day) = super::millis_to_local_day_hour(ts).map(|(d, _)| d) else { return 0 };
        {
            let open = self.ensure(batch, agent, ts);
            open.model_ms += (ts - open.anchor).max(0);
            open.model_done = 0;
        }
        if !has_tokens {
            self.touch(ts);
            return 0;
        }
        let new_call = !already_counted;
        if new_call {
            if let Some(id) = response_id {
                if let Some(r) = self.recent.iter_mut().find(|r| r.id == id) {
                    r.counted = true;
                }
            }
        }
        let mark = turn_mark.clamp(0, 1);
        let t = Tokens { input: delta[0], output: delta[1], total: delta[2], cache_read: delta[3], cache_write: delta[4] };
        {
            let open = self.open.as_mut().expect("open turn");
            if new_call {
                open.model_calls += 1;
                if open.first_day.is_none() {
                    open.first_day = Some(day.clone());
                    open.first_model = Some(model.to_string());
                }
            }
            match open.parts.iter_mut().find(|p| p.day == day && p.model == model) {
                Some(p) => {
                    p.input += t.input;
                    p.output += t.output;
                    p.total += t.total;
                    p.model_calls += new_call as i64;
                    p.turn_mark += mark;
                }
                None => open.parts.push(TurnPart {
                    day: day.clone(),
                    model: model.to_string(),
                    input: t.input,
                    output: t.output,
                    total: t.total,
                    model_calls: new_call as i64,
                    turn_mark: mark,
                }),
            }
        }
        self.touch(ts);
        batch.add_usage(&day, hour, agent, model, t, mark);
        mark
    }

    pub fn tool_start(&mut self, batch: &mut Batch, agent: &str, ts: i64, id: &str) {
        let open = self.ensure(batch, agent, ts);
        open.model_done = 0;
        if !open.open_tools.contains_key(id) {
            open.tool_calls += 1;
            open.open_tools.insert(id.to_string(), ts);
        }
        self.touch(ts);
    }

    /// 工具结果：配对到起始时间则累加 tool_ms;未配对（跨轮 / 丢行）只推进时间。
    pub fn tool_end(&mut self, ts: i64, id: &str) {
        if let Some(open) = self.open.as_mut() {
            open.model_done = 0;
            if let Some(start) = open.open_tools.remove(id) {
                open.tool_ms += (ts - start).max(0);
            }
        }
        self.touch(ts);
    }

    pub fn error(&mut self, batch: &mut Batch, agent: &str, ts: i64) {
        self.ensure(batch, agent, ts).error_count += 1;
        self.mark_done(ts, true);
        self.touch(ts);
    }

    /// 用户中止（源的显式中止事件）:只标 aborted,不计 error_count。
    pub fn abort(&mut self, batch: &mut Batch, agent: &str, ts: i64) {
        self.ensure(batch, agent, ts).aborted = true;
        self.touch(ts);
    }

    pub fn retry(&mut self, batch: &mut Batch, agent: &str, ts: i64) {
        let open = self.ensure(batch, agent, ts);
        open.retry_count += 1;
        open.model_done = 0;
        self.touch(ts);
    }

    /// 模型本次答完、轮停在等用户（`exact` = 源显式信号;false = 启发式,见 attention）。
    /// 未配对工具仍在时不生效（模型在等工具结果,不是在等用户）。
    pub fn mark_done(&mut self, ts: i64, exact: bool) {
        self.touch(ts);
        if let Some(open) = self.open.as_mut().filter(|o| o.open_tools.is_empty()) {
            open.model_done = if exact { 2 } else { 1 };
        }
    }

    /// 注意力观测——文件末尾（flush）的会话现状,供 attention 表派生 running / waiting。
    /// 开着的轮看 `open_tools` / `model_done`;已显式闭轮（Codex task_complete、DSH turn/end）
    /// = 模型答完在等用户（用户中止的除外）;应丢弃的待定轮（Claude 中断标记 / 斜杠命令）= 无状态。
    pub fn observe(&self) -> LiveTurn {
        let (phase, last_event, project_key) = match self.open.as_ref() {
            Some(_) if self.open_is_dropped() => (LivePhase::Idle, self.prev_end.unwrap_or(0), self.project()),
            Some(o) => {
                let phase = if !o.open_tools.is_empty() {
                    LivePhase::Tools
                } else if o.model_done > 0 {
                    LivePhase::Done { exact: o.model_done >= 2 }
                } else {
                    LivePhase::Busy
                };
                (phase, o.last_event, o.project_key.clone())
            }
            None => match self.prev_end {
                Some(end) if !self.last_aborted => (LivePhase::Done { exact: true }, end, self.project()),
                other => (LivePhase::Idle, other.unwrap_or(0), self.project()),
            },
        };
        LiveTurn { project_key, parent_id: self.parent_id.clone(), title: self.title.clone(), host: self.host.clone(), phase, last_event }
    }

    pub fn set_explicit_wall(&mut self, ms: i64) {
        if let Some(open) = self.open.as_mut() {
            if ms >= 0 {
                open.explicit_wall = Some(ms);
            }
        }
    }

    /// 闭合当前轮（会话结束事件 / 下一次输入）:落最终行并记上一轮终点（gap 基准）。
    /// 应丢弃的待定轮不落行、不前移 gap 基准,轮号退回给下一轮复用。
    pub fn close(&mut self, batch: &mut Batch, agent: &str) {
        if self.open_is_dropped() {
            self.open = None;
            self.seq -= 1;
        }
        self.flush(batch, agent);
        if let Some(open) = self.open.take() {
            self.prev_end = Some(open.last_event);
            self.last_aborted = open.aborted;
        }
    }

    /// 落当前轮现状（文件末尾调用;轮不闭合,下批续累加后整行覆盖）+ 会话行。
    pub fn flush(&self, batch: &mut Batch, agent: &str) {
        if self.session_id.is_empty() {
            return;
        }
        // 同一会话以文件末尾最后一次 flush 为准（close 内的 flush 被后续覆盖）
        batch.live.insert((agent.to_string(), self.session_id.clone()), self.observe());
        let mut session = SessionRow {
            session_id: self.session_id.clone(),
            project_key: self.session_project.clone(),
            project_authoritative: self.session_project.is_some(),
            parent_id: self.parent_id.clone(),
            title: self.title.clone(),
            started_at: None,
            ended_at: None,
        };
        if let Some(open) = self.open.as_ref().filter(|_| !self.open_is_dropped()) {
            if session.project_key.is_none() {
                session.project_key = Some(open.project_key.clone());
            }
            session.started_at = Some(open.started_at);
            session.ended_at = Some(open.last_event);
            let wall = open.explicit_wall.unwrap_or((open.wall_end - open.started_at).max(0));
            batch.add_turn(
                agent,
                TurnRow {
                    session_id: self.session_id.clone(),
                    turn_seq: open.seq,
                    day: open.first_day.clone().unwrap_or_else(|| open.start_day.clone()),
                    project_key: open.project_key.clone(),
                    model_key: open
                        .first_model
                        .clone()
                        .or_else(|| (!self.last_model.is_empty()).then(|| self.last_model.clone()))
                        .unwrap_or_else(|| "unknown".to_string()),
                    started_at: open.started_at,
                    ended_at: open.last_event,
                    wall_ms: Some(wall),
                    model_ms: Some(open.model_ms),
                    tool_ms: Some(open.tool_ms),
                    ttft_ms: None,
                    gap_ms: open.gap_ms,
                    model_calls: open.model_calls,
                    tool_calls: open.tool_calls,
                    error_count: open.error_count,
                    retry_count: open.retry_count,
                    aborted: open.aborted,
                    parts: open.parts.clone(),
                },
            );
        } else if self.title.is_none() && self.session_project.is_none() {
            return;
        }
        batch.upsert_session(agent, session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_788_602_400_000; // 2026-09-05 本地日内

    fn tok(i: i64, o: i64) -> Tokens {
        Tokens { input: i, output: o, total: i + o, cache_read: 0, cache_write: 0 }
    }

    #[test]
    fn project_key_normalization() {
        assert_eq!(normalize_project(r"E:\Work\Demo\"), "e:/Work/Demo");
        assert_eq!(normalize_project("e:/Work/Demo"), "e:/Work/Demo");
        assert_eq!(normalize_project(r"\\?\D:\Program\x"), "d:/Program/x");
        assert_eq!(normalize_project(r"C:\"), "c:");
        assert_eq!(normalize_project("/home/u/proj/"), "/home/u/proj");
        assert_eq!(normalize_project("/"), "/");
        assert_eq!(normalize_project("  "), UNKNOWN_PROJECT);
        if cfg!(windows) {
            assert_eq!(normalize_project("/e/Work/Demo"), "e:/Work/Demo", "Git Bash 形式");
            assert_eq!(normalize_project("/e"), "e:");
            assert_eq!(normalize_project("/etc/x"), "/etc/x", "非单字母段不折");
        }
    }

    #[test]
    fn three_counts_and_four_times() {
        let mut b = Batch::default();
        let mut st = TurnState::default();
        st.set_session("s1");
        st.set_project(r"E:\p");
        // 轮 1:输入 → 响应(5s) → 工具(3s) → 响应(2s)
        st.begin(&mut b, "a", T0, true);
        assert_eq!(st.response(&mut b, "a", T0 + 5_000, Some(9), "m", tok(10, 1), Some("r1"), 1), 1);
        st.tool_start(&mut b, "a", T0 + 5_000, "c1");
        st.tool_end(T0 + 8_000, "c1");
        assert_eq!(st.response(&mut b, "a", T0 + 10_000, Some(9), "m", tok(20, 2), Some("r2"), 0), 0);
        // 同一响应的分块行:output 递增 → 只入增量,不算新调用
        assert_eq!(st.response(&mut b, "a", T0 + 11_000, Some(9), "m", tok(20, 5), Some("r2"), 0), 0);
        // 轮 2:间隔 60s
        st.begin(&mut b, "a", T0 + 71_000, true);
        st.flush(&mut b, "a");

        let r1 = &b.turns[&("a".to_string(), "s1".to_string(), 1)];
        assert_eq!((r1.model_calls, r1.tool_calls), (2, 1));
        assert_eq!(r1.wall_ms, Some(11_000));
        assert_eq!(r1.model_ms, Some(5_000 + 2_000 + 1_000));
        assert_eq!(r1.tool_ms, Some(3_000));
        assert_eq!(r1.gap_ms, None);
        assert_eq!(r1.ttft_ms, None);
        assert_eq!(r1.project_key, "e:/p");
        let sum: i64 = r1.parts.iter().map(|p| p.total).sum();
        assert_eq!(sum, 11 + 25, "分块行按增量入账");
        assert_eq!(r1.parts.iter().map(|p| p.turn_mark).sum::<i64>(), 1);
        let r2 = &b.turns[&("a".to_string(), "s1".to_string(), 2)];
        assert_eq!(r2.gap_ms, Some(60_000));
        assert_eq!((r2.model_calls, r2.error_count), (0, 0), "文件末尾未闭合的轮不判中止");
        // daily_usage 同源:tokens 与 turns 与 turn_part 一致
        let daily: i64 = b.entries.values().map(|e| e[2]).sum();
        assert_eq!(daily, 36);
        assert_eq!(b.entries.values().map(|e| e[3]).sum::<i64>(), 1);
    }

    #[test]
    fn file_scope_separates_turn_keys() {
        let mut b = Batch::default();
        let (mut a, mut c) = (TurnState::default(), TurnState::default());
        for (st, file) in [(&mut a, "orig.jsonl"), (&mut c, "orig_seg2.jsonl")] {
            st.set_file_scope(file);
            st.set_session("same-session");
            st.begin(&mut b, "x", T0, true);
            st.flush(&mut b, "x");
        }
        assert_eq!(b.turns.len(), 2, "同会话两文件的第 1 轮不得互相覆盖");
    }

    #[test]
    fn aborted_turn_and_frozen_wall() {
        let mut b = Batch::default();
        let mut st = TurnState::default();
        st.set_session("s");
        st.begin(&mut b, "a", T0, true);
        st.begin(&mut b, "a", T0 + 4_000, true); // 上一轮无响应被顶掉 → 中止
        st.response(&mut b, "a", T0 + 6_000, None, "m", tok(1, 1), None, 1);
        st.freeze_wall(T0 + 7_000); // 系统注入
        st.response(&mut b, "a", T0 + 60_000, None, "m", tok(1, 1), None, 0);
        st.close(&mut b, "a");
        let r1 = &b.turns[&("a".to_string(), "s".to_string(), 1)];
        assert_eq!((r1.model_calls, r1.error_count, r1.aborted, r1.wall_ms), (0, 0, true, Some(0)), "中止不计错");
        let r2 = &b.turns[&("a".to_string(), "s".to_string(), 2)];
        assert_eq!(r2.gap_ms, Some(4_000));
        assert_eq!(r2.wall_ms, Some(3_000), "注入之后的事件不延长 wall");
        assert_eq!((r2.model_calls, r2.aborted), (2, false));
        assert_eq!(st.prev_end, Some(T0 + 60_000), "gap 基准取最后事件");
    }

    /// S4-R:显式中止与错误分列——同一轮可以既被中止又带 API 错误,两列各记各的。
    #[test]
    fn explicit_abort_does_not_count_as_error() {
        let mut b = Batch::default();
        let mut st = TurnState::default();
        st.set_session("s");
        st.begin(&mut b, "a", T0, true);
        st.response(&mut b, "a", T0 + 1_000, None, "m", tok(1, 1), None, 1);
        st.error(&mut b, "a", T0 + 2_000);
        st.abort(&mut b, "a", T0 + 3_000);
        st.close(&mut b, "a");
        st.begin(&mut b, "a", T0 + 10_000, true);
        st.response(&mut b, "a", T0 + 11_000, None, "m", tok(1, 1), None, 1);
        st.close(&mut b, "a");
        let r1 = &b.turns[&("a".to_string(), "s".to_string(), 1)];
        assert_eq!((r1.error_count, r1.aborted, r1.wall_ms), (1, true, Some(3_000)));
        let r2 = &b.turns[&("a".to_string(), "s".to_string(), 2)];
        assert_eq!((r2.error_count, r2.aborted), (0, false));
        // 旧游标（无 aborted 字段）反序列化为 false
        let old: TurnAcc = serde_json::from_str(r#"{"seq":1,"started_at":0,"start_day":"","project_key":"","last_event":0,"wall_end":0,"anchor":0}"#).unwrap();
        assert!(!old.aborted);
    }

    /// PHASE14 S3:显式闭轮 = 答完等用户;中止闭轮 = 无状态;有未配对工具时 mark_done 不生效;
    /// flush 以最后一次为准。
    #[test]
    fn observe_live_phase() {
        let mut b = Batch::default();
        let mut st = TurnState::default();
        st.set_session("s");
        st.set_project(r"E:\p");
        assert_eq!(st.observe().phase, LivePhase::Idle, "空会话");
        st.begin(&mut b, "a", T0, true);
        assert_eq!(st.observe().phase, LivePhase::Busy);
        st.response(&mut b, "a", T0 + 1_000, None, "m", tok(1, 1), None, 1);
        st.tool_start(&mut b, "a", T0 + 1_000, "c");
        st.mark_done(T0 + 1_500, true);
        assert_eq!(st.observe().phase, LivePhase::Tools, "工具未回不算答完");
        st.tool_end(T0 + 2_000, "c");
        st.response(&mut b, "a", T0 + 3_000, None, "m", tok(1, 1), None, 0);
        st.mark_done(T0 + 3_000, false);
        assert_eq!(st.observe().phase, LivePhase::Done { exact: false });
        st.close(&mut b, "a"); // 显式闭轮（Codex task_complete）
        st.flush(&mut b, "a");
        let live = &b.live[&("a".to_string(), "s".to_string())];
        assert_eq!((live.phase, live.last_event, live.project_key.as_str()), (LivePhase::Done { exact: true }, T0 + 3_000, "e:/p"));
        st.begin(&mut b, "a", T0 + 10_000, true);
        st.abort(&mut b, "a", T0 + 11_000);
        st.close(&mut b, "a");
        assert_eq!(st.observe().phase, LivePhase::Idle, "用户中止后不亮起");
    }

    #[test]
    fn state_survives_serde_roundtrip() {
        let mut b = Batch::default();
        let mut st = TurnState::default();
        st.set_session("s");
        st.begin(&mut b, "a", T0, true);
        st.tool_start(&mut b, "a", T0 + 1_000, "c");
        let json = serde_json::to_string(&st).unwrap();
        let mut back: TurnState = serde_json::from_str(&json).unwrap();
        back.tool_end(T0 + 4_000, "c");
        back.response(&mut b, "a", T0 + 5_000, None, "m", tok(1, 1), None, 1);
        back.flush(&mut b, "a");
        let r = &b.turns[&("a".to_string(), "s".to_string(), 1)];
        assert_eq!((r.tool_ms, r.model_ms, r.tool_calls), (Some(3_000), Some(1_000), 1));
        // 旧游标（无 turn 字段）反序列化为默认
        let empty: TurnState = serde_json::from_str("{}").unwrap();
        assert!(empty.open.is_none());
    }
}
