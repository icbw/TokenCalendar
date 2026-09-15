//! Claude Code 适配器：`~/.claude/projects/**/*.jsonl` 递归（CLAUDE_CONFIG_DIR 可覆盖）。
//!
//! 口径（跟随旧项目）：仅 `type=="assistant"` 且 `message.usage` 存在的行；
//! input/output 取 `message.usage.input_tokens / output_tokens` 原始值（cache 分项
//! 独立字段,不计入 total）；total = input + output（Anthropic 无 total 字段的旧约定）；
//! 时间取顶层 `timestamp`（RFC3339）→ 本地日。模型缺失填 "unknown"。
//!
//! **v8 token 去重**：一次 API 响应被流式拆成多条 assistant 行（thinking / text / tool_use
//! 各一行）,每行都带同一份 `message.usage`（本机：10075 行 / 4660 个
//! `message.id`,重复行 5288 条 usage 完全相同、134 条 output 递增）。按 `message.id`
//! 只入账增量——旧口径逐行相加,Claude token 约重计 1 倍。
//!
//! 对话轮计数：「真实用户输入行」置 pending 标志——type=="user" 且无 toolUseResult、
//! 无 tool_result 块、非 sidechain / meta / compact summary,且 `origin.kind` 缺省或为
//! `human`（`task-notification` 等是系统注入,不是用户发起）;下一条 assistant usage 行
//! 按其模型计 1 turn 并清位,pending 持久化进游标。
//!
//! 零调用输入：本文件出现过 `origin` 字段的前提下,缺 `origin` 的用户行（本机:
//! 本地斜杠命令 `<command-name>`、命令输出 `<local-command-stdout>`、`[Request interrupted…]`
//! 标记,64/64 均不带 origin;386 条真实输入全带 `origin.kind=human`）记为**待定输入**:拿到响应
//! 照常成轮计 request_count,零调用则不成轮（不写 turn_raw、不计中止错误）。全文件无 origin 的
//! 旧 CLI 文件维持原判（缺 origin 即真实输入,零调用按中止轮）。
//!
//! 轮与时间：真实输入开轮;assistant 行 = 模型调用（按 message.id 去重）,
//! `tool_use` 块 id → `tool_result.tool_use_id` 配对算 tool_ms;`isApiErrorMessage` 计错,
//! `system/api_error` 计重试;系统注入输入之后的事件不延长 wall。
//! 会话：主文件 = `sessionId`（文件内首个）;`<session>/subagents/agent-*.jsonl` 为子会话
//! （行带 `isSidechain` + `agentId`）,session_id = agentId、parent = sessionId,每条提示行开子轮。
//! 标题（内容列）：`custom-title.customTitle` 优先于 `ai-title.aiTitle`。

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

use super::store::{Batch, Store, Tokens};
use super::{
    Adapter, AdapterError, AdapterMeta, CollectOutcome, CollectResult, FileCursor, ProbeOutcome,
    advance_file, clamp0, load_cursor, rfc3339_to_local_day_hour, rfc3339_to_millis, seal_cursor,
};

pub struct ClaudeCodeAdapter {
    projects_dir: PathBuf,
}

static META: AdapterMeta = AdapterMeta {
    id: "claude-code",
    name: "Claude Code",
    location: "~/.claude/projects",
    kind: "jsonl",
};

/// 用户行的分类。
#[derive(Debug, PartialEq)]
enum UserKind {
    /// 真实用户输入（开轮 + 置 pending）。
    Human,
    /// 系统注入（task-notification / compact summary）:不开轮,冻结 wall。
    Injected,
    /// 工具结果回传（带 tool_result 块的 tool_use_id）。
    ToolResults(Vec<String>),
    /// 子会话的提示行（sidechain 非工具结果）。
    SidechainPrompt,
    /// meta 等其余用户行。
    Other,
}

