//! CodeBuddy 适配器。
//!
//! 数据源（Windows,明文 JSON,无需解码）：
//! - `%LOCALAPPDATA%/CodeBuddyExtension/Data/<profile>/CodeBuddyIDE/<profile>/history/
//!   <workspaceHash>/<sessionId>/index.json`——递归扫全部 history 根;
//! - 会话元数据库 `%APPDATA%/CodeBuddy CN/codebuddy-sessions.vscdb`（国际版 `CodeBuddy/`
//!   同名兜底）：VS Code 式 `ItemTable`,键 `session:<conversationId>` → JSON
//!   `{cwd, title, customTitle?, isPlayground?, ...}`——只读项目与标题。
//!
//! 结构：`requests[]` 追加式（无模型字段!），每条：
//! `usage.{inputTokens, outputTokens, totalTokens, cacheTokens,
//! cachedWriteTokens, cachedMissTokens, credit}`。
//! 口径：`inputTokens = cacheTokens + cachedMiss + cachedWrite`、
//! `total = input + output` → 入库 input = inputTokens − cacheTokens
//! （cache-exclusive,与全源统一）,output 保持 provider 值,total 取 provider total。
//!
//! 增量游标：单文件 JSON（非 JSONL）,offset 不适用——用「已处理 request 条数」：
//! generation 不变跳过;count > len（重写/清空）→ 归零重读。
//! 大体积 messages 由 serde derive 按字段跳过,不构建 Value 树。
//!
//! 模型归属：本地消息
//! `<会话>/messages/<id>.json` 的 `extra.modelId`（`extra` 是 JSON 字符串;按 `requests[].messages`
//! 顺序取首条非 helper、requestId 相符的消息,通常下标 0 的 user 消息即带; 642/642 覆盖,
//! 与旧导入账本重叠 630 条中 628 条一致）→ 缺失 `unknown`。`auto`（智能路由,本地不知实际模型）
//! 视为未解析;`custom-local:*`（用户自配模型）原样入库。
//!
//! 积分：`requests[].usage.credit`
//! → `daily_usage.credit`。与旧导入账本重叠 630 条中 537 条逐条相等,合计约低 0.7%;
//! 官网另计的辅助请求（本地无 history）不在本地,整体约低 2〜3%。
//!
//! 轮与时间：`requests[]` 天然请求级 → 每条带 usage 的 request = 1 轮
//! （turn_seq = 下标 + 1,model_calls = 1、tool_calls = 0——`messages` 只是 id 列表,
//! 不猜步数）;会话 = index.json 所在目录名;只有 `startedAt`、无结束时间 →
//! wall / model / tool / gap 一律 NULL;`state` 非 complete / running 计错。
//!
//! 项目：
//! 1. 元数据库 `session:<会话目录名>.cwd` → 归一化;`isPlayground = true`（IDE 临时对话目录）
//!    → `unknown`（自动折叠进 Scratch）。标题 = `customTitle` 优先,其次 `title`。
//! 2. 元数据缺行 → 同一 `<workspaceHash>` 目录下有元数据的兄弟会话的目录（hash =
//!    MD5（反斜杠形态的 cwd),一个 hash 目录恒对应一个 cwd;兄弟目录不一致 → 不取）。
//! 3. 仍无 → `unknown`。
//!
//! 归属自愈：游标记已落库的项目。旧游标（无此字段 = unknown）或上次落 unknown、本次解析出目录
//! → 即便文件未变也重读,`replace_session` 整会话重发轮（**只重发轮层,不重复加 daily_usage**）。
//! 已落为目录的不再改（与 session 表「首个非 unknown」一致）,元数据库暂不可读不会把目录降级。
//! 故存量库无需 user_version 迁移,下一轮采集即归位。

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

use super::store::{Batch, SessionRow, Store, Tokens, TurnPart, TurnRow};
use super::attention::{LivePhase, LiveTurn};
use super::turns::{UNKNOWN_PROJECT, normalize_project};
use super::{
    Adapter, AdapterError, AdapterMeta, CollectOutcome, CollectResult, ProbeOutcome, clamp0,
    jsonl, millis_to_local_day_hour, text,
};

pub struct CodebuddyAdapter {
    data_dir: PathBuf,
    /// 会话元数据库候选（按序,先读到的会话优先）;不存在 / 打不开 → 跳过,不影响用量采集。
    meta_dbs: Vec<PathBuf>,
}

