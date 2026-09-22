//! Claude Code 适配器：`~/.claude/projects/**/*.jsonl` 递归（CLAUDE_CONFIG_DIR 可覆盖）。
//!
//! 口径：仅 `type=="assistant"` 且 `message.usage` 存在的行；
//! input/output 取 `message.usage.input_tokens / output_tokens` 原始值；
//! **total = input + output + cache_read + cache_write**。
//! Anthropic 的 usage 没有 total 字段,而 `input_tokens` 是 **cache-exclusive** 的——开了
//! prompt caching 之后真实输入几乎全落在 `cache_read_input_tokens` /
//! `cache_creation_input_tokens` 里,`input_tokens` 只剩个位数（典型一行:
//! in=2 / cache_write=2623 / cache_read=99404 / out=459）。只取 input + output 会退化成
//! 「约等于 output」,把用量低估两个数量级。四项和即自洽关系 `total = input_excl + cache + output`。
//! 时间取顶层 `timestamp`（RFC3339）→ 本地日。模型缺失填 "unknown"。
//!
//! **token 去重**：一次 API 响应被流式拆成多条 assistant 行（thinking / text / tool_use
//! 各一行）,每行都带同一份 `message.usage`（绝大多数完全相同,少数 output 递增）。按 `message.id`
//! 只入账增量,逐行相加会让 token 约重计 1 倍。
//!
//! 对话轮计数：「真实用户输入行」置 pending 标志——type=="user" 且无 toolUseResult、
//! 无 tool_result 块、非 sidechain / meta / compact summary,且 `origin.kind` 缺省或为
//! `human`（`task-notification` 等是系统注入,不是用户发起）;下一条 assistant usage 行
//! 按其模型计 1 turn 并清位,pending 持久化进游标。
//!
//! 零调用输入：本文件出现过 `origin` 字段的前提下,缺 `origin` 的用户行（本地斜杠命令
//! `<command-name>`、命令输出 `<local-command-stdout>`、`[Request interrupted…]` 标记都不带 origin,
//! 真实输入全带 `origin.kind=human`）记为**待定输入**:拿到响应照常成轮计 request_count,
//! 零调用则不成轮（不写 turn_raw、不计中止错误）。全文件无 origin 的旧 CLI 文件维持原判
//! （缺 origin 即真实输入,零调用按中止轮）。
//!
//! 轮与时间：真实输入开轮;assistant 行 = 模型调用（按 message.id 去重）,
//! `tool_use` 块 id → `tool_result.tool_use_id` 配对算 tool_ms;`isApiErrorMessage` 计错,
//! `system/api_error` 计重试;系统注入输入之后的事件不延长 wall。
//! 会话：主文件 = `sessionId`（文件内首个）;`<session>/subagents/agent-*.jsonl` 为子会话
//! （行带 `isSidechain` + `agentId`）,session_id = agentId、parent = sessionId,每条提示行开子轮。
//! 标题（内容列）：`custom-title.customTitle` 优先于 `ai-title.aiTitle`。
//! 项目：= 文件所在文件夹 `projects/<编码启动目录>/`（源自己的分组,`project_dir`）;行内 `cwd` 只用来
//! 还原可读路径——它是 Bash 当前目录,随 `cd` 漂进子目录,不能当身份。
//!
//! **会话族折叠**：
//! 桌面应用「续聊 / fork」把整份历史复制进新会话文件——复制行的 `uuid` / `message.id` / 时间戳与原文件
//! 逐行一致;新版改写 `sessionId`（及用户行 `promptId`）,旧版（2.1.260）连 `sessionId` 都沿用根会话,
//! 所以判族只看行 uuid;根文件在 fork 之后不再追加带时间戳的行。
//! 口径：文件内**首个带 uuid 的主会话行**若已被计过 → 整个文件是该根会话的续篇:
//! session_id 归根（`TurnState:fold_into`）、文件名主干作副本 id 记入 `session_alias`（子会话 parent 归根）、
//! 只计未见过的 uuid（复制的历史行跳过:不开轮、不入账、不配对）、标题按最新文件覆盖。已计行持久化在
//! `seen_line`（派生数据）。文件按 （首个 uuid 行时间, 尾部时间) 升序处理,根文件先占 uuid
//! （mtime 不可靠:根文件事后会被追加无时间戳的元数据行）。`/compact` 在同一文件内追加,不受影响。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use serde_json::Value;

