//! Codex 适配器：`~/.codex/sessions/**/*.jsonl` 递归 + `~/.codex/archived_sessions/*.jsonl`
//! （CODEX_HOME 可覆盖）。
//!
//! 格式（cli 0.145+）：
//! - 模型在 `turn_context` 行的 `payload.model`（turn 级设置，token_count 行不带）；
//! - 用量在 token_count 行 `payload.info`：`last_token_usage` = **单次调用值**（优先，
//!   无需差分）；`total_token_usage` = 会话内累积快照（备用）。
//! - 旧格式兼容：顶层 `info.total_token_usage` → 走差分 + 游标持久化基线
//!   （基线持久化,续读时不会归零重计）。
//!
//! 对话轮计数：`event_msg/task_started`（带 `turn_id`,每轮一条）置 pending 标志,
//! 下一条 token_count 行按其模型计 1 turn 并清位;pending 持久化进游标。
//! `token_count` 是 API 回合级（一轮工具循环多条）,不能当对话数。
//! - 不用 `event_msg/user_message` 作主信号：cli 0.145+ 历史 rollout 经 Codex 自行迁移重写后,
//!   该事件几乎消失,task_started 完整保留。user_message 仅在文件从未出现 task_started 时兼容置位,
//!   防同轮双计。
//! - **子代理会话不计轮**：`session_meta.parent_thread_id` 非空（guardian 审批 /
//!   thread_spawn 子代理）的 task_started 是代理派发的,不是用户输入——子代理会话里的
//!   task_started 占多数,计入会把轮次放大数倍。其 token 照常计入。
//!
//! 轮与时间：task_started 开轮、task_complete / turn_aborted 闭轮（wall 优先取其
//! `duration_ms`,缺失退时间戳差）;token_count = 模型调用（model_ms 按相邻事件估算）;
//! `function_call` / `custom_tool_call` / `tool_search_call` 的 `call_id` 与对应 `*_output`
//! 配对算 tool_ms;`turn_aborted`（reason 实际均为 interrupted）记**中止**（不计错）,
//! `task_complete.error` 非空计错。会话 = `session_meta.id`,
//! 子代理会话 parent = `parent_thread_id`（token / 调用并入父会话对应轮）;项目 =
//! `turn_context.cwd` 逐轮,`session_meta.cwd` 兜底。rollout 无标题;标题取线程库 `threads.name`（侧栏名）,
//! 无名退首条用户消息首行,每轮就地同步到已有会话行。
//!
//! 口径（样例：total=19914 = input 19139（含 cached 11008) + output 775（含
//! reasoning 397)）：input = raw.input - cached（cache-exclusive）；output 保持
//! provider 口径（含 reasoning）；total = raw.total_tokens（回退 input+output）。

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::store::{Batch, Store, Tokens};
use super::{
    Adapter, AdapterError, AdapterMeta, CollectOutcome, CollectResult, FileCursor, ProbeOutcome,
    advance_file, clamp0, load_cursor, rfc3339_to_local_day_hour, rfc3339_to_millis, seal_cursor,
};

pub struct CodexAdapter {
    sessions_dir: PathBuf,
    archived_dir: PathBuf,
    /// Codex 自己的线程库 `state_<N>.sqlite`（`threads.cwd` = 线程当前工作区);缺失 / 结构漂移 → 会话行退回首轮目录。
    state_db: Option<PathBuf>,
}

static META: AdapterMeta = AdapterMeta {
    id: "codex",
    name: "Codex",
    location: "~/.codex/sessions",
    kind: "jsonl",
};

#[derive(Debug, Clone, Copy, PartialEq)]
struct Snapshot {
    input: i64,
    output: i64,
    cached: i64,
    /// `cache_write_input_tokens`（2026-07 起出现,至今恒 0）。按 OpenAI 惯例视为 `input_tokens` 的子集
    /// （同 cached;`total_tokens = input + output` 不含它）,入库时从未命中输入里拆出。
    cache_write: i64,
    reason: i64,
    total: i64,
}

fn read_snapshot(usage: &Value) -> Snapshot {
    let get = |k: &str| clamp0(usage.get(k).and_then(|x| x.as_i64()).unwrap_or(0));
    Snapshot {
        input: get("input_tokens"),
        output: get("output_tokens"),
        // 两个字段名并存：新格式 cached_input_tokens / 旧 cache_read_input_tokens
        cached: get("cached_input_tokens").max(get("cache_read_input_tokens")),
        cache_write: get("cache_write_input_tokens"),
        reason: get("reasoning_output_tokens"),
        total: get("total_tokens"),
    }
}

enum Parsed {
    /// token_count 行：（本地日, 本地小时, 毫秒时间, 单次快照（若有), 累积快照（若有), 模型)。
    Usage { day: String, hour: u8, ts: i64, last: Option<Snapshot>, total: Option<Snapshot>, model: String },
    /// 无用量、无轮语义的行（模型已按行更新）。
    ModelOnly,
    /// turn_context：逐轮工作目录（模型已按行更新）。
    TurnContext { cwd: Option<String> },
    /// 轮开始事件（event_msg/task_started,主信号）。
    TaskStarted { ts: Option<i64> },
    /// 旧用户输入事件（event_msg/user_message,兼容）。
    UserMessage { ts: Option<i64> },
    /// 会话头（session_meta）:parent_thread_id 非空 = 子代理会话。
    SessionMeta { subagent: bool, id: Option<String>, parent: Option<String>, cwd: Option<String> },
    /// 工具调用起始（response_item function_call / custom_tool_call / tool_search_call）。
    ToolCall { ts: Option<i64>, id: String },
    /// 工具结果（对应 *_output）。
    ToolOutput { ts: Option<i64>, id: String },
    /// 轮结束（task_complete）:duration_ms 与是否带 error。
    TaskComplete { ts: Option<i64>, duration: Option<i64>, error: bool },
    /// 轮中止（turn_aborted）。
    TurnAborted { ts: Option<i64>, duration: Option<i64> },
}

