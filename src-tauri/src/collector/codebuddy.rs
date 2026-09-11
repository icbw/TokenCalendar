//! CodeBuddy 适配器。
//!
//! 数据源（Windows,明文 JSON,无需解码）：
//! - `%LOCALAPPDATA%/CodeBuddyExtension/Data/<profile>/CodeBuddyIDE/<profile>/history/
//!   <workspaceHash>/<sessionId>/index.json`——递归扫全部 history 根;
//! - 元数据库 `%APPDATA%/CodeBuddy CN/codebuddy-sessions.vscdb`（cwd/title）
//!   **不接入**：矩阵只按 agent/model 聚合,无需会话元数据。
//!
//! 结构：`requests[]` 追加式（无模型字段!），每条：
//! `usage.{inputTokens, outputTokens, totalTokens, cacheTokens,
//! cachedWriteTokens, cachedMissTokens}`。
//! 口径：`inputTokens = cacheTokens + cachedMiss + cachedWrite`、
//! `total = input + output` → 入库 input = inputTokens − cacheTokens
//! （cache-exclusive,与全源统一）,output 保持 provider 值,total 取 provider total。
//!
//! 增量游标：单文件 JSON（非 JSONL）,offset 不适用——用「已处理 request 条数」：
//! generation 不变跳过;count > len（重写/清空）→ 归零重读。
//! 大体积 messages 由 serde derive 按字段跳过,不构建 Value 树。
//!
//! 模型归属：index.json 无模型字段,按 `requests[].id` 查
//! request_model 对账表（官网导出导入,见 imports.rs）。未命中 → `unknown`
//! （等待下一次导入触发失效重扫后归位）。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::store::{Batch, Store};
use super::{
    Adapter, AdapterError, AdapterMeta, CollectOutcome, CollectResult, ProbeOutcome, clamp0,
    jsonl, millis_to_local_day_hour,
};

pub struct CodebuddyAdapter {
    data_dir: PathBuf,
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
        CodebuddyAdapter { data_dir }
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

/// 游标（count = 已处理的 requests 条数）。
#[derive(serde::Serialize, serde::Deserialize)]
struct IndexCursor {
    count: u64,
    size: u64,
    mtime: i64,
}

impl IndexCursor {
    fn fresh() -> Self {
        IndexCursor { count: 0, size: 0, mtime: 0 }
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
    usage: Option<IndexUsage>,
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
}

/// 从一条 request 提取 （day, hour, input, output, total);usage/time 缺失 → None。
fn parse_request(r: &IndexRequest) -> Option<(String, u8, i64, i64, i64)> {
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
    Some((day, hour, input, output, total))
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

        for path in files {
            let scope = path.display().to_string();
            let mut cursor = store
                .get_cursor(META.id, &scope)
                .and_then(|j| serde_json::from_str::<IndexCursor>(&j).ok())
                .unwrap_or_else(IndexCursor::fresh);
            if cursor.up_to_date(&path) {
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
            // 模型归属：本批新请求的 id → 官网对账表（未命中 = unknown,
            // 等导入触发 invalidate_source 重扫后归位）。
            let new_ids: Vec<String> = file.requests.iter().skip(cursor.count as usize)
                .filter(|r| !r.id.is_empty())
                .map(|r| r.id.clone())
                .collect();
            let model_of = store.request_models(META.id, &new_ids);
            for r in file.requests.iter().skip(cursor.count as usize) {
                if let Some((day, hour, input, output, total)) = parse_request(r) {
                    let model = model_of.get(&r.id).cloned().unwrap_or_else(|| "unknown".into());
                    batch.add_hour(&day, Some(hour), META.id, &model, input, output, total, 1);
                    months.insert(day[..7].to_string());
                }
            }
            cursor.count = file.requests.len() as u64;
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
        let (day, hour, input, output, total) = parse_request(&req).unwrap();
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
        let (_, _, input, output, total) = parse_request(&req).unwrap();
        assert_eq!((input, output, total), (800, 500, 1500));
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
        let (_, _, input, _, total) = parse_request(&file.requests[0]).unwrap();
        assert_eq!((input, total), (10, 15));
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
