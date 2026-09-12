//! Codex 适配器：`~/.codex/sessions/**/*.jsonl` 递归 + `~/.codex/archived_sessions/*.jsonl`
//! （CODEX_HOME 可覆盖）。
//!
//! 本机格式（2026-09，cli 0.145+）：
//! - 模型在 `turn_context` 行的 `payload.model`（turn 级设置，token_count 行不带）；
//! - 用量在 token_count 行 `payload.info`：`last_token_usage` = **单次调用值**（优先，
//!   无需差分）；`total_token_usage` = 会话内累积快照（备用）。
//! - 旧格式兼容：顶层 `info.total_token_usage`（旧 Go 适配器的路径）→ 走差分 +
//!   游标持久化基线（旧 Go「续读基线归零重计」缺陷以持久化基线复）。
//!
//! 对话轮计数：`event_msg/user_message` 是专用的用户输入事件（`token_count`
//! 是 API 回合级,一轮工具循环多条,不能当对话数）→ 置 pending 标志,下一条
//! token_count 行按其模型计 1 turn 并清位;pending 持久化进游标。
//!
//! 口径（本机：total=19914 = input 19139（含 cached 11008) + output 775（含
//! reasoning 397)）：input = raw.input - cached（cache-exclusive）；output 保持
//! provider 口径（含 reasoning）；total = raw.total_tokens（回退 input+output）。

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

use super::store::{Batch, Store};
use super::{
    Adapter, AdapterError, AdapterMeta, CollectOutcome, CollectResult, FileCursor, ProbeOutcome,
    advance_file, clamp0, load_cursor, rfc3339_to_local_day_hour, seal_cursor,
};

pub struct CodexAdapter {
    sessions_dir: PathBuf,
    archived_dir: PathBuf,
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
        reason: get("reasoning_output_tokens"),
        total: get("total_tokens"),
    }
}

enum Parsed {
    /// token_count 行：（本地日, 本地小时, 单次快照（若有), 累积快照（若有), 模型)。
    Usage { day: String, hour: u8, last: Option<Snapshot>, total: Option<Snapshot>, model: String },
    /// 只有模型信息（turn_context），无用量。
    ModelOnly,
    /// 用户输入事件（event_msg/user_message,开启新 turn）。
    UserInput,
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
        }
    }

    /// 从一行提取模型更新与用量。info 路径双兼容：`payload.info`（新）/
    /// 顶层 `info`（旧 Go 格式）。
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

        // 用户输入事件（专用类型,优先于 usage 提取）
        if payload.and_then(|p| p.get("type")).and_then(|t| t.as_str()) == Some("user_message") {
            return Some(Parsed::UserInput);
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
        let ts = v.get("timestamp")?.as_str()?;
        let (day, hour) = rfc3339_to_local_day_hour(ts)?;
        Some(Parsed::Usage { day, hour, last, total, model: model() })
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

        for path in files {
            let scope = path.display().to_string();
            let mut cursor = load_cursor(store, META.id, &scope);
            let Some(consume) = advance_file(&path, &mut cursor) else { continue };
            if consume.reset {
                cursor = FileCursor::fresh(); // 重写：基线清零重读（已聚合不回滚）
            }

            for line in &consume.lines {
                let parsed = match Self::parse_line(line, &mut cursor.model) {
                    Some(Parsed::UserInput) => {
                        cursor.pending_turn = true;
                        continue;
                    }
                    other => other,
                };
                let Some(Parsed::Usage { day, hour, last, total, model }) = parsed else { continue };
                let turns = if cursor.pending_turn {
                    cursor.pending_turn = false;
                    1
                } else {
                    0
                };

                let (input, output, total_tokens) = if let Some(last) = last {
                    // 单次值路径（新格式）：无差分、无回退风险
                    let cached = last.cached.min(last.input);
                    let input = (last.input - cached).max(0);
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
                    (input, output, t)
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
                    (input, output, t)
                };

                batch.add_hour(&day, Some(hour), META.id, &model, input, output, total_tokens, turns);
                months.insert(day[..7].to_string());
            }
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
        assert!(matches!(CodexAdapter::parse_line(&turn_context_line("gpt-5.6-sol"), &mut fm), Some(Parsed::ModelOnly)));
        assert_eq!(fm, "gpt-5.6-sol");

        // 首行快照：in=19139 cached=11008 out=775 reason=397 total=19914
        let Some(Parsed::Usage { day, hour, last, total, model }) =
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
    fn user_message_event_marks_turn() {
        let mut fm = String::new();
        assert!(matches!(
            CodexAdapter::parse_line(
                r#"{"timestamp":"2026-09-05T10:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"hi"}}"#,
                &mut fm
            ),
            Some(Parsed::UserInput)
        ));
        // response_item/message role=user 是环境上下文注入,不算用户输入事件
        assert!(matches!(
            CodexAdapter::parse_line(
                r#"{"timestamp":"2026-09-05T10:00:00Z","type":"response_item","payload":{"type":"message","role":"developer","content":[]}}"#,
                &mut fm
            ),
            Some(Parsed::ModelOnly) | None
        ));
    }

    #[test]
    fn row_without_usage_skipped() {
        let mut fm = String::new();
        // 无 info 的行 → ModelOnly(无用量产出)
        assert!(matches!(
            CodexAdapter::parse_line(r#"{"timestamp":"2026-09-05T10:00:00Z","type":"session_meta","payload":{}}"#, &mut fm),
            Some(Parsed::ModelOnly)
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