/// `~/.codex/state_<N>.sqlite` 取 N 最大者（Codex 随 schema 版本换文件名）。
fn latest_state_db(base: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(base).ok()?;
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n: u32 = name.strip_prefix("state_")?.strip_suffix(".sqlite")?.parse().ok()?;
            Some((n, e.path()))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, p)| p)
}

/// Codex 线程库一行：会话当前工作区 + 标题。
struct ThreadRecord {
    cwd: Option<String>,
    title: Option<String>,
}

/// 线程标题：侧栏名优先;无名 → 首条用户消息的首个非空行（截 80 字符）。
fn thread_title(name: Option<String>, first_message: Option<String>) -> Option<String> {
    if let Some(n) = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) {
        return Some(n);
    }
    let first = first_message?;
    let line = first.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(line.chars().take(80).collect())
}

impl CodexAdapter {
    pub fn new() -> Self {
        let base = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| super::home_dir().map(|h| h.join(".codex")));
        let base = base.unwrap_or_default();
        CodexAdapter {
            sessions_dir: base.join("sessions"),
            archived_dir: base.join("archived_sessions"),
            state_db: latest_state_db(&base),
        }
    }

    /// 线程 id → Codex 线程记录（只读打开;库缺失 / 忙 / 列缺失 → 空表,会话行退回首轮目录、无标题）。
    fn thread_records(&self) -> HashMap<String, ThreadRecord> {
        let Some(db) = self.state_db.as_ref().filter(|p| p.is_file()) else { return HashMap::new() };
        let opened = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|c| c.busy_timeout(std::time::Duration::from_secs(2)).map(|_| c));
        let Ok(conn) = opened else { return HashMap::new() };
        // `name` = 侧栏线程名（应用生成或用户改名,较新 schema 才有）;`title` = 首条用户消息
        let has_name = conn
            .prepare("SELECT 1 FROM pragma_table_info('threads') WHERE name = 'name'")
            .and_then(|mut s| s.exists([]))
            .unwrap_or(false);
        let sql = if has_name { "SELECT id, cwd, title, name FROM threads" } else { "SELECT id, cwd, title, NULL FROM threads" };
        let Ok(mut stmt) = conn.prepare(sql) else {
            crate::dev_log!("[collector] codex threads table unreadable, session project falls back to first turn");
            return HashMap::new();
        };
        let rows = stmt.query_map([], |r| {
            let cwd: Option<String> = r.get(1)?;
            let first: Option<String> = r.get(2)?;
            let name: Option<String> = r.get(3)?;
            Ok((r.get::<_, String>(0)?, ThreadRecord { cwd: cwd.filter(|c| !c.is_empty()), title: thread_title(name, first) }))
        });
        rows.map(|it| it.flatten().collect()).unwrap_or_default()
    }

    /// 从一行提取模型更新与用量。info 路径双兼容：`payload.info`（新）/
    /// 顶层 `info`（旧格式）。
    fn parse_line(line: &str, file_model: &mut String) -> Option<Parsed> {
        let v = serde_json::from_str::<Value>(line).ok()?;
        let payload = v.get("payload");

        // 模型更新：turn_context 行的 payload.model（旧格式行内 info.model 亦兼容）
        let row_model = payload
            .and_then(|p| p.get("model"))
            .or_else(|| v.get("info").and_then(|i| i.get("model")))
            .or_else(|| payload.and_then(|p| p.get("info")).and_then(|i| i.get("model")))
            .or_else(|| v.get("info").and_then(|i| i.get("model_name")))
            .and_then(|m| m.as_str())
            .filter(|s| !s.is_empty());
        if let Some(m) = row_model {
            *file_model = m.to_string();
        }
        let model = || {
            if file_model.is_empty() { "unknown".to_string() } else { file_model.clone() }
        };

        // 会话头与轮信号（专用类型,优先于 usage 提取）
        let ts = v.get("timestamp").and_then(|t| t.as_str()).and_then(rfc3339_to_millis);
        let pstr = |k: &str| payload.and_then(|p| p.get(k)).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_string);
        match v.get("type").and_then(|t| t.as_str()) {
            Some("session_meta") => {
                let parent = pstr("parent_thread_id");
                return Some(Parsed::SessionMeta { subagent: parent.is_some(), id: pstr("id"), parent, cwd: pstr("cwd") });
            }
            Some("turn_context") => return Some(Parsed::TurnContext { cwd: pstr("cwd") }),
            _ => {}
        }
        let duration = || payload.and_then(|p| p.get("duration_ms")).and_then(|x| x.as_i64());
        match payload.and_then(|p| p.get("type")).and_then(|t| t.as_str()) {
            Some("task_started") => return Some(Parsed::TaskStarted { ts }),
            Some("user_message") => return Some(Parsed::UserMessage { ts }),
            Some("task_complete") => {
                let error = payload.and_then(|p| p.get("error")).map_or(false, |e| !e.is_null());
                return Some(Parsed::TaskComplete { ts, duration: duration(), error });
            }
            Some("turn_aborted") => return Some(Parsed::TurnAborted { ts, duration: duration() }),
            Some("function_call") | Some("custom_tool_call") | Some("tool_search_call") => {
                return Some(match pstr("call_id") {
                    Some(id) => Parsed::ToolCall { ts, id },
                    None => Parsed::ModelOnly,
                });
            }
            Some("function_call_output") | Some("custom_tool_call_output") | Some("tool_search_output") => {
                return Some(match pstr("call_id") {
                    Some(id) => Parsed::ToolOutput { ts, id },
                    None => Parsed::ModelOnly,
                });
            }
            _ => {}
        }

        // 用量提取：新 payload.info / 旧顶层 info；无 info 的行（turn_context 等）
        // 返回 ModelOnly（模型已按行更新）
        let info = match payload.and_then(|p| p.get("info")).or_else(|| v.get("info")) {
            Some(i) => i,
            None => return Some(Parsed::ModelOnly),
        };
        let last = info.get("last_token_usage").map(|u| read_snapshot(u));
        let total = info.get("total_token_usage").map(|u| read_snapshot(u));
        if last.is_none() && total.is_none() {
            return None; // turn_context 等无用量行（模型已更新）
        }
        let ts_str = v.get("timestamp")?.as_str()?;
        let (day, hour) = rfc3339_to_local_day_hour(ts_str)?;
        Some(Parsed::Usage { day, hour, ts: ts?, last, total, model: model() })
    }
}