use super::project_dir::{first_cwd, folder_of, FolderProjects};
use super::store::{Batch, Store, Tokens};
use super::turns::normalize_project;
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
    let (cache_read, cache_write) = (get("cache_read_input_tokens"), get("cache_creation_input_tokens"));
    // total 四项和:Anthropic 无 provider total,且 input 不含 cache（见文件头口径）。
    let tokens = Tokens { input, output, total: input + output + cache_read + cache_write, cache_read, cache_write };
    Some((day, hour, model, tokens))
}

/// 一次 collect 内的会话族缓存：根文件与副本文件常在同一批处理,批内新记的行尚未落库,
/// 库内查询只在「首个 uuid 行判定」与「续篇文件装根会话已计行」两处发生（每文件一次,不逐行查库）。
#[derive(Default)]
struct FamilyCache {
    /// 本批新记的行 uuid → 归属会话。
    new_seen: HashMap<String, String>,
    /// 根会话 → 库内已计行（续篇文件按需装入）。
    root_lines: HashMap<String, HashSet<String>>,
    /// 副本 sessionId → 根会话（本批新记 + 库内查过的;None = 无别名）。
    alias: HashMap<String, Option<String>>,
}

impl FamilyCache {
    fn seen_session(&self, store: &Store, uuid: &str) -> Option<String> {
        self.new_seen.get(uuid).cloned().or_else(|| store.seen_line_session(META.id, uuid))
    }

    fn is_seen(&mut self, store: &Store, root: &str, uuid: &str) -> bool {
        if self.new_seen.contains_key(uuid) {
            return true;
        }
        self.root_lines
            .entry(root.to_string())
            .or_insert_with(|| store.seen_lines_of(META.id, root))
            .contains(uuid)
    }

