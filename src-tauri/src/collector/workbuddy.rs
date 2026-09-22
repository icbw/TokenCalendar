//! WorkBuddy 适配器：`~/.workbuddy/projects/**/*.jsonl` 递归（provider_reported 主源）。
//!
//! 口径：
//! - `providerData.rawUsage` 存在的 `function_call` 行与 **assistant `message` 行**（纯文本
//!   最终回复的 usage 挂在 message 行上,漏掉会少计）；
//! - cache_read = `prompt_cache_hit_tokens`（clamp ≥0）；input = prompt_tokens - cache_read
//!   （prompt 已含 cache hit,拆出 cache-exclusive input）；output = completion_tokens；
//! - total = `rawUsage.total_tokens`（含 cache 与 reasoning,不重复加总）,≤0 时回退 input+output；
//! - 时间戳：顶层 `timestamp` 数字,> 1e10 视为毫秒否则秒；
//! - 模型：`providerData.requestModelId` 优先 → `providerData.model` → "unknown"。
//! - 积分：`rawUsage.credit` 按行入 `daily_usage.credit`（与该行 usage 同格）。带 usage 的行都带 credit、
//!   `messageId` 无重复行,按行入账不会重计。
//!
//! 对话轮计数：`type=="message" && role=="user"` 是真实用户输入（function_call
//! 是模型发起的**工具调用**,不是对话,勿计入）→ 置 pending 标志,下一条带 rawUsage
//! 的行按其模型计 1 turn 并清位;pending 持久化进游标。子代理文件
//! （`<session>/subagents/agent-*.jsonl`）的提示行不计轮。
//!
//! 轮与时间：用户 message 开轮;带 usage 的行 = 模型调用（按
//! `providerData.messageId` 去重）;`function_call.callId` → `function_call_result.callId`
//! 配对算 tool_ms;assistant message `status=="incomplete"` 计错（源不区分截断 / 失败 /
//! 中断,不当作用户中止;零响应被下一次输入顶掉的轮由累加器记中止）。会话 = `sessionId`,
//! 子代理文件 parent = 上两级目录名（= 主会话文件名 = 主 sessionId）;
//! 项目 = 文件所在文件夹（行内 `cwd` 只用于还原可读路径）;标题（内容列）= `ai-title.aiTitle`。
//!
//! db（session 级 estimated 口径）与 traces 兜底源不接入——避免低质量数据混入主指标。

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

use super::project_dir::{first_cwd, folder_of, FolderProjects};
use super::store::{Batch, Store, Tokens};
use super::turns::normalize_project;
use super::{
    Adapter, AdapterError, AdapterMeta, CollectOutcome, CollectResult, FileCursor, ProbeOutcome,
    advance_file, clamp0, epoch_number_to_millis, load_cursor, millis_to_local_day_hour, seal_cursor,
};

pub struct WorkBuddyAdapter {
    projects_dir: PathBuf,
}

static META: AdapterMeta = AdapterMeta {
    id: "workbuddy",
    name: "WorkBuddy",
    location: "~/.workbuddy/projects",
    kind: "jsonl",
};

enum WbLine {
    /// 真实用户输入行（message role=user,开启新 turn)。
    UserInput,
    /// 带 rawUsage 的 function_call / assistant message 行:（本地日, 本地小时, 模型, token 分项, 积分)。
    Usage { day: String, hour: u8, model: String, tokens: Tokens, credit: f64 },
    None,
}

impl WorkBuddyAdapter {
    pub fn new() -> Self {
        let projects_dir = super::home_dir()
            .map(|h| h.join(".workbuddy").join("projects"))
            .unwrap_or_default();
        WorkBuddyAdapter { projects_dir }
    }