impl Adapter for CodexAdapter {
    fn meta(&self) -> &'static AdapterMeta {
        &META
    }

    fn probe(&self) -> ProbeOutcome {
        if self.sessions_dir.is_dir() || self.archived_dir.is_dir() {
            ProbeOutcome { status: "ready".into(), fingerprint: None }
        } else {
            ProbeOutcome { status: "no_source".into(), fingerprint: None }
        }
    }

    fn collect(&self, store: &mut Store) -> CollectResult {
        if !self.sessions_dir.is_dir() && !self.archived_dir.is_dir() {
            return Err(AdapterError::new(
                "no_source",
                format!("missing {} and {}", self.sessions_dir.display(), self.archived_dir.display()),
            ));
        }
        let mut files = Vec::new();
        super::jsonl::discover(&self.sessions_dir, true, &mut files);
        super::jsonl::discover(&self.archived_dir, true, &mut files);
        super::jsonl::sort_by_mtime(&mut files);

        let mut batch = Batch::default();
        let mut months = BTreeSet::new();
        let threads = self.thread_records();

        for path in files {
            let scope = path.display().to_string();
            let mut cursor = load_cursor(store, META.id, &scope);
            let Some(consume) = advance_file(&path, &mut cursor) else { continue };
            if consume.reset {
                cursor = FileCursor::fresh(); // 重写：基线清零重读（已聚合不回滚）
            }

            cursor.turn.set_file_scope(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
            for line in &consume.lines {
                let Some(parsed) = Self::parse_line(line, &mut cursor.model) else { continue };
                let st = &mut cursor.turn;
                let (day, hour, ts, last, total, model) = match parsed {
                    Parsed::Usage { day, hour, ts, last, total, model } => (day, hour, ts, last, total, model),
                    Parsed::ModelOnly => continue,
                    Parsed::SessionMeta { subagent, id, parent, cwd } => {
                        cursor.subagent = subagent;
                        if let Some(id) = id {
                            st.set_session(&id);
                        }
                        if let Some(p) = parent {
                            st.set_parent(&p);
                        }
                        if let Some(c) = cwd {
                            st.set_project(&c);
                        }
                        continue;
                    }
                    Parsed::TurnContext { cwd } => {
                        if let Some(c) = cwd {
                            st.set_project(&c);
                        }
                        continue;
                    }
                    Parsed::TaskStarted { ts } => {
                        cursor.task_signal = true;
                        cursor.pending_turn = !cursor.subagent;
                        if let Some(ts) = ts {
                            st.begin(&mut batch, META.id, ts, !cursor.subagent);
                        }
                        continue;
                    }
                    Parsed::UserMessage { ts } => {
                        if !cursor.task_signal {
                            if !cursor.subagent {
                                cursor.pending_turn = true;
                            }
                            if let Some(ts) = ts {
                                st.begin(&mut batch, META.id, ts, !cursor.subagent);
                            }
                        }
                        continue;
                    }
                    Parsed::ToolCall { ts: Some(ts), id } => {
                        st.tool_start(&mut batch, META.id, ts, &id);
                        continue;
                    }
                    Parsed::ToolOutput { ts: Some(ts), id } => {
                        st.tool_end(ts, &id);
                        continue;
                    }
                    Parsed::TaskComplete { ts, duration, error } => {
                        if st.open.is_some() {
                            if let Some(ts) = ts {
                                if error {
                                    st.error(&mut batch, META.id, ts);
                                }
                                st.touch(ts);
                            }
                            if let Some(d) = duration {
                                st.set_explicit_wall(d);
                            }
                            st.close(&mut batch, META.id);
                        }
                        continue;
                    }
                    Parsed::TurnAborted { ts, duration } => {
                        if st.open.is_some() {
                            if let Some(ts) = ts {
                                st.abort(&mut batch, META.id, ts);
                            }
                            if let Some(d) = duration {
                                st.set_explicit_wall(d);
                            }
                            st.close(&mut batch, META.id);
                        }
                        continue;
                    }
                    Parsed::ToolCall { .. } | Parsed::ToolOutput { .. } => continue,
                };

                let (input, output, total_tokens, cached, cache_write) = if let Some(last) = last {
                    // 单次值路径（新格式）：无差分、无回退风险。
                    // input_tokens = 总输入 ⊇ 缓存读 + 缓存写;入库 input = 其余未命中部分
                    let cached = last.cached.min(last.input);
                    let cache_write = last.cache_write.min(last.input - cached);
                    let input = (last.input - cached - cache_write).max(0);
                    let output = last.output;
                    let t = if last.total > 0 { last.total } else { last.input + last.output };
                    // 基线同步到累积快照，保证文件内后续纯累积行差分仍正确
                    if let Some(tot) = total {
                        cursor.base_in = tot.input;
                        cursor.base_out = tot.output;
                        cursor.base_cached = tot.cached;
                        cursor.base_reason = tot.reason;
                        cursor.base_total = tot.total;
                    }
                    (input, output, t, cached, cache_write)
                } else {
                    // 差分路径（旧格式纯累积快照）：任一分量回退 → 丢弃整行且不更新基线
                    let tot = total.expect("last 与 total 至少存在其一");
                    let d_in = tot.input - cursor.base_in;
                    let d_out = tot.output - cursor.base_out;
                    let d_cached = tot.cached - cursor.base_cached;
                    let d_reason = tot.reason - cursor.base_reason;
                    let d_total = tot.total - cursor.base_total;
                    if d_in < 0 || d_out < 0 || d_cached < 0 || d_reason < 0 || d_total < 0 {
                        continue;
                    }
                    cursor.base_in = tot.input;
                    cursor.base_out = tot.output;
                    cursor.base_cached = tot.cached;
                    cursor.base_reason = tot.reason;
                    cursor.base_total = tot.total;
                    if d_in == 0 && d_out == 0 {
                        continue;
                    }
                    let cached = d_cached.min(d_in);
                    let input = d_in - cached;
                    let output = d_out;
                    let t = if d_total > 0 { d_total } else { d_in + d_out };
                    // 纯累积旧格式早于 cache_write_input_tokens 字段,不做差分
                    (input, output, t, cached, 0)
                };

                // cache_read = cached_input_tokens;cache_write = cache_write_input_tokens（原样存,目前恒 0）
                let tokens = Tokens { input, output, total: total_tokens, cache_read: cached, cache_write };
                let mark = cursor.pending_turn as i64;
                if cursor.turn.response(&mut batch, META.id, ts, Some(hour), &model, tokens, None, mark) == 1 {
                    cursor.pending_turn = false;
                }
                months.insert(day[..7].to_string());
            }
            if cursor.turn.session_id.is_empty() {
                // 无 session_meta 的异常文件:以文件名兜底会话 id
                if let Some(stem) = path.file_stem().and_then(|x| x.to_str()) {
                    cursor.turn.set_session(stem);
                }
            }
            // 会话行的项目以 Codex 自己的线程记录为准（用户在应用里切换工作区后 threads.cwd 跟着变;轮仍逐轮归属）
            if let Some(rec) = threads.get(&cursor.turn.session_id) {
                if let Some(cwd) = rec.cwd.as_deref() {
                    cursor.turn.set_session_project(cwd);
                }
                if let Some(t) = rec.title.as_deref() {
                    cursor.turn.set_title(t, 1);
                }
            }
            cursor.turn.flush(&mut batch, META.id);
            cursor.offset = consume.new_offset;
            seal_cursor(&mut cursor, &path, &scope, &mut batch);
        }

        store.commit(META.id, &batch).map_err(|e| AdapterError::new("error", e))?;
        // 标题只在线程库里、且会在 rollout 不增长时变化（应用稍后生成线程名 / 用户改名）:
        // 每轮就地同步到已有会话行,历史会话无需重读 rollout。
        let titles: Vec<(&str, &str)> =
            threads.iter().filter_map(|(id, r)| r.title.as_deref().map(|t| (id.as_str(), t))).collect();
        match store.sync_session_titles(META.id, &titles) {
            Ok(n) if n > 0 => crate::dev_log!("[collector] codex session titles synced: {n}"),
            Ok(_) => {}
            Err(e) => crate::dev_log!("[collector] codex title sync failed: {e}"),
        }
        Ok(CollectOutcome { events: batch.events, months })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 新格式 token_count 行（payload.info 嵌套，last + total 并存）。
    fn new_line(ts: &str, i: i64, o: i64, c: i64, r: i64) -> String {
        let usage = format!(
            r#"{{"input_tokens":{i},"output_tokens":{o},"cached_input_tokens":{c},"reasoning_output_tokens":{r},"total_tokens":{}}}"#,
            i + o
        );
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{usage},"last_token_usage":{usage}}}}}}}"#
        )
    }

    fn turn_context_line(model: &str) -> String {
        format!(r#"{{"timestamp":"2026-09-05T10:00:00Z","type":"turn_context","payload":{{"turn_id":"t1","model":"{model}"}}}}"#)
    }

    /// 旧格式（顶层 info，纯累积快照）。
    fn old_line(ts: &str, i: i64, o: i64, c: i64, r: i64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","info":{{"model":"gpt-5","total_token_usage":{{"input_tokens":{i},"output_tokens":{o},"cached_input_tokens":{c},"reasoning_output_tokens":{r}}}}}}}"#
        )
    }

    #[test]
    fn new_format_last_usage_single_shot() {
        let mut fm = String::new();
        assert!(matches!(CodexAdapter::parse_line(&turn_context_line("gpt-5.6-sol"), &mut fm), Some(Parsed::TurnContext { .. })));
        assert_eq!(fm, "gpt-5.6-sol");

        // 首行快照：in=19139 cached=11008 out=775 reason=397 total=19914
        let Some(Parsed::Usage { day, hour, last, total, model, .. }) =
            CodexAdapter::parse_line(&new_line("2026-09-05T10:01:00Z", 19139, 775, 11008, 397), &mut fm)
        else { panic!() };
        assert!(hour <= 23);
        let last = last.unwrap();
        assert_eq!(last.input - last.cached.min(last.input), 8131); // cache-exclusive input
        assert_eq!(last.output, 775); // provider 口径,含 reasoning
        assert_eq!(last.total, 19914); // = 19139 + 775
        assert_eq!(model, "gpt-5.6-sol"); // 沿用 turn_context 的文件级模型
        assert!(total.is_some());
        assert!(day.starts_with("2026-09"));
    }

    #[test]
    fn old_format_differential() {
        let mut fm = String::new();
        let (_, s1) = match CodexAdapter::parse_line(&old_line("2026-09-05T10:00:00Z", 100, 50, 30, 20), &mut fm).unwrap() {
            Parsed::Usage { last, total, .. } => (last, total.unwrap()),
            _ => panic!(),
        };
        assert!(fm == "gpt-5");
        let (_, s2) = match CodexAdapter::parse_line(&old_line("2026-09-05T10:01:00Z", 260, 80, 60, 30), &mut fm).unwrap() {
            Parsed::Usage { last, total, .. } => (last, total.unwrap()),
            _ => panic!(),
        };
        // 差分：Δin=160 Δout=30 Δcached=30 → input=130 cached=30 output=30
        // total 口径 = Δin + Δout = 190（reasoning 含在 output 内不扣减）
        assert_eq!((s2.input - s1.input) - (s2.cached - s1.cached).min(s2.input - s1.input), 130);
        assert_eq!(s2.output - s1.output, 30);
        assert_eq!((s2.input + s2.output) - (s1.input + s1.output), 190);
    }

    #[test]
    fn turn_signal_events_parsed() {
        let mut fm = String::new();
        assert!(matches!(
            CodexAdapter::parse_line(
                r#"{"timestamp":"2026-09-05T10:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1","started_at":1788602400,"model_context_window":258400,"collaboration_mode_kind":"default"}}"#,
                &mut fm
            ),
            Some(Parsed::TaskStarted { ts: Some(_) })
        ));
        assert!(matches!(
            CodexAdapter::parse_line(
                r#"{"timestamp":"2026-09-05T10:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#,
                &mut fm
            ),
            Some(Parsed::UserMessage { .. })
        ));
        // 主会话头 / 子代理会话头（guardian:parent_thread_id 非空）
        assert!(matches!(
            CodexAdapter::parse_line(
                r#"{"timestamp":"2026-09-05T10:00:00Z","type":"session_meta","payload":{"id":"a","session_id":"a","source":"vscode","thread_source":"user"}}"#,
                &mut fm
            ),
            Some(Parsed::SessionMeta { subagent: false, .. })
        ));
        assert!(matches!(
            CodexAdapter::parse_line(
                r#"{"timestamp":"2026-09-05T10:00:00Z","type":"session_meta","payload":{"id":"b","session_id":"a","parent_thread_id":"a","source":{"subagent":{"other":"guardian"}},"thread_source":"guardian_review"}}"#,
                &mut fm
            ),
            Some(Parsed::SessionMeta { subagent: true, .. })
        ));
        // response_item/message role=user 是环境上下文注入,不算轮信号
        assert!(matches!(
            CodexAdapter::parse_line(
                r#"{"timestamp":"2026-09-05T10:00:00Z","type":"response_item","payload":{"type":"message","role":"developer","content":[]}}"#,
                &mut fm
            ),
            Some(Parsed::ModelOnly) | None
        ));
    }

    /// 端到端（临时目录走真实 collect）:返回 (Σtokens, Σturns)。
    fn run_lines(name: &str, lines: &[String]) -> (i64, i64) {
        let dir = std::env::temp_dir().join(format!("tc_codex_turns_{}_{}", name, std::process::id()));
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("rollout.jsonl"), lines.join("\n") + "\n").unwrap();
        let adapter = CodexAdapter { sessions_dir: sessions, archived_dir: dir.join("archived"), state_db: None };
        let mut store = Store::open_in_memory().unwrap();
        let collected = adapter.collect(&mut store).is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(collected);
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap();
        let rows = store.month_rows("2026-09", "agent", "total", today).unwrap();
        let tokens: i64 = rows.iter().map(|r| r.month_total).sum();
        let turns: i64 = rows.iter().map(|r| r.message_counts.iter().sum::<i64>()).sum();
        (tokens, turns)
    }

    fn task_started() -> String {
        r#"{"timestamp":"2026-09-05T10:00:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t"}}"#.to_string()
    }
    fn user_message() -> String {
        r#"{"timestamp":"2026-09-05T10:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"x"}}"#.to_string()
    }
    fn meta(parent: Option<&str>) -> String {
        match parent {
            Some(p) => format!(r#"{{"timestamp":"2026-09-05T09:59:00Z","type":"session_meta","payload":{{"id":"c","parent_thread_id":"{p}"}}}}"#),
            None => r#"{"timestamp":"2026-09-05T09:59:00Z","type":"session_meta","payload":{"id":"m"}}"#.to_string(),
        }
    }
    fn tc(i: i64) -> String {
        new_line("2026-09-05T10:01:00Z", i, 1, 0, 0)
    }

    #[test]
    fn task_started_counts_main_session_turns() {
        // 主会话两轮,每轮两次模型回合;第一轮 task_started 后紧跟 user_message（真实顺序）
        let lines = vec![
            meta(None), turn_context_line("gpt-5.6"),
            task_started(), user_message(), tc(10), tc(10),
            task_started(), tc(10), tc(10),
        ];
        assert_eq!(run_lines("main", &lines), (4 * 11, 2), "task_started 计轮,同轮 user_message 不双计");
    }

    #[test]
    fn subagent_session_tokens_without_turns() {
        let lines = vec![meta(Some("m")), turn_context_line("gpt-5.6"), task_started(), tc(10), task_started(), tc(10)];
        assert_eq!(run_lines("sub", &lines), (22, 0), "子代理会话 token 计入、轮不计");
    }

    #[test]
    fn legacy_user_message_only_file_still_counts() {
        let lines = vec![turn_context_line("gpt-5"), user_message(), tc(10), tc(10), user_message(), tc(10)];
        assert_eq!(run_lines("legacy", &lines), (33, 2), "无 task_started 的旧文件走 user_message 兼容");
    }

    // ---------- 冻结样本行（本机 rollout 真实结构,键集保持,正文脱敏） ----------

    fn ev(ts: &str, ty: &str, payload: &str) -> String {
        format!(r#"{{"timestamp":"{ts}","type":"{ty}","payload":{payload}}}"#)
    }

    /// 主会话 + guardian 子代理会话走真实 collect:三层计数 / 四段时间 / 中止轮 / 子会话并入 / 守恒。
    #[test]
    fn s2_turns_times_and_guardian_merge() {
        let main = vec![
            ev("2026-09-05T09:59:00.000Z", "session_meta", r#"{"id":"m1","session_id":"m1","timestamp":"2026-09-05T09:59:00Z","cwd":"E:\\Work\\Demo","originator":"codex_vscode","cli_version":"0.154.0","source":"vscode","thread_source":"user","model_provider":"openai"}"#),
            ev("2026-09-05T10:00:00.000Z", "event_msg", r#"{"type":"thread_settings_applied"}"#),
            ev("2026-09-05T10:00:00.100Z", "event_msg", r#"{"type":"task_started","turn_id":"t1","started_at":1788602400,"model_context_window":258400,"collaboration_mode_kind":"default"}"#),
            ev("2026-09-05T10:00:00.200Z", "turn_context", r#"{"turn_id":"t1","cwd":"E:\\Work\\Demo","model":"gpt-5.6-sol","effort":"high","approval_policy":"on-request"}"#),
            new_line("2026-09-05T10:00:05.100Z", 100, 10, 40, 3),
            ev("2026-09-05T10:00:05.100Z", "response_item", r#"{"type":"function_call","name":"shell","arguments":"{}","call_id":"call_1"}"#),
            ev("2026-09-05T10:00:09.100Z", "response_item", r#"{"type":"function_call_output","call_id":"call_1","output":"<redacted>"}"#),
            new_line("2026-09-05T10:00:12.100Z", 200, 20, 0, 0),
            ev("2026-09-05T10:00:13.100Z", "event_msg", r#"{"type":"task_complete","turn_id":"t1","started_at":1788602400,"completed_at":1788602413,"duration_ms":13000,"time_to_first_token_ms":4100,"error":null,"last_agent_message":"<redacted>"}"#),
            ev("2026-09-05T10:10:13.100Z", "event_msg", r#"{"type":"task_started","turn_id":"t2","started_at":1788603013,"model_context_window":258400,"collaboration_mode_kind":"default"}"#),
            ev("2026-09-05T10:10:13.200Z", "turn_context", r#"{"turn_id":"t2","cwd":"E:\\Work\\Demo","model":"gpt-5.6-sol"}"#),
            ev("2026-09-05T10:10:20.100Z", "event_msg", r#"{"type":"turn_aborted","turn_id":"t2","reason":"interrupted","started_at":1788603013,"completed_at":1788603020,"duration_ms":7000}"#),
        ];
        let guardian = vec![
            ev("2026-09-05T10:00:06.000Z", "session_meta", r#"{"id":"g1","session_id":"m1","parent_thread_id":"m1","cwd":"E:\\Work\\Demo","source":{"subagent":{"other":"guardian"}},"thread_source":"guardian_review","agent_nickname":"guardian"}"#),
            ev("2026-09-05T10:00:06.100Z", "event_msg", r#"{"type":"task_started","turn_id":"gt1","collaboration_mode_kind":"default"}"#),
            ev("2026-09-05T10:00:06.200Z", "turn_context", r#"{"turn_id":"gt1","cwd":"E:\\Work\\Demo","model":"gpt-5.6-terra"}"#),
            new_line("2026-09-05T10:00:08.100Z", 50, 5, 0, 0),
            ev("2026-09-05T10:00:08.600Z", "event_msg", r#"{"type":"task_complete","turn_id":"gt1","duration_ms":2500,"error":null}"#),
        ];
        let dir = std::env::temp_dir().join(format!("tc_codex_s2_{}", std::process::id()));
        let day_dir = dir.join("sessions").join("2026").join("09").join("05");
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("rollout-main.jsonl"), main.join("\n") + "\n").unwrap();
        std::fs::write(day_dir.join("rollout-guardian.jsonl"), guardian.join("\n") + "\n").unwrap();
        let adapter = CodexAdapter { sessions_dir: dir.join("sessions"), archived_dir: dir.join("archived"), state_db: None };
        let mut store = Store::open_in_memory().unwrap();
        let ok = adapter.collect(&mut store).is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok);

        let turns = store.test_turns(META.id);
        assert_eq!(turns.len(), 2, "子代理会话不单独出轮");
        let t1 = &turns[0];
        assert_eq!((t1.model_calls, t1.tool_calls, t1.subagent_count, t1.subagent_calls), (3, 1, 1, 1));
        assert_eq!(t1.wall_ms, Some(13_000), "wall 取 task_complete.duration_ms");
        assert_eq!(t1.tool_ms, Some(4_000));
        assert_eq!(t1.model_ms, Some(5_000 + 3_000 + 2_000), "父 5s+3s 估算 + 子代理 2s 并入");
        assert_eq!((t1.ttft_ms, t1.gap_ms, t1.error_count), (None, None, 0), "ttft 不取 Codex 值");
        assert_eq!(t1.total_tokens, 110 + 220 + 55);
        assert_eq!((t1.model_key.as_str(), t1.project_key.as_str()), ("gpt-5.6-sol", "e:/Work/Demo"));
        let t2 = &turns[1];
        assert_eq!((t2.model_calls, t2.error_count, t2.aborted, t2.wall_ms), (0, 0, true, Some(7_000)), "turn_aborted = 中止,不计错");
        assert!(!t1.aborted);
        assert_eq!(t2.gap_ms, Some(600_000));

        let sessions = store.test_sessions(META.id);
        let g = sessions.iter().find(|s| s.session_id == "g1").unwrap();
        assert_eq!(g.parent_id.as_deref(), Some("m1"));
        assert_eq!(store.test_task_sessions(META.id), vec!["m1".to_string()]);
        assert_eq!(store.test_child_sessions_in_tasks(META.id), 0);
        let rows = store.month_rows("2026-09", "agent", "total", chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()).unwrap();
        assert_eq!(rows[0].message_counts.iter().sum::<i64>(), 1, "中止轮不计 request_count");
        assert!(store.test_project_conservation().is_empty(), "{:?}", store.test_project_conservation());
    }

    /// `cache_write_input_tokens` 原样入 cache_write,且视为 input_tokens 的子集:
    /// 四分项守恒,未命中输入（uncached）= input_tokens − cached_input_tokens 不变。
    #[test]
    fn cache_write_field_is_stored_as_subset_of_input() {
        let usage = r#"{"input_tokens":1000,"cached_input_tokens":700,"cache_write_input_tokens":200,"output_tokens":50,"reasoning_output_tokens":10,"total_tokens":1050}"#;
        let lines = vec![
            ev("2026-09-05T09:59:00.000Z", "session_meta", r#"{"id":"cw","session_id":"cw","cwd":"E:\\Work\\Demo","source":"vscode"}"#),
            turn_context_line("gpt-5.6"),
            ev("2026-09-05T10:00:00.100Z", "event_msg", r#"{"type":"task_started","turn_id":"t1"}"#),
            format!(r#"{{"timestamp":"2026-09-05T10:00:05.000Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{usage},"last_token_usage":{usage}}}}}}}"#),
        ];
        let dir = std::env::temp_dir().join(format!("tc_codex_cw_{}", std::process::id()));
        let day_dir = dir.join("sessions").join("2026").join("09").join("05");
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("rollout-cw.jsonl"), lines.join("\n") + "\n").unwrap();
        let adapter = CodexAdapter { sessions_dir: dir.join("sessions"), archived_dir: dir.join("archived"), state_db: None };
        let mut store = Store::open_in_memory().unwrap();
        let ok = adapter.collect(&mut store).is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok);
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap();
        let got = |metric: &str| store.month_rows("2026-09", "agent", metric, today).unwrap()[0].month_total;
        assert_eq!(
            (got("input"), got("cache_write"), got("cache_read"), got("output"), got("uncached"), got("total")),
            (100, 200, 700, 50, 300, 1050)
        );
    }

    /// 会话行的项目 = Codex `threads.cwd`（用户在应用里把线程切到别的工作区）;轮仍按各自 turn_context 归属;
    /// 线程库缺该线程 → 退回首轮目录。
    #[test]
    fn session_project_follows_codex_thread_record() {
        let rollout = |id: &str| {
            vec![
                ev("2026-09-05T09:59:00.000Z", "session_meta", &format!(r#"{{"id":"{id}","session_id":"{id}","cwd":"E:\\Work\\Chat\\bang","source":"vscode"}}"#)),
                ev("2026-09-05T10:00:00.100Z", "event_msg", r#"{"type":"task_started","turn_id":"t1"}"#),
                ev("2026-09-05T10:00:00.200Z", "turn_context", r#"{"turn_id":"t1","cwd":"E:\\Work\\Chat\\bang","model":"gpt-5.6-sol"}"#),
                new_line("2026-09-05T10:00:05.100Z", 100, 10, 0, 0),
                ev("2026-09-05T10:00:06.000Z", "event_msg", r#"{"type":"task_complete","turn_id":"t1","duration_ms":6000,"error":null}"#),
                ev("2026-09-05T10:30:00.100Z", "event_msg", r#"{"type":"task_started","turn_id":"t2"}"#),
                ev("2026-09-05T10:30:00.200Z", "turn_context", r#"{"turn_id":"t2","cwd":"E:\\Work\\Spring","model":"gpt-5.6-sol"}"#),
                new_line("2026-09-05T10:30:05.100Z", 200, 20, 0, 0),
                ev("2026-09-05T10:30:06.000Z", "event_msg", r#"{"type":"task_complete","turn_id":"t2","duration_ms":6000,"error":null}"#),
            ]
        };
        let dir = std::env::temp_dir().join(format!("tc_codex_thread_cwd_{}", std::process::id()));
        let day_dir = dir.join("sessions").join("2026").join("09").join("05");
        std::fs::create_dir_all(&day_dir).unwrap();
        std::fs::write(day_dir.join("rollout-a.jsonl"), rollout("th_a").join("\n") + "\n").unwrap();
        std::fs::write(day_dir.join("rollout-b.jsonl"), rollout("th_b").join("\n") + "\n").unwrap();
        let state = dir.join("state_5.sqlite");
        {
            let c = rusqlite::Connection::open(&state).unwrap();
            c.execute_batch(r"CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, title TEXT);
                              INSERT INTO threads (id, cwd, title) VALUES ('th_a', 'E:\Work\Spring', char(10) || '  修复时间线' || char(13, 10) || '细节');").unwrap();
        }
        assert_eq!(latest_state_db(&dir).as_deref(), Some(state.as_path()));
        let adapter = CodexAdapter { sessions_dir: dir.join("sessions"), archived_dir: dir.join("archived"), state_db: Some(state) };
        let mut store = Store::open_in_memory().unwrap();
        let ok = adapter.collect(&mut store).is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok);
        let sessions = store.test_sessions(META.id);
        let get = |id: &str| sessions.iter().find(|s| s.session_id == id).unwrap().project_key.clone();
        assert_eq!(get("th_a"), "e:/Work/Spring", "会话行 = threads.cwd");
        assert_eq!(get("th_b"), "e:/Work/Chat/bang", "线程库无记录 → 首轮目录");
        let keys: Vec<String> = store.test_turns(META.id).iter().filter(|t| t.session_id == "th_a").map(|t| t.project_key.clone()).collect();
        assert_eq!(keys, vec!["e:/Work/Chat/bang".to_string(), "e:/Work/Spring".to_string()], "轮仍逐轮归属");
        let title = |id: &str| sessions.iter().find(|s| s.session_id == id).unwrap().title.clone();
        assert_eq!(title("th_a").as_deref(), Some("修复时间线"), "旧 schema 无 name 列 → 首条消息首行");
        assert_eq!(title("th_b"), None);
    }

    /// 线程标题：侧栏名优先;空名退首条消息首个非空行,截 80 字符。
    #[test]
    fn thread_title_prefers_sidebar_name() {
        assert_eq!(thread_title(Some(" 排查连接 ".into()), Some("原话".into())).as_deref(), Some("排查连接"));
        assert_eq!(thread_title(Some("".into()), Some("\r\n继续下一步\n细节".into())).as_deref(), Some("继续下一步"));
        assert_eq!(thread_title(None, Some("x".repeat(200))).map(|t| t.chars().count()), Some(80));
        assert_eq!(thread_title(None, Some("  \n ".into())), None);
    }

    /// 非 UTF-8 路径:同一中文目录分别以 UTF-8 与 GBK 字节写进 session_meta / turn_context 的 cwd,
    /// 走真实 collect 后 project_key 一致（GBK 样本按系统 ANSI 代码页回退;本机 CP936 时断言中文原文）。
    #[test]
    fn chinese_cwd_utf8_and_gbk_bytes() {
        let dir = std::env::temp_dir().join(format!("tc_codex_cwd_{}", std::process::id()));
        let day_dir = dir.join("sessions").join("2026").join("09").join("05");
        std::fs::create_dir_all(&day_dir).unwrap();
        let rollout = |id: &str, cwd_json: &[u8]| -> Vec<u8> {
            let mut out = Vec::new();
            let mut line = |parts: &[&[u8]]| {
                for p in parts {
                    out.extend_from_slice(p);
                }
                out.push(b'\n');
            };
            line(&[br#"{"timestamp":"2026-09-05T09:59:00.000Z","type":"session_meta","payload":{"id":""#, id.as_bytes(), br#"","cwd":""#, cwd_json, br#""}}"#]);
            line(&[br#"{"timestamp":"2026-09-05T10:00:00.100Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#]);
            line(&[br#"{"timestamp":"2026-09-05T10:00:00.200Z","type":"turn_context","payload":{"turn_id":"t1","cwd":""#, cwd_json, br#"","model":"gpt-5.6-sol"}}"#]);
            line(&[new_line("2026-09-05T10:00:05.100Z", 100, 10, 0, 0).as_bytes()]);
            out
        };
        // 冻结样本 1:UTF-8「文档」
        std::fs::write(day_dir.join("rollout-utf8.jsonl"), rollout("u1", r"D:\\OneDrive\\文档\\knowledge".as_bytes())).unwrap();
        // 冻结样本 2:GBK 字节「文档」= CE C4 B5 B5
        let mut gbk = br"D:\\OneDrive\\".to_vec();
        gbk.extend_from_slice(&[0xCE, 0xC4, 0xB5, 0xB5]);
        gbk.extend_from_slice(br"\\knowledge");
        std::fs::write(day_dir.join("rollout-gbk.jsonl"), rollout("g1", &gbk)).unwrap();

        let adapter = CodexAdapter { sessions_dir: dir.join("sessions"), archived_dir: dir.join("archived"), state_db: None };
        let mut store = Store::open_in_memory().unwrap();
        let ok = adapter.collect(&mut store).is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok);
        let sessions = store.test_sessions(META.id);
        let key = |id: &str| sessions.iter().find(|s| s.session_id == id).map(|s| s.project_key.clone()).unwrap();
        assert_eq!(key("u1"), "d:/OneDrive/文档/knowledge");
        assert!(!key("g1").contains('\u{FFFD}'), "GBK 字节不得落成替换符");
        #[cfg(windows)]
        if unsafe { windows_sys::Win32::Globalization::GetACP() } == 936 {
            assert_eq!(key("g1"), key("u1"), "CP936 系统上两种编码归一到同一项目");
        }
        assert!(store.test_project_conservation().is_empty());
    }

    #[test]
    fn row_without_usage_skipped() {
        let mut fm = String::new();
        // 无 info 且无轮语义的行 → ModelOnly(无用量产出)
        assert!(matches!(
            CodexAdapter::parse_line(r#"{"timestamp":"2026-09-05T10:00:00Z","type":"event_msg","payload":{"type":"item_completed"}}"#, &mut fm),
            Some(Parsed::ModelOnly)
        ));
        assert!(matches!(
            CodexAdapter::parse_line(r#"{"timestamp":"2026-09-05T10:00:00Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t","duration_ms":1200,"error":null}}"#, &mut fm),
            Some(Parsed::TaskComplete { duration: Some(1200), error: false, .. })
        ));
        assert!(CodexAdapter::parse_line("bad", &mut fm).is_none());
    }

    #[test]
    fn model_fallback_to_unknown() {
        let mut fm = String::new();
        let Some(Parsed::Usage { model, .. }) =
            CodexAdapter::parse_line(&new_line("2026-09-05T10:01:00Z", 10, 5, 0, 0), &mut fm)
        else { panic!() };
        assert_eq!(model, "unknown");
    }

    #[test]
    fn clamps_negatives() {
        let s = read_snapshot(&serde_json::json!({
            "input_tokens": -5,
            "output_tokens": 10,
            "cached_input_tokens": -3,
            "cache_read_input_tokens": 8,
            "reasoning_output_tokens": -1
        }));
        assert_eq!(s.input, 0);
        assert_eq!(s.output, 10);
        assert_eq!(s.cached, 8); // max(-3, 8)
        assert_eq!(s.reason, 0);
    }
}