const META_DB_NAME: &str = "codebuddy-sessions.vscdb";
const META_BUSY_TIMEOUT: Duration = Duration::from_secs(1);
/// 每条请求最多探查的消息文件数（首条 user 消息即带 modelId,余量兜底缺文件 / helper）。
const MODEL_PROBE_MESSAGES: usize = 4;

#[derive(Deserialize)]
struct MessageFile {
    #[serde(default)]
    extra: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MessageExtra {
    #[serde(default)]
    model_id: String,
    #[serde(default)]
    request_id: String,
    #[serde(default)]
    is_helper_message: bool,
}

/// 本地消息里的请求模型（`auto` / 缺失 → None）。`extra` 兼容 JSON 字符串与对象两种形态。
fn local_model(session_dir: &Path, r: &IndexRequest) -> Option<String> {
    let ids = r.messages.iter().filter_map(|m| m.as_str().or_else(|| m.get("id").and_then(|v| v.as_str())));
    for id in ids.filter(|id| !id.is_empty() && !id.contains(['/', '\\', '.'])).take(MODEL_PROBE_MESSAGES) {
        let Ok(bytes) = std::fs::read(session_dir.join("messages").join(format!("{id}.json"))) else { continue };
        let Ok(msg) = serde_json::from_slice::<MessageFile>(&bytes) else { continue };
        let extra = match msg.extra {
            Some(serde_json::Value::String(s)) => serde_json::from_str::<MessageExtra>(&s).ok(),
            Some(v @ serde_json::Value::Object(_)) => serde_json::from_value::<MessageExtra>(v).ok(),
            _ => None,
        };
        let Some(extra) = extra else { continue };
        if extra.is_helper_message || (!extra.request_id.is_empty() && extra.request_id != r.id) {
            continue;
        }
        let model = extra.model_id.trim();
        if model.is_empty() {
            continue;
        }
        return (model != "auto").then(|| model.to_string());
    }
    None
}

/// 一个会话的元数据（已解析为 project_key）。
#[derive(Debug, Clone, PartialEq)]
struct SessionMeta {
    project: String,
    title: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MetaJson {
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    custom_title: String,
    #[serde(default)]
    is_playground: bool,
}

impl MetaJson {
    fn into_meta(self) -> SessionMeta {
        let project = if self.is_playground { UNKNOWN_PROJECT.to_string() } else { normalize_project(&self.cwd) };
        let title = [self.custom_title, self.title].into_iter().map(|t| t.trim().to_string()).find(|t| !t.is_empty());
        SessionMeta { project, title }
    }
}

/// 读全部候选元数据库 → 会话 id → 元数据。任一库失败只跳过该库。
fn load_session_meta(dbs: &[PathBuf]) -> HashMap<String, SessionMeta> {
    let mut out = HashMap::new();
    for db in dbs.iter().filter(|p| p.is_file()) {
        let Ok(conn) = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX) else { continue };
        let _ = conn.busy_timeout(META_BUSY_TIMEOUT);
        let Ok(mut stmt) = conn.prepare("SELECT key, value FROM ItemTable WHERE key LIKE 'session:%'") else { continue };
        let rows = stmt.query_map([], |r| {
            let key: String = r.get(0)?;
            let value = match r.get_ref(1)? {
                rusqlite::types::ValueRef::Text(b) | rusqlite::types::ValueRef::Blob(b) => text::decode_bytes(b).into_owned(),
                _ => String::new(),
            };
            Ok((key, value))
        });
        let Ok(rows) = rows else { continue };
        for (key, value) in rows.flatten() {
            let Some(id) = key.strip_prefix("session:").filter(|id| !id.is_empty()) else { continue };
            let Ok(json) = serde_json::from_str::<MetaJson>(&value) else { continue };
            out.entry(id.to_string()).or_insert_with(|| json.into_meta());
        }
    }
    out
}

/// index.json 路径 → （会话 id = 所在目录名, 工作区 hash = 上一级目录名)。
fn session_and_workspace(path: &Path) -> (String, String) {
    let name = |p: Option<&Path>| p.and_then(|d| d.file_name()).and_then(|n| n.to_str()).unwrap_or_default().to_string();
    let session = path.parent();
    (name(session), name(session.and_then(|s| s.parent())))
}

/// 工作区 hash → 目录（只收有元数据且非 unknown 的会话;同 hash 出现不同目录 → None,不取）。
fn workspace_projects(files: &[PathBuf], metas: &HashMap<String, SessionMeta>) -> HashMap<String, Option<String>> {
    let mut out: HashMap<String, Option<String>> = HashMap::new();
    for path in files {
        let (sid, ws) = session_and_workspace(path);
        let Some(meta) = metas.get(&sid).filter(|m| m.project != UNKNOWN_PROJECT) else { continue };
        if ws.is_empty() {
            continue;
        }
        out.entry(ws)
            .and_modify(|cur| {
                if cur.as_deref() != Some(meta.project.as_str()) {
                    *cur = None;
                }
            })
            .or_insert_with(|| Some(meta.project.clone()));
    }
    out
}

static META: AdapterMeta = AdapterMeta {
    id: "codebuddy",
    name: "CodeBuddy",
    location: "%LOCALAPPDATA%/CodeBuddyExtension/Data",
    kind: "json",
};

impl CodebuddyAdapter {
    pub fn new() -> Self {
        let data_dir = std::env::var_os("LOCALAPPDATA")
            .map(|base| {
                PathBuf::from(base)
                    .join("CodeBuddyExtension")
                    .join("Data")
            })
            .unwrap_or_default();
        let meta_dbs = std::env::var_os("APPDATA")
            .map(|base| {
                ["CodeBuddy CN", "CodeBuddy"].iter().map(|app| PathBuf::from(&base).join(app).join(META_DB_NAME)).collect()
            })
            .unwrap_or_default();
        CodebuddyAdapter { data_dir, meta_dbs }
    }