    /// 从一行提取：真实用户输入 / 带 usage 行 / 无关（测试入口;采集走 `process_line`）。
    #[cfg(test)]
    fn parse_line(line: &str) -> WbLine {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return WbLine::None };
        Self::classify(&v)
    }

    fn classify(v: &Value) -> WbLine {
        match v.get("type").and_then(|t| t.as_str()) {
            Some("message") if v.get("role").and_then(|r| r.as_str()) == Some("user") => WbLine::UserInput,
            Some("message") | Some("function_call") => {
                let Some((day, hour, model, tokens)) = Self::parse_usage(v) else {
                    return WbLine::None;
                };
                let credit = v.pointer("/providerData/rawUsage/credit").and_then(|x| x.as_f64()).unwrap_or(0.0);
                WbLine::Usage { day, hour, model, tokens, credit }
            }
            _ => WbLine::None,
        }
    }

    /// 带 rawUsage 行的 usage 提取。
    /// cache_write = `prompt_cache_write_tokens`（缺失退 `cache_creation_input_tokens`）。
    fn parse_usage(v: &Value) -> Option<(String, u8, String, Tokens)> {
        let raw = v.get("providerData")?.get("rawUsage")?;
        let prompt = clamp0(raw.get("prompt_tokens").and_then(|x| x.as_i64()).unwrap_or(0));
        let completion = clamp0(raw.get("completion_tokens").and_then(|x| x.as_i64()).unwrap_or(0));
        let cache_read = clamp0(raw.get("prompt_cache_hit_tokens").and_then(|x| x.as_i64()).unwrap_or(0));
        let input = (prompt - cache_read).max(0);
        let cache_write = clamp0(
            raw.get("prompt_cache_write_tokens")
                .or_else(|| raw.get("cache_creation_input_tokens"))
                .and_then(|x| x.as_i64())
                .unwrap_or(0),
        );
        let total_raw = raw.get("total_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
        let total = if total_raw > 0 { total_raw } else { input + completion };
        if total <= 0 {
            return None;
        }
        let provider = v.get("providerData")?;
        let model = provider
            .get("requestModelId")
            .and_then(|m| m.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| provider.get("model").and_then(|m| m.as_str()).filter(|s| !s.is_empty()))
            .unwrap_or("unknown")
            .to_string();
        let ts = epoch_number_to_millis(v.get("timestamp")?.as_f64()?)?;
        let (day, hour) = millis_to_local_day_hour(ts)?;
        Some((day, hour, model, Tokens { input, output: completion, total, cache_read, cache_write }))
    }
}

impl WorkBuddyAdapter {
    fn process_line(line: &str, cursor: &mut FileCursor, batch: &mut Batch, months: &mut BTreeSet<String>, parent: Option<&str>) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        let agent = META.id;
        let st = &mut cursor.turn;
        let child = parent.is_some();
        if let Some(sid) = v.get("sessionId").and_then(|x| x.as_str()) {
            st.set_session(sid);
        }
        if let Some(p) = parent {
            st.set_parent(p);
        }
        // 项目由文件所在文件夹决定（collect 里设定）,行内 cwd 只用于还原可读路径。
        let ts = v.get("timestamp").and_then(|t| t.as_f64()).and_then(epoch_number_to_millis);
        let ty = v.get("type").and_then(|t| t.as_str());
        if ty == Some("ai-title") {
            if let Some(t) = v.get("aiTitle").and_then(|x| x.as_str()) {
                st.set_title(t, 1);
            }
            return;
        }
        let Some(ts) = ts else { return };
        match Self::classify(&v) {
            WbLine::UserInput => {
                st.begin(batch, agent, ts, !child);
                if !child {
                    cursor.pending_turn = true;
                }
            }
            WbLine::Usage { day, hour, model, tokens, credit } => {
                batch.add_credit(&day, agent, &model, credit);
                let mark = (cursor.pending_turn && !child) as i64;
                let id = v.pointer("/providerData/messageId").and_then(|x| x.as_str());
                if st.response(batch, agent, ts, Some(hour), &model, tokens, id, mark) == 1 {
                    cursor.pending_turn = false;
                }
                months.insert(day[..7].to_string());
            }
            WbLine::None => st.touch(ts),
        }
        match ty {
            Some("function_call") => {
                if let Some(id) = v.get("callId").and_then(|x| x.as_str()) {
                    st.tool_start(batch, agent, ts, id);
                }
            }
            Some("function_call_result") => {
                if let Some(id) = v.get("callId").and_then(|x| x.as_str()) {
                    st.tool_end(ts, id);
                }
            }
            Some("message") if v.get("status").and_then(|x| x.as_str()) == Some("incomplete") => {
                st.error(batch, agent, ts);
            }
            // 无显式答完信号 → assistant message 记启发式答完（其后工具调用即清零,
            // attention 侧再要求静默 HEURISTIC_SETTLE_MS 才亮起）
            Some("message") if v.get("role").and_then(|x| x.as_str()) == Some("assistant") => {
                st.mark_done(ts, false);
            }
            _ => {}
        }
    }
}