    fn root_of(&mut self, store: &Store, sid: &str) -> Option<String> {
        self.alias.entry(sid.to_string()).or_insert_with(|| store.session_root_alias(META.id, sid)).clone()
    }
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
    /// 首个带 uuid 的主会话行判定会话族;续篇文件跳过已计过的行,其余带 uuid 的行记为已计。
    /// `file_id` = 文件名主干（副本文件自己的会话 id;旧版副本连行内 sessionId 都沿用根会话,不能靠行判别名）。
    fn process_line(
        line: &str,
        file_id: &str,
        cursor: &mut FileCursor,
        batch: &mut Batch,
        months: &mut BTreeSet<String>,
        store: &Store,
        fam: &mut FamilyCache,
    ) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        let agent = META.id;
        let sidechain = v.get("isSidechain").and_then(|x| x.as_bool()).unwrap_or(false);
        let line_sid = v.get("sessionId").and_then(|x| x.as_str());
        let uuid = v.get("uuid").and_then(|x| x.as_str());
        let ts = v.get("timestamp").and_then(|t| t.as_str()).and_then(rfc3339_to_millis);
        if let Some(t) = ts {
            cursor.last_ts = cursor.last_ts.max(t);
        }
        // 会话族判定:首个带 uuid 的行。主会话行已被计过（不论行内 sessionId 是否改写:旧版副本沿用根会话的
        // sessionId,新版改写成新 id）→ 本文件是该根会话的续篇;截断重读的文件由 collect
        // 预先置已判定（自身重读不是副本）。子会话文件（全是 sidechain 行,不会被复制）只置已判定。
        if let (Some(u), false) = (uuid, cursor.family_resolved) {
            cursor.family_resolved = true;
            let root = if sidechain { None } else { fam.seen_session(store, u) };
            if let Some(root) = root {
                cursor.turn.fold_into(&root);
                if file_id != root {
                    batch.session_aliases.push((agent.to_string(), file_id.to_string(), root.clone()));
                    fam.alias.insert(file_id.to_string(), Some(root.clone()));
                    crate::dev_log!("[collector] claude-code fold {} -> {}", file_id, root);
                }
                cursor.family_root = Some(root);
            }
        }
        let st = &mut cursor.turn;
        if let Some(sid) = line_sid {
            match v.get("agentId").and_then(|x| x.as_str()).filter(|_| sidechain) {
                Some(agent_id) => {
                    st.set_session(agent_id);
                    // 副本文件下的子会话:parent 按别名归根
                    let parent = fam.root_of(store, sid).unwrap_or_else(|| sid.to_string());
                    st.set_parent(&parent);
                }
                None => st.set_session(sid),
            }
        }
        // 行内 cwd 不参与项目归属（它是 Bash 当前目录,`cd` 后漂进子目录）;项目由文件所在文件夹决定,collect 里设定。
        // 宿主线索（claude-desktop / claude-vscode）,供聚焦选目标进程。
        if let Some(ep) = v.get("entrypoint").and_then(|x| x.as_str()) {
            st.set_host(ep);
        }
        let has_origin = v.get("origin").is_some();
        if has_origin {
            st.drop_tentative = true;
        }
        // 续篇文件跳过复制的历史行;其余带 uuid 的行记为已计（归属 = 本文件会话:根 / 子会话）。
        if let Some(u) = uuid {
            if let Some(root) = cursor.family_root.as_deref() {
                if fam.is_seen(store, root, u) {
                    return;
                }
            }
            if !st.session_id.is_empty() {
                fam.new_seen.insert(u.to_string(), st.session_id.clone());
                batch.seen_lines.push((agent.to_string(), u.to_string(), st.session_id.clone()));
            }
        }
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
                // stop_reason 每行都带（同一响应的 thinking / text 分块行一致）;
                // 非 tool_use（end_turn / stop_sequence / max_tokens）= 答完在等用户。
                // 子会话（sidechain）答完只是回到父会话,不算等用户——子会话本就不单独亮起。
                if let Some(reason) = v.pointer("/message/stop_reason").and_then(|x| x.as_str()) {
                    if reason != "tool_use" {
                        st.mark_done(ts, true);
                    }
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
        // 按 （首个 uuid 行时间, 尾部时间, 路径) 升序——根文件先于其续聊 / fork 副本占 uuid。
        // 排序键随游标持久化;只对未扫过且有新内容的文件读头尾。
        let mut entries: Vec<(PathBuf, String, FileCursor)> = files
            .into_iter()
            .map(|path| {
                let scope = path.display().to_string();
                let mut cursor = load_cursor(store, META.id, &scope);
                if cursor.first_ts == 0 && !cursor.up_to_date(&path) {
                    let (first, last) = super::jsonl::head_tail_stamp(&path);
                    cursor.first_ts = first;
                    cursor.last_ts = last;
                }
                (path, scope, cursor)
            })
            .collect();
        entries.sort_by(|a, b| (a.2.first_ts, a.2.last_ts, &a.0).cmp(&(b.2.first_ts, b.2.last_ts, &b.0)));

        let mut batch = Batch::default();
        let mut months = BTreeSet::new();
        let mut fam = FamilyCache::default();
        let mut folders = FolderProjects::new(META.id);

        for (path, scope, mut cursor) in entries {
            let Some(consume) = advance_file(&path, &mut cursor) else { continue };
            let mut cursor = if consume.reset {
                let mut c = FileCursor::fresh();
                c.model = cursor.model.clone();
                c.first_ts = cursor.first_ts;
                c.last_ts = cursor.last_ts;
                // 截断重写的自身重读:行已在 seen_line 里,但不是副本 → 按根处理（既有限制:重计不回滚）
                c.family_resolved = true;
                c
            } else {
                cursor
            };
            cursor.turn.set_file_scope(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
            // 项目 = 文件所在文件夹（`projects/<编码启动目录>/`,续篇 / fork / 子代理都在同一文件夹）;
            // 每次采集重设,旧游标里按行漂移过的目录一并纠正。
            let project = match folder_of(&self.projects_dir, &path) {
                Some(folder) => folders.resolve(store, &folder, &consume.lines, &cursor.turn.project_key),
                None => first_cwd(&consume.lines).map(|c| normalize_project(&c)).unwrap_or_default(),
            };
            cursor.turn.set_project(&project);
            let file_id = path.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            for line in &consume.lines {
                Self::process_line(line, &file_id, &mut cursor, &mut batch, &mut months, store, &mut fam);
            }
            // 文件末尾:落当前轮现状（轮不闭合,下批续累加后整行覆盖）。
            // 会话族未判定（只有 custom-title 等头部元数据行）时不落会话行——副本文件的 sessionId 不能成会话。
            if cursor.family_resolved {
                cursor.turn.flush(&mut batch, META.id);
            }
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

    // ---------- 冻结样本行（本机真实结构,正文 / 标题 / 路径已脱敏,键集保持） ----------

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

    /// 缺 origin 的本地命令 / 命令输出 / 中断标记行（真实键集,正文脱敏为结构标记）。
    fn no_origin(ts: &str, content: &str) -> String {
        format!(r#"{{"parentUuid":"p","isSidechain":false,"promptId":"pr-{ts}","type":"user","message":{{"role":"user","content":{content}}},"uuid":"n-{ts}","timestamp":"{ts}","userType":"external","entrypoint":"claude-desktop","cwd":"E:\\Work\\Demo","sessionId":"{S}","version":"2.1.0","gitBranch":"main"}}"#)
    }
    const CMD: &str = r#""<command-name>/redacted</command-name>""#;
    const CMD_OUT: &str = r#""<local-command-stdout><redacted></local-command-stdout>""#;
    const INTERRUPTED: &str = r#"[{"type":"text","text":"[Request interrupted by user]"}]"#;

    fn collect_lines(tag: &str, batches: &[Vec<String>]) -> Store {
        let dir = std::env::temp_dir().join(format!("tc_claude_{tag}_{}", std::process::id()));
        let proj = dir.join("projects").join("E--Work-Demo");
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

    /// 行内 cwd 是 Bash 当前目录——`cd` 进子目录后工具结果、助手行乃至下一条真实提问都带子目录。
    /// 项目恒为启动目录:轮、会话行、daily_project 都不出现子目录键。
    #[test]
    fn project_stays_at_launch_dir_after_cd() {
        let cd = |line: String| line.replace(r"E:\\Work\\Demo", r"E:\\Work\\Demo\\src-tauri\\src");
        let tool = r#"{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}"#;
        let text = r#"{"type":"text","text":"x"}"#;
        let store = collect_lines(
            "cd_drift",
            &[
                vec![
                    human("2026-09-05T10:00:00.000Z", Some("human")),
                    assistant("2026-09-05T10:00:03.000Z", "msg_a", tool, 100, 10),
                    // `cd src-tauri/src && …` 之后:本轮余下行都在子目录,轮在子目录里结束
                    cd(tool_result("2026-09-05T10:00:05.000Z", "toolu_1")),
                    cd(assistant("2026-09-05T10:00:08.000Z", "msg_b", text, 200, 20)),
                ],
                vec![
                    // 跨批次:下一条真实提问仍带子目录
                    cd(human("2026-09-05T10:05:00.000Z", Some("human"))),
                    cd(assistant("2026-09-05T10:05:03.000Z", "msg_c", text, 300, 30)),
                ],
            ],
        );
        let turns = store.test_turns(META.id);
        assert_eq!(turns.len(), 2);
        assert!(turns.iter().all(|t| t.project_key == "e:/Work/Demo"), "{:?}", turns.iter().map(|t| &t.project_key).collect::<Vec<_>>());
        assert_eq!(store.test_sessions(META.id)[0].project_key, "e:/Work/Demo");
        let keys: Vec<String> = {
            let mut stmt = store.conn().prepare("SELECT DISTINCT project_key FROM daily_project").unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().flatten().collect()
        };
        assert_eq!(keys, vec!["e:/Work/Demo".to_string()]);
        assert!(store.test_project_conservation().is_empty(), "{:?}", store.test_project_conservation());
    }

    /// 身份 = 文件夹。同一文件夹下另一个文件每一行都已漂进子目录（首个 cwd 也是子目录）,仍归该文件夹的项目;
    /// 可读路径来自同批已解析的兄弟文件,并持久化为 `folder:` 映射供下次采集复用。
    #[test]
    fn folder_decides_project_even_if_every_line_drifted() {
        const S2: &str = "0a1b2c3d-0000-4000-8000-000000000002";
        let dir = std::env::temp_dir().join(format!("tc_claude_folder_{}", std::process::id()));
        let proj = dir.join("projects").join("E--Work-Demo");
        std::fs::create_dir_all(&proj).unwrap();
        let text = r#"{"type":"text","text":"x"}"#;
        let drift = |line: String| line.replace(r"E:\\Work\\Demo", r"E:\\Work\\Demo\\src").replace(S, S2);
        let clean = [human("2026-09-05T10:00:00.000Z", Some("human")), assistant("2026-09-05T10:00:03.000Z", "msg_a", text, 100, 10)];
        let drifted = [drift(human("2026-09-06T10:00:00.000Z", Some("human"))), drift(assistant("2026-09-06T10:00:03.000Z", "msg_b", text, 100, 10))];
        std::fs::write(proj.join(format!("{S}.jsonl")), clean.join("\n") + "\n").unwrap();
        std::fs::write(proj.join(format!("{S2}.jsonl")), drifted.join("\n") + "\n").unwrap();
        let adapter = ClaudeCodeAdapter { projects_dir: dir.join("projects") };
        let mut store = Store::open_in_memory().unwrap();
        let ok = adapter.collect(&mut store).is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok);
        let sessions = store.test_sessions(META.id);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|s| s.project_key == "e:/Work/Demo"), "{:?}", sessions.iter().map(|s| &s.project_key).collect::<Vec<_>>());
        assert!(store.test_turns(META.id).iter().all(|t| t.project_key == "e:/Work/Demo"));
        assert_eq!(store.get_cursor(META.id, "folder:E--Work-Demo").as_deref(), Some("e:/Work/Demo"), "映射持久化");
    }

    /// stop_reason 驱动注意力观测——tool_use 未回 = 工具中;非 tool_use = 答完等用户;
    /// 答完后的中断标记（待定零调用轮）= 无状态。
    #[test]
    fn stop_reason_drives_live_phase() {
        use crate::collector::attention::LivePhase;
        let stop = |line: String, reason: &str| line.replace(r#""stop_reason":null"#, &format!(r#""stop_reason":"{reason}""#));
        let tool = r#"{"type":"tool_use","id":"toolu_1","name":"Bash","input":{}}"#;
        let text = r#"{"type":"text","text":"x"}"#;
        let phase = |tag: &str, lines: Vec<String>| {
            let mut store = collect_lines(tag, &[lines]);
            store.take_live().get(&("claude-code".to_string(), S.to_string())).map(|o| o.phase)
        };
        let head = || {
            vec![
                human("2026-09-05T10:00:00.000Z", Some("human")),
                stop(assistant("2026-09-05T10:00:03.000Z", "msg_a", r#"{"type":"thinking","thinking":""}"#, 100, 10), "tool_use"),
                stop(assistant("2026-09-05T10:00:04.000Z", "msg_a", tool, 100, 20), "tool_use"),
            ]
        };
        assert_eq!(phase("live_tools", head()), Some(LivePhase::Tools));
        let mut done = head();
        done.push(tool_result("2026-09-05T10:00:10.000Z", "toolu_1"));
        assert_eq!(phase("live_busy", done.clone()), Some(LivePhase::Busy), "工具结果已回、模型未续");
        done.push(stop(assistant("2026-09-05T10:00:20.000Z", "msg_b", text, 100, 30), "end_turn"));
        assert_eq!(phase("live_done", done.clone()), Some(LivePhase::Done { exact: true }));
        done.push(no_origin("2026-09-05T10:01:00.000Z", INTERRUPTED));
        assert_eq!(phase("live_interrupt", done), Some(LivePhase::Idle));
    }

    /// 同文件出现过 origin → 缺 origin 的零调用行不成轮;拿到响应的照常成轮;跨批次正确。
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

    /// 旧版全文件无 origin:缺 origin 仍是真实输入,零调用按中止轮。
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
        // 无 toolUseResult 但带 tool_result 块 → 仍是工具结果
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
        assert_eq!((t.input, t.output, t.total, t.cache_read, t.cache_write), (1234, 567, 1234 + 567 + 4800 + 120, 4800, 120));
        assert_eq!(model, "claude-opus-5");
        assert!(day.len() == 10 && hour <= 23);
        let zero: Value = serde_json::from_str(r#"{"type":"assistant","timestamp":"2026-09-05T18:30:00Z","message":{"model":"m","usage":{"input_tokens":0,"output_tokens":0}}}"#).unwrap();
        assert!(assistant_usage(&zero).is_none());
        let no_ts: Value = serde_json::from_str(r#"{"type":"assistant","message":{"model":"m","usage":{"input_tokens":1,"output_tokens":1}}}"#).unwrap();
        assert!(assistant_usage(&no_ts).is_none());
    }

    // ---------- 续聊 / fork 副本（复制行只改 sessionId / promptId） ----------

    const S2: &str = "0a1b2c3d-0000-4000-8000-000000000002";
    /// 副本文件的行 = 原行改写 sessionId（uuid / message.id / timestamp 逐字不变）。
    fn forked(line: &str) -> String {
        line.replace(S, S2)
    }
    const TEXT: &str = r#"{"type":"text","text":"x"}"#;
    /// 冻结样本每条 assistant 行带的 cache 两项（同一 message.id 只入账一次）。
    const CPM: i64 = 4800 + 120;

    /// 根文件:两轮 + 末尾一条未得响应的输入（根文件末尾的 stop_hook / 中断行不被复制）。
    fn root_lines() -> Vec<String> {
        vec![
            TITLE_CUSTOM.to_string(),
            human("2026-09-05T10:00:00.000Z", Some("human")),
            assistant("2026-09-05T10:00:05.000Z", "msg_a", TEXT, 100, 10),
            human("2026-09-05T10:02:00.000Z", Some("human")),
            assistant("2026-09-05T10:02:03.000Z", "msg_b", TEXT, 200, 20),
            human("2026-09-05T10:05:00.000Z", Some("human")),
        ]
    }

    /// fork 文件:头部元数据（新 sessionId）+ 复制根文件前 4 行 + 续写两轮。
    fn fork_lines() -> Vec<String> {
        let mut v = vec![
            forked(r#"{"type":"custom-title","customTitle":"<fork title>","sessionId":"0a1b2c3d-0000-4000-8000-000000000001"}"#),
            forked(r#"{"type":"mode","mode":"normal","sessionId":"0a1b2c3d-0000-4000-8000-000000000001"}"#),
        ];
        v.extend(root_lines()[1..5].iter().map(|l| forked(l)));
        v.extend([
            forked(&human("2026-09-05T10:10:00.000Z", Some("human"))),
            forked(&assistant("2026-09-05T10:10:04.000Z", "msg_c", TEXT, 300, 30)),
            forked(&human("2026-09-05T10:12:00.000Z", Some("human"))),
            forked(&assistant("2026-09-05T10:12:02.000Z", "msg_d", TEXT, 400, 40)),
        ]);
        v
    }

    /// 多文件多批次采集:每批写（覆盖）给定文件后跑一次 collect;返回库与根目录（调用方负责删除）。
    fn collect_files(tag: &str, batches: &[Vec<(&str, Vec<String>)>]) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("tc_claude_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join("projects").join("E--Work-Demo");
        std::fs::create_dir_all(&proj).unwrap();
        let adapter = ClaudeCodeAdapter { projects_dir: dir.join("projects") };
        let mut store = Store::open_in_memory().unwrap();
        for batch in batches {
            for (rel, lines) in batch {
                let path = proj.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            }
            assert!(adapter.collect(&mut store).is_ok());
        }
        (store, dir)
    }

    /// 计数摘要:(任务会话数, 物化轮数, request_count Σ, token Σ, 别名数, 已计行数)。
    fn family_summary(store: &Store) -> (usize, usize, i64, i64, i64, i64) {
        let turns = store.test_turns(META.id);
        let rows = store.month_rows("2026-09", "agent", "total", chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()).unwrap();
        let (seen, aliases) = store.test_family_stats(META.id);
        (
            store.test_task_sessions(META.id).len(),
            turns.len(),
            rows[0].message_counts.iter().sum::<i64>(),
            rows[0].month_total,
            aliases,
            seen,
        )
    }

    /// 全量重扫:根 + fork 副本 + fork 目录下的子代理同批出现,根文件 mtime 反而更新（根文件事后被
    /// 追加元数据行）→ 仍按首个 uuid 行时间先处理根;副本折进根会话,轮 / token 只计一份,子会话 parent 归根。
    #[test]
    fn fork_copies_fold_into_root_session() {
        let root_rel = format!("{S}.jsonl");
        let fork_rel = format!("{S2}.jsonl");
        let sub_rel = format!("{S2}/subagents/agent-a1b2c3d4e5.jsonl");
        let subagent = vec![
            forked(&sub_line("2026-09-05T10:10:01.000Z", "prompt")),
            forked(&sub_line("2026-09-05T10:10:02.000Z", "reply")),
        ];
        // 先写 fork 与子代理,最后写根并把 mtime 推到更晚:mtime 序 = fork 先,内容序 = 根先
        let dir = std::env::temp_dir().join(format!("tc_claude_fork_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join("projects").join("E--Work-Demo");
        std::fs::create_dir_all(proj.join(S2).join("subagents")).unwrap();
        std::fs::write(proj.join(&fork_rel), fork_lines().join("\n") + "\n").unwrap();
        std::fs::write(proj.join(&sub_rel), subagent.join("\n") + "\n").unwrap();
        std::fs::write(proj.join(&root_rel), root_lines().join("\n") + "\n").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(proj.join(&root_rel)).unwrap().set_modified(later).unwrap();
        let mut by_mtime = Vec::new();
        super::super::jsonl::discover(&dir.join("projects"), true, &mut by_mtime);
        super::super::jsonl::sort_by_mtime(&mut by_mtime);
        assert!(by_mtime.last().unwrap().ends_with(&root_rel), "前提:按 mtime 根文件排最后");

        let adapter = ClaudeCodeAdapter { projects_dir: dir.join("projects") };
        let mut store = Store::open_in_memory().unwrap();
        assert!(adapter.collect(&mut store).is_ok());
        assert!(adapter.collect(&mut store).is_ok(), "二次采集无新内容:幂等");
        let _ = std::fs::remove_dir_all(&dir);

        let sessions = store.test_sessions(META.id);
        assert!(sessions.iter().all(|s| s.session_id != S2), "副本 sessionId 不成会话: {sessions:?}");
        assert_eq!(store.test_task_sessions(META.id), vec![S.to_string()], "一个对话 = 一条任务会话");
        let root = sessions.iter().find(|s| s.session_id == S).unwrap();
        assert_eq!(root.title.as_deref(), Some("<fork title>"), "标题按最新文件覆盖");
        assert_eq!((root.subagent_count, root.subagent_calls), (1, 1), "fork 目录下的子代理并入根会话");
        let child = sessions.iter().find(|s| s.session_id == "a1b2c3d4e5").unwrap();
        assert_eq!(child.parent_id.as_deref(), Some(S), "子会话 parent 按别名归根");

        let turns = store.test_turns(META.id);
        assert_eq!(turns.len(), 5, "根 3 轮（含末尾未响应的一轮）+ fork 续写 2 轮,复制的 2 轮不重复: {turns:?}");
        assert!(turns.iter().all(|t| t.session_id == S));
        let calls: Vec<i64> = turns.iter().map(|t| t.model_calls).collect();
        assert_eq!(calls, vec![1, 1, 0, 1 + 1, 1], "第 4 轮含子代理调用");
        assert_eq!(turns[2].aborted, false, "根文件末尾未响应的轮不判中止");
        assert_eq!(turns.iter().map(|t| t.total_tokens).sum::<i64>(), 110 + 220 + 330 + 440 + 55 + 4 * CPM, "token 只计一份");
        let (tasks, rows, requests, tokens, aliases, seen) = family_summary(&store);
        assert_eq!((tasks, rows, requests, tokens), (1, 5, 4, 110 + 220 + 330 + 440 + 55 + 4 * CPM));
        assert_eq!((aliases, seen), (1, 5 + 4 + 2), "一个别名;已计行 = 根 5 + fork 新 4 + 子代理 2");
        assert!(store.test_project_conservation().is_empty(), "{:?}", store.test_project_conservation());
    }

    /// 增量采集:「先根后 fork」（真实顺序）与「先 fork 后根」（极端顺序）计数一致——只是根会话 id 不同。
    #[test]
    fn fork_incremental_orders_agree() {
        let root_rel = format!("{S}.jsonl");
        let fork_rel = format!("{S2}.jsonl");
        let (a, dir_a) = collect_files(
            "inc_root_first",
            &[vec![(root_rel.as_str(), root_lines())], vec![(fork_rel.as_str(), fork_lines())]],
        );
        let (b, dir_b) = collect_files(
            "inc_fork_first",
            &[vec![(fork_rel.as_str(), fork_lines())], vec![(root_rel.as_str(), root_lines())]],
        );
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
        let sa = family_summary(&a);
        let sb = family_summary(&b);
        assert_eq!(sa, sb, "两种到达顺序计数一致");
        assert_eq!(sa, (1, 5, 4, 110 + 220 + 330 + 440 + 4 * CPM, 1, 9));
        assert_eq!(a.test_task_sessions(META.id), vec![S.to_string()], "真实顺序:根文件的 sessionId 是根");
        assert_eq!(b.test_task_sessions(META.id), vec![S2.to_string()], "极端顺序:先到者为根,根文件成续篇");
        assert!(a.test_project_conservation().is_empty() && b.test_project_conservation().is_empty());
        // fork 文件再续写一轮:只计新行,不产生第二条会话
        let (c, dir_c) = collect_files(
            "inc_fork_grow",
            &[
                vec![(root_rel.as_str(), root_lines())],
                vec![(fork_rel.as_str(), fork_lines())],
                vec![(fork_rel.as_str(), {
                    let mut v = fork_lines();
                    v.push(forked(&human("2026-09-05T10:20:00.000Z", Some("human"))));
                    v.push(forked(&assistant("2026-09-05T10:20:02.000Z", "msg_e", TEXT, 500, 50)));
                    v
                })],
            ],
        );
        let _ = std::fs::remove_dir_all(&dir_c);
        assert_eq!(family_summary(&c), (1, 6, 5, 110 + 220 + 330 + 440 + 550 + 5 * CPM, 1, 11));
    }

    /// 旧版副本（早期桌面版）:复制行连 `sessionId` 都沿用根会话,
    /// 只有文件名是新 id → 仍按已计行折叠,别名取文件名。
    #[test]
    fn fork_copies_keeping_root_session_id_fold_too() {
        let root_rel = format!("{S}.jsonl");
        let fork_rel = format!("{S2}.jsonl");
        let mut old_fork = vec![TITLE_CUSTOM.to_string()];
        old_fork.extend(root_lines()[1..5].iter().cloned()); // 原样复制,sessionId 仍是 S
        old_fork.extend([
            human("2026-09-05T10:10:00.000Z", Some("human")),
            assistant("2026-09-05T10:10:04.000Z", "msg_c", TEXT, 300, 30),
        ]);
        let (store, dir) = collect_files(
            "old_fork",
            &[vec![(root_rel.as_str(), root_lines())], vec![(fork_rel.as_str(), old_fork)]],
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(family_summary(&store), (1, 4, 3, 110 + 220 + 330 + 3 * CPM, 1, 5 + 2));
        assert_eq!(store.test_task_sessions(META.id), vec![S.to_string()]);
        let alias: Option<String> = store.session_root_alias(META.id, S2);
        assert_eq!(alias.as_deref(), Some(S), "别名取文件名主干");
    }

    /// 端到端：主会话 + 子代理文件走真实 collect,断言三层计数、四段时间、去重、守恒。
    #[test]
    fn end_to_end_session_turns_and_subagent_merge() {
        let dir = std::env::temp_dir().join(format!("tc_claude_turns_{}", std::process::id()));
        let proj = dir.join("projects").join("E--Work-Demo");
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
        assert_eq!(t1.total_tokens, 110 + 220 + 330 + 55 + 3 * CPM, "按 message.id 去重 + 子代理 token 并入");
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
        assert_eq!(rows[0].month_total, 110 + 220 + 330 + 11 + 55 + 4 * CPM);
        assert!(store.test_project_conservation().is_empty(), "{:?}", store.test_project_conservation());
    }
}