    /// 全部 history 根：Data/<profile>/CodeBuddyIDE/<sub>/history（层数固定但
    /// 目录名不定,直接按路径模式探测）。
    fn history_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.data_dir) else { return roots };
        for profile in entries.flatten() {
            let ide = profile.path().join("CodeBuddyIDE");
            let Ok(subs) = std::fs::read_dir(&ide) else { continue };
            for sub in subs.flatten() {
                let history = sub.path().join("history");
                if history.is_dir() {
                    roots.push(history);
                }
            }
        }
        roots
    }
}

/// 游标（count = 已处理的 requests 条数;project = 轮层已落的 project_key,旧游标缺省 = unknown）。
#[derive(serde::Serialize, serde::Deserialize)]
struct IndexCursor {
    count: u64,
    size: u64,
    mtime: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
}

impl IndexCursor {
    fn fresh() -> Self {
        IndexCursor { count: 0, size: 0, mtime: 0, project: None }
    }
    fn up_to_date(&self, path: &Path) -> bool {
        jsonl::generation(path)
            .map(|(size, mtime)| size == self.size && mtime == self.mtime)
            .unwrap_or(false)
    }
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

// ---- index.json 结构（serde derive 跳过 messages 等未知字段,省内存） ----

#[derive(Deserialize)]
struct IndexFile {
    #[serde(default)]
    requests: Vec<IndexRequest>,
}

#[derive(Deserialize)]
struct IndexRequest {
    #[serde(default)]
    id: String,
    #[serde(rename = "startedAt", default)]
    started_at: Option<i64>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    usage: Option<IndexUsage>,
    /// 本请求的消息 id 列表（字符串;形态漂移时容忍对象 / 其他）。
    #[serde(default)]
    messages: Vec<serde_json::Value>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct IndexUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
    #[serde(default)]
    cache_tokens: i64,
    #[serde(default)]
    cached_write_tokens: i64,
    /// 本请求消耗积分（入 daily_usage.credit）。
    #[serde(default)]
    credit: f64,
}

/// 从一条 request 提取 （day, hour, token 分项);usage/time 缺失 → None。
/// cache_read = cacheTokens、cache_write = cachedWriteTokens（input 口径不变,仍含 write）。
fn parse_request(r: &IndexRequest) -> Option<(String, u8, Tokens)> {
    let usage = r.usage.as_ref()?;
    let started = r.started_at?;
    if started <= 0 {
        return None;
    }
    let (day, hour) = millis_to_local_day_hour(started)?;
    let input = clamp0(usage.input_tokens - clamp0(usage.cache_tokens));
    let output = clamp0(usage.output_tokens);
    let total = if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        clamp0(usage.input_tokens) + output
    };
    if total <= 0 && input == 0 && output == 0 {
        return None;
    }
    let tokens = Tokens { input, output, total, cache_read: clamp0(usage.cache_tokens), cache_write: clamp0(usage.cached_write_tokens) };
    Some((day, hour, tokens))
}

/// 本文件的轮归属:会话 id + 项目 + 标题（元数据）。
struct TurnTarget<'a> {
    session_id: &'a str,
    project: &'a str,
    title: Option<&'a str>,
}