impl Adapter for WorkBuddyAdapter {
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
        let mut folders = FolderProjects::new(META.id);

        for path in files {
            let scope = path.display().to_string();
            let mut cursor = load_cursor(store, META.id, &scope);
            let Some(consume) = advance_file(&path, &mut cursor) else { continue };
            let mut cursor = if consume.reset { FileCursor::fresh() } else { cursor };
            // 项目 = 文件所在文件夹 `projects/<编码启动目录>/`（与 Claude Code 同构,见 project_dir）
            let project = match folder_of(&self.projects_dir, &path) {
                Some(folder) => folders.resolve(store, &folder, &consume.lines, &cursor.turn.project_key),
                None => first_cwd(&consume.lines).map(|c| normalize_project(&c)).unwrap_or_default(),
            };
            cursor.turn.set_project(&project);

            // 子代理文件:<projects>/<proj>/<session>/subagents/agent-*.jsonl → parent = <session>
            let parent_dir = path
                .parent()
                .filter(|d| d.file_name().and_then(|n| n.to_str()) == Some("subagents"))
                .and_then(|d| d.parent())
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                .map(str::to_string);
            cursor.turn.set_file_scope(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
            for line in &consume.lines {
                Self::process_line(line, &mut cursor, &mut batch, &mut months, parent_dir.as_deref());
            }
            cursor.turn.flush(&mut batch, META.id);
            cursor.offset = consume.new_offset;
            seal_cursor(&mut cursor, &path, &scope, &mut batch);
        }
        folders.persist(&mut batch);

        store.commit(META.id, &batch).map_err(|e| AdapterError::new("error", e))?;
        Ok(CollectOutcome { events: batch.events, months })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ts: f64, model_req: Option<&str>, model: &str, p: i64, c: i64, hit: i64, total: i64) -> String {
        let req = match model_req {
            Some(m) => format!(r#""requestModelId":"{m}","#),
            None => String::new(),
        };
        format!(
            r#"{{"id":"call_1","type":"function_call","timestamp":{ts},"sessionId":"s1","providerData":{{{req}"model":"{model}","rawUsage":{{"prompt_tokens":{p},"completion_tokens":{c},"total_tokens":{total},"prompt_cache_hit_tokens":{hit}}}}}}}"#
        )
    }

    fn expect_usage(line: &str) -> (String, u8, String, i64, i64, i64) {
        match WorkBuddyAdapter::parse_line(line) {
            WbLine::Usage { day, hour, model, tokens: t, .. } => (day, hour, model, t.input, t.output, t.total),
            _ => panic!("expected usage"),
        }
    }

    #[test]
    fn parses_input_cache_exclusive() {
        // prompt=1000（含 cache hit 400）,completion=200,total=1200
        let (day, _, model, input, output, total) =
            expect_usage(&line(1_757_000_000_000.0, Some("glm-5.3"), "glm-5.3", 1000, 200, 400, 1200));
        assert_eq!(input, 600);
        assert_eq!(output, 200);
        assert_eq!(total, 1200);
        assert_eq!(model, "glm-5.3");
        assert_eq!(day.len(), 10);
        let WbLine::Usage { tokens, .. } = WorkBuddyAdapter::parse_line(&line(1_757_000_000_000.0, None, "m", 1000, 200, 400, 1200)) else { panic!() };
        assert_eq!((tokens.cache_read, tokens.cache_write), (400, 0));
    }

    #[test]
    fn total_falls_back_to_io_when_nonpositive() {
        let (_, _, _, input, output, total) =
            expect_usage(&line(1_757_000_000_000.0, None, "m", 100, 50, 0, 0));
        assert_eq!((input, output, total), (100, 50, 150));
    }

    #[test]
    fn timestamp_seconds_vs_millis() {
        // 秒级时间戳（约 2026-09-05）
        let (d1, _, _, _, _, _) = expect_usage(&line(1_785_800_000.0, None, "m", 10, 5, 0, 15));
        // 毫秒级
        let (d2, _, _, _, _, _) = expect_usage(&line(1_785_800_000_000.0, None, "m", 10, 5, 0, 15));
        assert_eq!(d1, d2, "秒/毫秒应自适应到同一天");
        // 非法时间戳跳过
        assert!(matches!(WorkBuddyAdapter::parse_line(&line(0.0, None, "m", 10, 5, 0, 15)), WbLine::None));
        assert!(matches!(WorkBuddyAdapter::parse_line(&line(-5.0, None, "m", 10, 5, 0, 15)), WbLine::None));
    }

    #[test]
    fn user_message_marks_turn_but_not_usage() {
        // role=user 的 message 行 = 真实用户输入 → UserInput(不计 usage)
        assert!(matches!(
            WorkBuddyAdapter::parse_line(r#"{"id":"m1","timestamp":1757000000000,"type":"message","role":"user","content":[{"t":1}]}"#),
            WbLine::UserInput
        ));
        // 无 rawUsage 的 assistant message 行 → 忽略
        assert!(matches!(
            WorkBuddyAdapter::parse_line(r#"{"id":"m2","timestamp":1757000000000,"type":"message","role":"assistant"}"#),
            WbLine::None
        ));
        // function_call 无 usage → 忽略
        assert!(matches!(
            WorkBuddyAdapter::parse_line(r#"{"type":"function_call","timestamp":1757000000000,"providerData":{"model":"m"}}"#),
            WbLine::None
        ));
        assert!(matches!(WorkBuddyAdapter::parse_line("bad"), WbLine::None));
    }

    #[test]
    fn assistant_message_usage_counts() {
        // 纯文本最终回复（assistant message 带 rawUsage）入账
        let msg = r#"{"id":"m9","timestamp":1757000000000,"type":"message","role":"assistant","status":"completed","sessionId":"s1","providerData":{"messageId":"mid9","model":"glm-5.3","rawUsage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_cache_hit_tokens":0}}}"#;
        assert!(matches!(WorkBuddyAdapter::parse_line(msg), WbLine::Usage { .. }));
    }

    // ---------- 冻结样本行（本机真实结构,键集保持,正文脱敏） ----------

    const T: i64 = 1_788_602_400_000;
    fn row(ty: &str, dt: i64, extra: &str) -> String {
        format!(r#"{{"id":"i{dt}","parentId":"p","type":"{ty}","timestamp":{},"cwd":"E:\\Work\\Demo","sessionId":"s1"{extra}}}"#, T + dt)
    }
    fn pd(mid: &str, usage: Option<(i64, i64, i64)>) -> String {
        let u = usage
            .map(|(p, c, hit)| format!(r#","rawUsage":{{"prompt_tokens":{p},"completion_tokens":{c},"total_tokens":{},"prompt_cache_hit_tokens":{hit},"prompt_cache_miss_tokens":0,"credit":0.1}}"#, p + c))
            .unwrap_or_default();
        format!(r#","providerData":{{"messageId":"{mid}","model":"glm-5.3","requestModelId":"glm-5.3","requestModelName":"GLM-5.3","traceId":"t","conversationRequestId":"c","agent":"cli"{u}}}"#)
    }

    #[test]
    fn s2_turns_times_and_subagent_merge() {
        let main = [
            row("message", 0, r#","role":"user","content":[{"type":"input_text","text":"<redacted>"}]"#),
            row("reasoning", 2_000, &format!(r#","content":[]{}"#, pd("m1", None))),
            row("function_call", 3_000, &format!(r#","name":"read_file","callId":"c1","arguments":"{{}}"{}"#, pd("m1", Some((1000, 100, 400))))),
            row("function_call_result", 6_000, r#","name":"read_file","callId":"c1","status":"completed","output":"<redacted>""#),
            row("message", 9_000, &format!(r#","role":"assistant","status":"completed","content":[]{}"#, pd("m2", Some((500, 50, 0))))),
            r#"{"type":"ai-title","aiTitle":"<ai title>","cwd":"E:\\Work\\Demo","sessionId":"s1","timestamp":1788602410000}"#.to_string(),
            row("message", 60_000, r#","role":"user","content":[]"#),
            row("message", 62_000, &format!(r#","role":"assistant","status":"incomplete","content":[]{}"#, pd("m3", None))),
        ];
        let sub = [
            row("message", 4_000, r#","role":"user","content":[]"#).replace(r#""sessionId":"s1""#, r#""sessionId":"sub1""#),
            row("function_call", 5_000, &format!(r#","name":"grep","callId":"sc1","arguments":"{{}}"{}"#, pd("sm1", Some((100, 10, 0)))))
                .replace(r#""sessionId":"s1""#, r#""sessionId":"sub1""#),
        ];
        let dir = std::env::temp_dir().join(format!("tc_wb_s2_{}", std::process::id()));
        let proj = dir.join("projects").join("E--Work-Demo");
        let subdir = proj.join("s1").join("subagents");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(proj.join("s1.jsonl"), main.join("\n") + "\n").unwrap();
        std::fs::write(subdir.join("agent-x.jsonl"), sub.join("\n") + "\n").unwrap();
        let adapter = WorkBuddyAdapter { projects_dir: dir.join("projects") };
        let mut store = Store::open_in_memory().unwrap();
        let ok = adapter.collect(&mut store).is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok);

        let turns = store.test_turns(META.id);
        assert_eq!(turns.len(), 2);
        let t1 = &turns[0];
        assert_eq!((t1.model_calls, t1.tool_calls, t1.subagent_count, t1.subagent_calls), (3, 2, 1, 1), "调用含子代理");
        assert_eq!((t1.wall_ms, t1.tool_ms, t1.model_ms), (Some(9_000), Some(3_000), Some(1_000 + 3_000 + 1_000)));
        assert_eq!(t1.total_tokens, 1100 + 550 + 110);
        assert_eq!((t1.ttft_ms, t1.gap_ms), (None, None));
        let t2 = &turns[1];
        assert_eq!((t2.model_calls, t2.error_count, t2.aborted, t2.wall_ms, t2.gap_ms), (0, 1, false, Some(2_000), Some(51_000)), "incomplete 计错,不算中止");
        let sessions = store.test_sessions(META.id);
        let root = sessions.iter().find(|s| s.session_id == "s1").unwrap();
        assert_eq!((root.title.as_deref(), root.project_key.as_str()), (Some("<ai title>"), "e:/Work/Demo"));
        assert_eq!(sessions.iter().find(|s| s.session_id == "sub1").unwrap().parent_id.as_deref(), Some("s1"));
        assert_eq!(store.test_task_sessions(META.id), vec!["s1".to_string()]);
        let rows = store.month_rows("2026-09", "agent", "total", chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()).unwrap();
        assert_eq!(rows[0].message_counts.iter().sum::<i64>(), 1, "子代理提示行与无响应轮不计");
        assert!(store.test_project_conservation().is_empty(), "{:?}", store.test_project_conservation());
        // 积分:三条带 usage 行（含子代理）各 0.1 → 积分池合计 0.3;WorkBuddy 不进模型分布
        let credit = store.credit_summary("2026-09").unwrap();
        assert!((credit.total_credit - 0.3).abs() < 1e-9, "{}", credit.total_credit);
        assert!(credit.by_model.is_empty());
    }

    #[test]
    fn request_model_id_takes_priority() {
        let (_, _, model, _, _, _) =
            expect_usage(&line(1_757_000_000_000.0, Some("req-model"), "raw-model", 10, 5, 0, 15));
        assert_eq!(model, "req-model");
    }
}