fn user_kind(v: &Value) -> UserKind {
    let content = v.pointer("/message/content").and_then(|c| c.as_array());
    let tool_ids: Vec<String> = content
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                .filter_map(|b| b.get("tool_use_id").and_then(|x| x.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let has_tool_result = v.get("toolUseResult").map_or(false, |x| !x.is_null());
    if has_tool_result || !tool_ids.is_empty() {
        return UserKind::ToolResults(tool_ids);
    }
    let flag = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    if flag("isMeta") {
        return UserKind::Other;
    }
    if flag("isSidechain") {
        return UserKind::SidechainPrompt;
    }
    let origin_kind = v.pointer("/origin/kind").and_then(|k| k.as_str());
    if flag("isCompactSummary") || origin_kind.map_or(false, |k| k != "human") {
        return UserKind::Injected;
    }
    UserKind::Human
}

/// assistant 行的 usage 提取：（本地日, 本地小时, 模型, token 分项);input 与 output 全零 → None。
fn assistant_usage(v: &Value) -> Option<(String, u8, String, Tokens)> {
    let usage = v.pointer("/message/usage")?;
    let get = |k: &str| clamp0(usage.get(k).and_then(|x| x.as_i64()).unwrap_or(0));
    let (input, output) = (get("input_tokens"), get("output_tokens"));
    if input == 0 && output == 0 {
        return None;
    }
    let model = v
        .pointer("/message/model")
        .and_then(|m| m.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string();
    let (day, hour) = rfc3339_to_local_day_hour(v.get("timestamp")?.as_str()?)?;
    let tokens = Tokens {
        input,
        output,
        total: input + output,
        cache_read: get("cache_read_input_tokens"),
        cache_write: get("cache_creation_input_tokens"),
    };
    Some((day, hour, model, tokens))
}

impl ClaudeCodeAdapter {
    pub fn new() -> Self {
        let base = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| super::home_dir().map(|h| h.join(".claude")));
        let projects_dir = base.map(|b| b.join("projects")).unwrap_or_default();
        ClaudeCodeAdapter { projects_dir }
    }

    /// 单行处理：会话 / 项目 / 标题元数据 + 轮事件 + token 入账（daily_usage 与 turn_part 同源）。
    fn process_line(line: &str, cursor: &mut FileCursor, batch: &mut Batch, months: &mut BTreeSet<String>) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        let agent = META.id;
        let st = &mut cursor.turn;
        let sidechain = v.get("isSidechain").and_then(|x| x.as_bool()).unwrap_or(false);
        if let Some(sid) = v.get("sessionId").and_then(|x| x.as_str()) {
            match v.get("agentId").and_then(|x| x.as_str()).filter(|_| sidechain) {
                Some(agent_id) => {
                    st.set_session(agent_id);
                    st.set_parent(sid);
                }
                None => st.set_session(sid),
            }
        }
        if let Some(cwd) = v.get("cwd").and_then(|x| x.as_str()) {
            st.set_project(cwd);
        }
        let has_origin = v.get("origin").is_some();
        if has_origin {
            st.drop_tentative = true;
        }
        let ts = v.get("timestamp").and_then(|t| t.as_str()).and_then(rfc3339_to_millis);
        match v.get("type").and_then(|t| t.as_str()) {
            Some("custom-title") => {
                if let Some(t) = v.get("customTitle").and_then(|x| x.as_str()) {
                    st.set_title(t, 2);
                }
            }
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(|x| x.as_str()) {
                    st.set_title(t, 1);
                }
            }
            Some("user") => {
                let Some(ts) = ts else { return };
                match user_kind(&v) {
                    UserKind::Human if st.parent_id.is_none() => {
                        st.begin(batch, agent, ts, true);
                        if !has_origin {
                            st.mark_tentative();
                        }
                        cursor.pending_turn = true;
                    }
                    UserKind::Human | UserKind::SidechainPrompt => st.begin(batch, agent, ts, false),
                    UserKind::Injected => st.freeze_wall(ts),
                    UserKind::ToolResults(ids) => {
                        for id in &ids {
                            st.tool_end(ts, id);
                        }
                        st.touch(ts);
                    }
                    UserKind::Other => {}
                }
            }
            Some("assistant") => {
                let Some(ts) = ts else { return };
                if let Some((day, hour, model, tokens)) = assistant_usage(&v) {
                    let mark = (cursor.pending_turn && st.parent_id.is_none()) as i64;
                    let id = v.pointer("/message/id").and_then(|x| x.as_str());
                    if st.response(batch, agent, ts, Some(hour), &model, tokens, id, mark) == 1 {
                        cursor.pending_turn = false;
                    }
                    cursor.model = model;
                    months.insert(day[..7].to_string());
                }
                if let Some(blocks) = v.pointer("/message/content").and_then(|c| c.as_array()) {
                    for b in blocks {
                        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            if let Some(id) = b.get("id").and_then(|x| x.as_str()) {
                                st.tool_start(batch, agent, ts, id);
                            }
                        }
                    }
                }
                if v.get("isApiErrorMessage").and_then(|x| x.as_bool()).unwrap_or(false) {
                    st.error(batch, agent, ts);
                }
            }
            Some("system") => {
                if let Some(ts) = ts {
                    if v.get("subtype").and_then(|x| x.as_str()) == Some("api_error") {
                        st.retry(batch, agent, ts);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Adapter for ClaudeCodeAdapter {
    fn meta(&self) -> &'static AdapterMeta {
        &META
    }

    fn probe(&self) -> ProbeOutcome {
        if self.projects_dir.is_dir() {
            ProbeOutcome { status: "ready".into(), fingerprint: None }
        } else {
            ProbeOutcome { status: "no_source".into(), fingerprint: None }
        }
    }

    fn collect(&self, store: &mut Store) -> CollectResult {
        if !self.projects_dir.is_dir() {
            return Err(AdapterError::new("no_source", format!("missing {}", self.projects_dir.display())));
        }
        let mut files = Vec::new();
        super::jsonl::discover(&self.projects_dir, true, &mut files);
        super::jsonl::sort_by_mtime(&mut files);

        let mut batch = Batch::default();
        let mut months = BTreeSet::new();

        for path in files {
            let scope = path.display().to_string();
            let mut cursor = load_cursor(store, META.id, &scope);
            let Some(consume) = advance_file(&path, &mut cursor) else { continue };
            let mut cursor = if consume.reset {
                let mut c = FileCursor::fresh();
                c.model = cursor.model.clone();
                c
            } else {
                cursor
            };
            cursor.turn.set_file_scope(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
            for line in &consume.lines {
                Self::process_line(line, &mut cursor, &mut batch, &mut months);
            }
            // 文件末尾:落当前轮现状（轮不闭合,下批续累加后整行覆盖）
            cursor.turn.flush(&mut batch, META.id);
            cursor.offset = consume.new_offset;
            seal_cursor(&mut cursor, &path, &scope, &mut batch);
        }

        store.commit(META.id, &batch).map_err(|e| AdapterError::new("error", e))?;
        Ok(CollectOutcome { events: batch.events, months })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- 冻结样本行（2026-09-15 本机真实结构,正文 / 标题 / 路径已脱敏,键集保持） ----------

    const S: &str = "0a1b2c3d-0000-4000-8000-000000000001";
    fn human(ts: &str, origin: Option<&str>) -> String {
        let origin = origin.map(|k| format!(r#","origin":{{"kind":"{k}"}},"promptSource":"sdk""#)).unwrap_or_default();
        format!(r#"{{"parentUuid":null,"isSidechain":false,"type":"user","message":{{"role":"user","content":"<redacted>"}},"uuid":"u-{ts}","timestamp":"{ts}","permissionMode":"auto","userType":"external","entrypoint":"claude-desktop","cwd":"E:\\Work\\Demo","sessionId":"{S}","version":"2.1.0","gitBranch":"main"{origin}}}"#)
    }
    fn assistant(ts: &str, mid: &str, block: &str, input: i64, output: i64) -> String {
        format!(r#"{{"parentUuid":"p","isSidechain":false,"message":{{"model":"claude-opus-5","id":"{mid}","type":"message","role":"assistant","content":[{block}],"stop_reason":null,"usage":{{"input_tokens":{input},"cache_creation_input_tokens":120,"cache_read_input_tokens":4800,"output_tokens":{output},"service_tier":"standard"}}}},"requestId":"req_{mid}","type":"assistant","uuid":"a-{ts}","timestamp":"{ts}","userType":"external","entrypoint":"claude-desktop","cwd":"E:\\Work\\Demo","sessionId":"{S}","version":"2.1.0","gitBranch":"main"}}"#)
    }
    fn tool_result(ts: &str, id: &str) -> String {
        format!(r#"{{"parentUuid":"p","isSidechain":false,"type":"user","message":{{"role":"user","content":[{{"tool_use_id":"{id}","type":"tool_result","content":"<redacted>"}}]}},"uuid":"t-{ts}","timestamp":"{ts}","toolUseResult":{{"stdout":"","stderr":""}},"sourceToolAssistantUUID":"a","userType":"external","cwd":"E:\\Work\\Demo","sessionId":"{S}","version":"2.1.0"}}"#)
    }
    const TITLE_AI: &str = r#"{"type":"ai-title","aiTitle":"<ai title>","sessionId":"0a1b2c3d-0000-4000-8000-000000000001"}"#;
    const TITLE_CUSTOM: &str = r#"{"type":"custom-title","customTitle":"<custom title>","sessionId":"0a1b2c3d-0000-4000-8000-000000000001"}"#;
    const API_RETRY: &str = r#"{"parentUuid":"p","isSidechain":false,"type":"system","subtype":"api_error","level":"error","error":{"status":529},"retryInMs":1000,"retryAttempt":1,"maxRetries":10,"timestamp":"2026-09-05T10:00:40.000Z","uuid":"s1","userType":"external","cwd":"E:\\Work\\Demo","sessionId":"0a1b2c3d-0000-4000-8000-000000000001","version":"2.1.0"}"#;

    fn sub_line(ts: &str, kind: &str) -> String {
        let body = match kind {
            "prompt" => r#""type":"user","message":{"role":"user","content":"<redacted>"}"#.to_string(),
            _ => r#""type":"assistant","message":{"model":"claude-haiku-4-5","id":"msg_sub1","type":"message","role":"assistant","content":[{"type":"text","text":"<redacted>"}],"usage":{"input_tokens":50,"output_tokens":5}}"#.to_string(),
        };
        format!(r#"{{"parentUuid":null,"isSidechain":true,"agentId":"a1b2c3d4e5","userType":"external","cwd":"E:\\Work\\Demo","sessionId":"{S}","version":"2.1.0",{body},"uuid":"x-{ts}","timestamp":"{ts}"}}"#)
    }

    /// 缺 origin 的本地命令 / 命令输出 / 中断标记行（2026-09-15 本机真实键集,正文脱敏为结构标记）。
    fn no_origin(ts: &str, content: &str) -> String {
        format!(r#"{{"parentUuid":"p","isSidechain":false,"promptId":"pr-{ts}","type":"user","message":{{"role":"user","content":{content}}},"uuid":"n-{ts}","timestamp":"{ts}","userType":"external","entrypoint":"claude-desktop","cwd":"E:\\Work\\Demo","sessionId":"{S}","version":"2.1.0","gitBranch":"main"}}"#)
    }
    const CMD: &str = r#""<command-name>/redacted</command-name>""#;
    const CMD_OUT: &str = r#""<local-command-stdout><redacted></local-command-stdout>""#;
    const INTERRUPTED: &str = r#"[{"type":"text","text":"[Request interrupted by user]"}]"#;

    fn collect_lines(tag: &str, batches: &[Vec<String>]) -> Store {
        let dir = std::env::temp_dir().join(format!("tc_claude_{tag}_{}", std::process::id()));
        let proj = dir.join("projects").join("E--Projects-Demo");
        std::fs::create_dir_all(&proj).unwrap();
        let file = proj.join(format!("{S}.jsonl"));
        let adapter = ClaudeCodeAdapter { projects_dir: dir.join("projects") };
        let mut store = Store::open_in_memory().unwrap();
        let mut text = String::new();
        let mut ok = true;
        for lines in batches {
            text.push_str(&(lines.join("
") + "
"));
            std::fs::write(&file, &text).unwrap();
            // 追加写 = 尺寸变化,游标按增量续读（模拟跨批次）
            ok &= adapter.collect(&mut store).is_ok();
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok);
        store
    }

    /// S3 定案:同文件出现过 origin → 缺 origin 的零调用行不成轮;拿到响应的照常成轮;跨批次正确。
    #[test]
    fn zero_call_lines_without_origin_are_not_turns() {
        let text = r#"{"type":"text","text":"x"}"#;
        let store = collect_lines(
            "no_origin",
            &[
                vec![
                    human("2026-09-05T10:00:00.000Z", Some("human")),
                    assistant("2026-09-05T10:00:05.000Z", "msg_a", text, 100, 10),
                    no_origin("2026-09-05T10:01:00.000Z", CMD),
                    no_origin("2026-09-05T10:01:00.500Z", CMD_OUT),
                    human("2026-09-05T10:02:00.000Z", Some("human")),
                    assistant("2026-09-05T10:02:03.000Z", "msg_b", text, 100, 10),
                    // 批末:中断标记（待定,零调用）→ 本批不落行
                    no_origin("2026-09-05T10:03:00.000Z", INTERRUPTED),
                ],
                vec![
                    // 下一批:被真实输入顶掉 → 丢弃,轮号复用
                    human("2026-09-05T10:04:00.000Z", Some("human")),
                    assistant("2026-09-05T10:04:02.000Z", "msg_c", text, 100, 10),
                    // 缺 origin 但拿到响应（展开成提示的斜杠命令）→ 照常成轮并计 request_count
                    no_origin("2026-09-05T10:05:00.000Z", CMD),
                    assistant("2026-09-05T10:05:04.000Z", "msg_d", text, 100, 10),
                ],
            ],
        );
        let turns = store.test_turns(META.id);
        assert_eq!(turns.len(), 4, "{turns:?}");
        assert!(turns.iter().all(|t| t.model_calls == 1 && t.error_count == 0), "零调用待定行不成轮、不计错");
        assert_eq!(turns[1].gap_ms, Some(115_000), "gap 基准不因命令行前移（10:00:05 → 10:02:00）");
        assert_eq!(turns[2].gap_ms, Some(117_000), "10:02:03 → 10:04:00");
        let (raw, ..) = store.test_task_stats(META.id);
        assert_eq!(raw, 4, "turn_raw 无残留待定行");
        let rows = store.month_rows("2026-09", "agent", "total", chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()).unwrap();
        assert_eq!(rows[0].message_counts.iter().sum::<i64>(), 4);
        assert!(store.test_project_conservation().is_empty());
    }

    /// 旧版全文件无 origin:缺 origin 仍是真实输入,零调用按中止轮（维持 S2 口径）。
    #[test]
    fn legacy_files_without_origin_keep_aborted_turns() {
        let text = r#"{"type":"text","text":"x"}"#;
        let store = collect_lines(
            "legacy",
            &[vec![
                human("2026-09-05T10:00:00.000Z", None),
                assistant("2026-09-05T10:00:05.000Z", "msg_a", text, 100, 10),
                no_origin("2026-09-05T10:01:00.000Z", CMD),
                human("2026-09-05T10:02:00.000Z", None),
                assistant("2026-09-05T10:02:03.000Z", "msg_b", text, 100, 10),
            ]],
        );
        let turns = store.test_turns(META.id);
        assert_eq!(turns.len(), 3);
        assert_eq!((turns[1].model_calls, turns[1].error_count, turns[1].aborted), (0, 0, true), "中止轮（不计错）");
    }

    #[test]
    fn user_kind_classification() {
        let v = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert_eq!(user_kind(&v(&human("2026-09-05T10:00:00Z", None))), UserKind::Human);
        assert_eq!(user_kind(&v(&human("2026-09-05T10:00:00Z", Some("human")))), UserKind::Human);
        assert_eq!(user_kind(&v(&human("2026-09-05T10:00:00Z", Some("task-notification")))), UserKind::Injected);
        assert_eq!(
            user_kind(&v(r#"{"type":"user","isCompactSummary":true,"isVisibleInTranscriptOnly":true,"message":{"content":"x"}}"#)),
            UserKind::Injected
        );
        assert_eq!(user_kind(&v(&tool_result("2026-09-05T10:00:00Z", "toolu_1"))), UserKind::ToolResults(vec!["toolu_1".into()]));
        // 无 toolUseResult 但带 tool_result 块（实证 232 行）→ 仍是工具结果
        assert_eq!(
            user_kind(&v(r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_9"}]}}"#)),
            UserKind::ToolResults(vec!["toolu_9".into()])
        );
        assert_eq!(user_kind(&v(r#"{"type":"user","isMeta":true,"message":{}}"#)), UserKind::Other);
        assert_eq!(user_kind(&v(&sub_line("2026-09-05T10:00:00Z", "prompt"))), UserKind::SidechainPrompt);
    }

    #[test]
    fn usage_fields_and_zero_skip() {
        let v: Value = serde_json::from_str(&assistant("2026-09-05T18:30:00.123Z", "msg_01", r#"{"type":"text","text":"x"}"#, 1234, 567)).unwrap();
        let (day, hour, model, t) = assistant_usage(&v).unwrap();
        assert_eq!((t.input, t.output, t.total, t.cache_read, t.cache_write), (1234, 567, 1801, 4800, 120));
        assert_eq!(model, "claude-opus-5");
        assert!(day.len() == 10 && hour <= 23);
        let zero: Value = serde_json::from_str(r#"{"type":"assistant","timestamp":"2026-09-05T18:30:00Z","message":{"model":"m","usage":{"input_tokens":0,"output_tokens":0}}}"#).unwrap();
        assert!(assistant_usage(&zero).is_none());
        let no_ts: Value = serde_json::from_str(r#"{"type":"assistant","message":{"model":"m","usage":{"input_tokens":1,"output_tokens":1}}}"#).unwrap();
        assert!(assistant_usage(&no_ts).is_none());
    }

    /// 端到端：主会话 + 子代理文件走真实 collect,断言三层计数、四段时间、去重、守恒。
    #[test]
    fn end_to_end_session_turns_and_subagent_merge() {
        let dir = std::env::temp_dir().join(format!("tc_claude_turns_{}", std::process::id()));
        let proj = dir.join("projects").join("E--Projects-Demo");
        let sub = proj.join(S).join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        let tool_block = r#"{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}"#;
        let main = [
            TITLE_AI.to_string(),
            human("2026-09-05T10:00:00.000Z", Some("human")),
            // 同一响应拆两行（thinking + tool_use）,usage 相同 → 只计 1 次调用、token 只入一次
            assistant("2026-09-05T10:00:04.000Z", "msg_a", r#"{"type":"thinking","thinking":""}"#, 100, 10),
            assistant("2026-09-05T10:00:05.000Z", "msg_a", tool_block, 100, 10),
            tool_result("2026-09-05T10:00:08.000Z", "toolu_1"),
            assistant("2026-09-05T10:00:10.000Z", "msg_b", r#"{"type":"text","text":"x"}"#, 200, 20),
            // 系统注入（后台任务通知）:不开轮、不计 request_count,之后事件不延长 wall
            human("2026-09-05T10:00:30.000Z", Some("task-notification")),
            API_RETRY.to_string(),
            assistant("2026-09-05T10:00:50.000Z", "msg_c", r#"{"type":"text","text":"x"}"#, 300, 30),
            TITLE_CUSTOM.to_string(),
            // 第 2 轮:用户输入后无响应就被第 3 轮顶掉 → 中止轮
            human("2026-09-05T10:05:00.000Z", Some("human")),
            human("2026-09-05T10:06:00.000Z", Some("human")),
            assistant("2026-09-05T10:06:02.000Z", "msg_d", r#"{"type":"text","text":"x"}"#, 10, 1),
        ];
        std::fs::write(proj.join(format!("{S}.jsonl")), main.join("\n") + "\n").unwrap();
        let subagent = [sub_line("2026-09-05T10:00:06.000Z", "prompt"), sub_line("2026-09-05T10:00:07.000Z", "reply")];
        std::fs::write(sub.join("agent-a1b2c3d4e5.jsonl"), subagent.join("\n") + "\n").unwrap();

        let adapter = ClaudeCodeAdapter { projects_dir: dir.join("projects") };
        let mut store = Store::open_in_memory().unwrap();
        let ok = adapter.collect(&mut store).is_ok();
        // 二次采集无新内容:幂等
        let ok2 = adapter.collect(&mut store).is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok && ok2);

        let turns = store.test_turns(META.id);
        assert_eq!(turns.len(), 3, "三次用户输入 = 三行（含中止轮）;注入与子会话不成轮");
        let t1 = &turns[0];
        assert_eq!((t1.model_calls, t1.tool_calls, t1.error_count, t1.retry_count), (3 + 1, 1, 0, 1));
        assert_eq!((t1.subagent_count, t1.subagent_calls), (1, 1), "子代理并入第 1 轮");
        assert_eq!(t1.wall_ms, Some(30_000), "注入之后的事件不延长 wall");
        assert_eq!(t1.tool_ms, Some(3_000));
        assert_eq!(t1.total_tokens, 110 + 220 + 330 + 55, "按 message.id 去重 + 子代理 token 并入");
        assert_eq!(t1.project_key, "e:/Work/Demo");
        assert_eq!(t1.gap_ms, None);
        assert_eq!(t1.ttft_ms, None);
        let t2 = &turns[1];
        assert_eq!((t2.model_calls, t2.error_count, t2.aborted, t2.wall_ms), (0, 0, true, Some(0)), "中止轮（不计错）");
        assert_eq!(t2.gap_ms, Some(250_000), "10:00:50 → 10:05:00");
        let t3 = &turns[2];
        assert_eq!((t3.model_calls, t3.gap_ms), (1, Some(60_000)));

        let sessions = store.test_sessions(META.id);
        let root = sessions.iter().find(|s| s.session_id == S).unwrap();
        assert_eq!(root.title.as_deref(), Some("<custom title>"), "customTitle 优先 aiTitle");
        assert_eq!((root.subagent_count, root.subagent_calls), (1, 1));
        let child = sessions.iter().find(|s| s.session_id == "a1b2c3d4e5").unwrap();
        assert_eq!(child.parent_id.as_deref(), Some(S));
        assert_eq!(store.test_task_sessions(META.id), vec![S.to_string()], "子会话不单独成任务");

        // request_count:两次真实输入拿到响应（第 1、3 轮）;注入与中止不计
        let rows = store.month_rows("2026-09", "agent", "total", chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()).unwrap();
        assert_eq!(rows[0].message_counts.iter().sum::<i64>(), 2);
        assert_eq!(rows[0].month_total, 110 + 220 + 330 + 11 + 55);
        assert!(store.test_project_conservation().is_empty(), "{:?}", store.test_project_conservation());
    }
}