/// 一条 request = 一轮（与 daily_usage 的 `add_usage（.., turns=1)` 同一份 day / model / tokens）。
fn push_request_turn(batch: &mut Batch, target: &TurnTarget, idx: usize, r: &IndexRequest, day: &str, model: &str, t: Tokens) {
    let session_id = target.session_id;
    let started = r.started_at.unwrap_or_default();
    let failed = !r.state.is_empty() && r.state != "complete" && r.state != "running";
    batch.add_turn(
        META.id,
        TurnRow {
            session_id: session_id.to_string(),
            turn_seq: idx as i64 + 1,
            day: day.to_string(),
            project_key: target.project.to_string(),
            model_key: model.to_string(),
            started_at: started,
            ended_at: started,
            wall_ms: None,
            model_ms: None,
            tool_ms: None,
            ttft_ms: None,
            gap_ms: None,
            model_calls: 1,
            tool_calls: 0,
            error_count: failed as i64,
            retry_count: 0,
            aborted: false,
            parts: vec![TurnPart {
                day: day.to_string(),
                model: model.to_string(),
                input: t.input,
                output: t.output,
                total: t.total,
                model_calls: 1,
                turn_mark: 1,
            }],
        },
    );
    batch.upsert_session(
        META.id,
        SessionRow {
            session_id: session_id.to_string(),
            project_key: Some(target.project.to_string()),
            project_authoritative: false,
            parent_id: None,
            title: target.title.map(str::to_string),
            started_at: Some(started),
            ended_at: Some(started),
        },
    );
}

impl Adapter for CodebuddyAdapter {
    fn meta(&self) -> &'static AdapterMeta {
        &META
    }

    fn probe(&self) -> ProbeOutcome {
        if !self.history_roots().is_empty() {
            ProbeOutcome { status: "ready".into(), fingerprint: None }
        } else {
            ProbeOutcome { status: "no_source".into(), fingerprint: None }
        }
    }

    fn collect(&self, store: &mut Store) -> CollectResult {
        let roots = self.history_roots();
        if roots.is_empty() {
            return Err(AdapterError::new("no_source", format!("missing {}", self.data_dir.display())));
        }
        let mut files = Vec::new();
        for root in &roots {
            jsonl::discover_named(root, true, "index.json", &mut files);
        }
        jsonl::sort_by_mtime(&mut files);

        let mut batch = Batch::default();
        let mut months = BTreeSet::new();
        let metas = load_session_meta(&self.meta_dbs);
        let workspaces = workspace_projects(&files, &metas);

        for path in files {
            let scope = path.display().to_string();
            let mut cursor = store
                .get_cursor(META.id, &scope)
                .and_then(|j| serde_json::from_str::<IndexCursor>(&j).ok())
                .unwrap_or_else(IndexCursor::fresh);
            let (session_id, workspace) = session_and_workspace(&path);
            let meta = metas.get(&session_id);
            let resolved = match meta {
                Some(m) => m.project.clone(),
                None => workspaces.get(&workspace).cloned().flatten().unwrap_or_else(|| UNKNOWN_PROJECT.to_string()),
            };
            // 已落为目录的不改;上次 unknown（含旧游标）而本次解析出目录 → 整会话重发轮层。
            let landed = cursor.project.clone().unwrap_or_else(|| UNKNOWN_PROJECT.to_string());
            let project = if landed != UNKNOWN_PROJECT { landed.clone() } else { resolved };
            let reattribute = project != landed;
            if cursor.up_to_date(&path) && !reattribute {
                continue;
            }
            // 截断/重写：count 超过实际条数 → 归零重读（已聚合不回滚）
            let file: IndexFile = match std::fs::File::open(&path)
                .map_err(|e| e.to_string())
                .and_then(|f| serde_json::from_reader(f).map_err(|e| e.to_string()))
            {
                Ok(f) => f,
                Err(_) => continue, // 文件此刻不可读/坏行,下轮重试
            };
            if (cursor.count as usize) > file.requests.len() {
                cursor.count = 0;
            }
            // 用量只加新请求;重归属时轮层从第 0 条整会话重发（已入账的 daily_usage 不动）。
            let usage_from = cursor.count as usize;
            let turns_from = if reattribute { 0 } else { usage_from };
            let session_dir = path.parent().unwrap_or(Path::new(""));
            let target = TurnTarget { session_id: &session_id, project: &project, title: meta.and_then(|m| m.title.as_deref()) };
            if reattribute && !session_id.is_empty() {
                batch.replace_session(META.id, &session_id);
            }
            for (idx, r) in file.requests.iter().enumerate().skip(turns_from) {
                if let Some((day, hour, tokens)) = parse_request(r) {
                    let model = local_model(session_dir, r).unwrap_or_else(|| "unknown".into());
                    if idx >= usage_from {
                        batch.add_usage(&day, Some(hour), META.id, &model, tokens, 1);
                        batch.add_credit(&day, META.id, &model, r.usage.as_ref().map_or(0.0, |u| u.credit));
                        months.insert(day[..7].to_string());
                    }
                    if !session_id.is_empty() {
                        push_request_turn(&mut batch, &target, idx, r, &day, &model, tokens);
                    }
                }
            }
            // 末条 request 现状（running = 在处理;complete = 答完在等用户;其余 = 失败 / 取消,
            // 不亮起）。无结束时间 → 以文件 mtime（请求完成时 index.json 重写）作最近事件。
            if let (false, Some(last)) = (session_id.is_empty(), file.requests.iter().rev().find(|r| r.started_at.is_some())) {
                let phase = match last.state.as_str() {
                    "running" => LivePhase::Busy,
                    "complete" => LivePhase::Done { exact: true },
                    _ => LivePhase::Idle,
                };
                let mtime = jsonl::generation(&path).map(|(_, m)| m).unwrap_or(0);
                batch.live.insert(
                    (META.id.to_string(), session_id.clone()),
                    LiveTurn {
                        project_key: project.clone(),
                        parent_id: None,
                        title: target.title.map(str::to_string),
                        host: None,
                        phase,
                        last_event: mtime.max(last.started_at.unwrap_or(0)),
                    },
                );
            }
            cursor.count = file.requests.len() as u64;
            cursor.project = Some(project);
            if let Some((size, mtime)) = jsonl::generation(&path) {
                cursor.size = size;
                cursor.mtime = mtime;
            }
            batch.cursors.push((scope, cursor.to_json()));
        }

        store.commit(META.id, &batch).map_err(|e| AdapterError::new("error", e))?;
        Ok(CollectOutcome { events: batch.events, months })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_json(input: i64, output: i64, total: i64, cache: i64, miss: i64, write: i64) -> String {
        format!(
            r#"{{"inputTokens":{input},"outputTokens":{output},"totalTokens":{total},"lastTokens":59002,"cacheTokens":{cache},"cachedWriteTokens":{write},"cachedMissTokens":{miss},"credit":1.85}}"#
        )
    }

    fn request_json(ts: i64, usage: &str) -> String {
        format!(r#"{{"id":"r1","type":"ask","state":"complete","startedAt":{ts},"messages":[{{"x":1}}],"usage":{usage}}}"#)
    }

    #[test]
    fn parses_cache_exclusive_input() {
        // 实证样本:in=293013 out=7631 total=300644 cache=221184 miss=71829 write=0
        let raw = request_json(1_787_914_300_234, &usage_json(293013, 7631, 300644, 221184, 71829, 0));
        let req: IndexRequest = serde_json::from_str(&raw).unwrap();
        let (day, hour, t) = parse_request(&req).unwrap();
        let (input, output, total) = (t.input, t.output, t.total);
        assert_eq!((t.cache_read, t.cache_write), (221184, 0));
        assert_eq!(input, 71829); // = cachedMiss + cachedWrite(cache-exclusive)
        assert_eq!(output, 7631);
        assert_eq!(total, 300644); // provider total(含 cache)
        assert_eq!(day.len(), 10);
        assert!(hour <= 23);
    }

    #[test]
    fn total_falls_back_to_input_plus_output() {
        let raw = request_json(1_787_914_300_234, &usage_json(1000, 500, 0, 200, 800, 0));
        let req: IndexRequest = serde_json::from_str(&raw).unwrap();
        let (_, _, t) = parse_request(&req).unwrap();
        assert_eq!((t.input, t.output, t.total), (800, 500, 1500));
    }

    #[test]
    fn skips_missing_usage_or_time() {
        let no_usage: IndexRequest = serde_json::from_str(r#"{"id":"r","startedAt":1757000000000}"#).unwrap();
        assert!(parse_request(&no_usage).is_none());
        let no_time: IndexRequest = serde_json::from_str(&format!(r#"{{"id":"r","usage":{}}}"#, usage_json(1, 1, 2, 0, 1, 0))).unwrap();
        assert!(parse_request(&no_time).is_none());
        let bad_time: IndexRequest = serde_json::from_str(&format!(r#"{{"id":"r","startedAt":-5,"usage":{}}}"#, usage_json(1, 1, 2, 0, 1, 0))).unwrap();
        assert!(parse_request(&bad_time).is_none());
        // 全零 usage 跳过
        let zero: IndexRequest = serde_json::from_str(&request_json(1_757_000_000_000, &usage_json(0, 0, 0, 0, 0, 0))).unwrap();
        assert!(parse_request(&zero).is_none());
    }

    #[test]
    fn messages_field_ignored_by_derive() {
        let raw = r#"{"messages":[{"role":"user","content":"<list:14> 嵌套任意结构"}],"requests":[{"id":"r1","type":"craft","state":"complete","startedAt":1757000000000,"usage":{"inputTokens":10,"outputTokens":5,"totalTokens":15,"cacheTokens":0,"cachedWriteTokens":0,"cachedMissTokens":10}}]}"#;
        let file: IndexFile = serde_json::from_str(raw).unwrap();
        assert_eq!(file.requests.len(), 1);
        let (_, _, t) = parse_request(&file.requests[0]).unwrap();
        assert_eq!((t.input, t.total), (10, 15));
    }

    /// PHASE12 S2:真实结构 index.json 走完整 collect——一 request 一轮、项目 unknown、时间 NULL、守恒。
    #[test]
    fn s2_requests_become_turns() {
        let dir = std::env::temp_dir().join(format!("tc_cb_s2_{}", std::process::id()));
        let sess = dir.join("Data").join("p1").join("CodeBuddyIDE").join("p1").join("history").join("wshash").join("sess-1");
        std::fs::create_dir_all(&sess).unwrap();
        let reqs = [
            request_json(1_788_602_400_000, &usage_json(1000, 100, 1100, 600, 400, 0)),
            request_json(1_788_602_460_000, &usage_json(500, 50, 550, 0, 500, 0)).replace(r#""state":"complete""#, r#""state":"error""#),
            r#"{"id":"r3","type":"ask","state":"running","messages":["m1"]}"#.to_string(),
        ];
        std::fs::write(sess.join("index.json"), format!(r#"{{"messages":["m1","m2"],"requests":[{}]}}"#, reqs.join(","))).unwrap();
        let adapter = CodebuddyAdapter { data_dir: dir.join("Data"), meta_dbs: vec![] };
        let mut store = Store::open_in_memory().unwrap();
        let ok = adapter.collect(&mut store).is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok);
        let turns = store.test_turns(META.id);
        assert_eq!(turns.len(), 2, "无 usage 的 running request 不成轮");
        assert!(turns.iter().all(|t| t.model_calls == 1 && t.tool_calls == 0 && t.project_key == "unknown"));
        assert!(turns.iter().all(|t| t.wall_ms.is_none() && t.model_ms.is_none() && t.tool_ms.is_none() && t.gap_ms.is_none()));
        assert_eq!((turns[0].error_count, turns[1].error_count), (0, 1));
        assert_eq!(store.test_task_sessions(META.id), vec!["sess-1".to_string()]);
        assert!(store.test_project_conservation().is_empty(), "{:?}", store.test_project_conservation());
    }

    /// 会话元数据库（ItemTable 同 VS Code 形态）;rows = (会话 id, JSON 值)。
    fn write_meta_db(path: &Path, rows: &[(&str, &str)]) {
        let _ = std::fs::remove_file(path);
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)").unwrap();
        for (id, value) in rows {
            conn.execute("INSERT INTO ItemTable (key, value) VALUES (?1, ?2)", rusqlite::params![format!("session:{id}"), value]).unwrap();
        }
    }

    fn write_session(history: &Path, workspace: &str, session: &str, ts: i64) {
        let dir = history.join(workspace).join(session);
        std::fs::create_dir_all(&dir).unwrap();
        let reqs = [request_json(ts, &usage_json(1000, 100, 1100, 600, 400, 0)), request_json(ts + 60_000, &usage_json(500, 50, 550, 0, 500, 0))];
        std::fs::write(dir.join("index.json"), format!(r#"{{"requests":[{}]}}"#, reqs.join(","))).unwrap();
    }

    fn session_projects(store: &Store) -> Vec<(String, String, Option<String>)> {
        store.test_sessions(META.id).into_iter().map(|s| (s.session_id, s.project_key, s.title)).collect()
    }

    /// 元数据 cwd → 项目 + 标题（customTitle 优先）;缺行按同工作区兄弟会话;playground → unknown;无任何线索 → unknown。
    #[test]
    fn projects_resolved_from_session_meta() {
        let dir = std::env::temp_dir().join(format!("tc_cb_proj_{}", std::process::id()));
        let history = dir.join("Data").join("p1").join("CodeBuddyIDE").join("p1").join("history");
        write_session(&history, "wsA", "sA1", 1_788_602_400_000);
        write_session(&history, "wsA", "sA2", 1_788_602_500_000); // 元数据缺行 → 兄弟 sA1 的目录
        write_session(&history, "wsP", "sP", 1_788_602_600_000); // playground
        write_session(&history, "wsX", "sX", 1_788_602_700_000); // 无元数据、无兄弟
        let db = dir.join(META_DB_NAME);
        write_meta_db(&db, &[
            ("sA1", r#"{"conversationId":"sA1","cwd":"E:\\Work\\Demo\\","title":"auto","customTitle":"renamed","status":"Completed"}"#),
            ("sP", r#"{"conversationId":"sP","cwd":"c:/Users/Demo/CodeBuddy/20260101000000","title":"pg","isPlayground":true}"#),
        ]);
        let adapter = CodebuddyAdapter { data_dir: dir.join("Data"), meta_dbs: vec![dir.join("missing.vscdb"), db] };
        let mut store = Store::open_in_memory().unwrap();
        let ok = adapter.collect(&mut store).is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok);
        let demo = "e:/Work/Demo".to_string();
        assert_eq!(session_projects(&store), vec![
            ("sA1".into(), demo.clone(), Some("renamed".into())),
            ("sA2".into(), demo.clone(), None),
            ("sP".into(), "unknown".into(), Some("pg".into())),
            ("sX".into(), "unknown".into(), None),
        ]);
        let turns = store.test_turns(META.id);
        assert_eq!(turns.iter().filter(|t| t.project_key == demo).count(), 4);
        assert!(store.test_project_conservation().is_empty(), "{:?}", store.test_project_conservation());
    }

    /// 存量自愈:先无元数据落 unknown → 元数据出现（文件未变）→ 轮层整会话改归属、用量不重复入账;
    /// 之后元数据库不可读 → 已落目录不降级。
    #[test]
    fn unknown_sessions_reattributed_without_double_usage() {
        let dir = std::env::temp_dir().join(format!("tc_cb_heal_{}", std::process::id()));
        let history = dir.join("Data").join("p1").join("CodeBuddyIDE").join("p1").join("history");
        write_session(&history, "wsA", "sA1", 1_788_602_400_000);
        let db = dir.join(META_DB_NAME);
        let adapter = CodebuddyAdapter { data_dir: dir.join("Data"), meta_dbs: vec![db.clone()] };
        let mut store = Store::open_in_memory().unwrap();
        let first = adapter.collect(&mut store).map(|o| o.events);
        let before = store.test_turns(META.id).iter().map(|t| t.project_key.clone()).collect::<Vec<_>>();
        write_meta_db(&db, &[("sA1", r#"{"cwd":"E:/Work/Demo","title":"t"}"#)]);
        let second = adapter.collect(&mut store).map(|o| o.events);
        let healed = session_projects(&store);
        let healed_turns = store.test_turns(META.id);
        let conservation = store.test_project_conservation();
        std::fs::remove_file(&db).unwrap();
        let third = adapter.collect(&mut store).map(|o| o.events);
        let after_loss = session_projects(&store);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(first.ok(), Some(2));
        assert_eq!(before, vec!["unknown".to_string(); 2]);
        assert_eq!(second.ok(), Some(0), "重归属不产生新用量事件");
        assert_eq!(healed, vec![("sA1".into(), "e:/Work/Demo".into(), Some("t".into()))]);
        assert_eq!(healed_turns.len(), 2);
        assert!(healed_turns.iter().all(|t| t.project_key == "e:/Work/Demo"));
        assert!(conservation.is_empty(), "用量未重复入账:{conservation:?}");
        assert_eq!(third.ok(), Some(0));
        assert_eq!(after_loss, healed);
    }

    /// 本地模型:extra 字符串形态取 modelId;helper / requestId 不符跳过;auto → unknown;
    /// 积分按请求 usage.credit 入 daily_usage.credit,重复采集不重复入账。
    #[test]
    fn models_and_credits_from_local_data() {
        let dir = std::env::temp_dir().join(format!("tc_cb_model_{}", std::process::id()));
        let sess = dir.join("Data").join("p1").join("CodeBuddyIDE").join("p1").join("history").join("ws").join("s1");
        std::fs::create_dir_all(sess.join("messages")).unwrap();
        let msg = |id: &str, extra: &str| {
            let body = serde_json::json!({ "role": "user", "id": id, "extra": extra });
            std::fs::write(sess.join("messages").join(format!("{id}.json")), body.to_string()).unwrap();
        };
        msg("m1", r#"{"requestId":"rA","modelId":"glm-5.3-flash","modelName":"GLM-5.3-Flash","isHelperMessage":false}"#);
        msg("m2h", r#"{"requestId":"rB","modelId":"helper-model","isHelperMessage":true}"#);
        msg("m2", r#"{"requestId":"rB","modelId":"deepseek-v4.1-flash"}"#);
        msg("m3", r#"{"requestId":"rC","modelId":"auto"}"#);
        msg("m4", r#"{"requestId":"other","modelId":"wrong-model"}"#);
        msg("m5", r#"{"requestId":"rE","modelId":"glm-5.3"}"#);
        let usage = usage_json(1000, 100, 1100, 600, 400, 0); // credit = 1.85
        let req = |id: &str, ts: i64, msgs: &str| format!(r#"{{"id":"{id}","state":"complete","startedAt":{ts},"messages":{msgs},"usage":{usage}}}"#);
        let reqs = [
            req("rA", 1_788_602_400_000, r#"["m1"]"#),
            req("rB", 1_788_602_410_000, r#"["missing","m2h","m2"]"#),
            req("rC", 1_788_602_420_000, r#"["m3"]"#),
            req("rD", 1_788_602_430_000, r#"["m4"]"#),
            req("rE", 1_788_602_440_000, r#"["m5"]"#),
        ];
        std::fs::write(sess.join("index.json"), format!(r#"{{"requests":[{}]}}"#, reqs.join(","))).unwrap();
        let adapter = CodebuddyAdapter { data_dir: dir.join("Data"), meta_dbs: vec![] };

        let mut store = Store::open_in_memory().unwrap();
        let first = adapter.collect(&mut store).map(|o| o.events);
        let turns = store.test_turns(META.id);
        let second = adapter.collect(&mut store).map(|o| o.events);
        let summary = store.credit_summary("2026-09").unwrap();
        let conservation = store.test_project_conservation();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(first.ok(), Some(5));
        let models: Vec<&str> = turns.iter().map(|t| t.model_key.as_str()).collect();
        assert_eq!(models, vec!["glm-5.3-flash", "deepseek-v4.1-flash", "unknown", "unknown", "glm-5.3"]);
        assert_eq!(second.ok(), Some(0));
        assert!((summary.total_credit - 5.0 * 1.85).abs() < 1e-9, "{}", summary.total_credit);
        assert_eq!(summary.total_requests, 5);
        let unknown = summary.by_model.iter().find(|r| r.key == "unknown").unwrap();
        assert!((unknown.credit - 2.0 * 1.85).abs() < 1e-9);
        assert!(conservation.is_empty(), "{conservation:?}");
    }

    #[test]
    fn workspace_fallback_skips_conflicting_dirs() {
        let files = vec![PathBuf::from("h/ws/s1/index.json"), PathBuf::from("h/ws/s2/index.json"), PathBuf::from("h/ws2/s3/index.json")];
        let meta = |p: &str| SessionMeta { project: p.to_string(), title: None };
        let metas = HashMap::from([("s1".to_string(), meta("e:/a")), ("s2".to_string(), meta("e:/b")), ("s3".to_string(), meta("unknown"))]);
        let ws = workspace_projects(&files, &metas);
        assert_eq!(ws.get("ws"), Some(&None));
        assert!(!ws.contains_key("ws2"), "unknown / playground 不作兄弟兜底");
    }

    #[test]
    fn id_deserialized_for_model_join() {
        // id 字段必须进 IndexRequest（模型对账按它查表）;缺失不致命(归 unknown)。
        let with_id: IndexRequest = serde_json::from_str(&request_json(1_757_000_000_000, &usage_json(1, 1, 2, 0, 1, 0))).unwrap();
        assert_eq!(with_id.id, "r1");
        let no_id: IndexRequest = serde_json::from_str(r#"{"startedAt":1757000000000,"usage":{"inputTokens":1}}"#).unwrap();
        assert_eq!(no_id.id, "");
    }
}
