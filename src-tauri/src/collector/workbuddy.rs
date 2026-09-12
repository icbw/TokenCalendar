//! WorkBuddy 适配器：`~/.workbuddy/projects/**/*.jsonl` 递归（provider_reported 主源）。
//!
//! 口径：
//! - 仅 `type=="function_call"` 且 `providerData.rawUsage` 存在的行；
//! - cache_read = `prompt_cache_hit_tokens`（clamp ≥0）；input = prompt_tokens - cache_read
//!   （prompt 已含 cache hit,拆出 cache-exclusive input）；output = completion_tokens；
//! - total = `rawUsage.total_tokens`（含 cache 与 reasoning,不重复加总）,≤0 时回退 input+output；
//! - 时间戳：顶层 `timestamp` 数字,> 1e10 视为毫秒否则秒（旧口径）；
//! - 模型：`providerData.requestModelId` 优先 → `providerData.model` → "unknown"。
//!
//! 对话轮计数：`type=="message" && role=="user"` 是真实用户输入（function_call
//! 是模型发起的**工具调用**,不是对话,勿计入）→ 置 pending 标志,下一条带 rawUsage
//! 的 function_call 行按其模型计 1 turn 并清位;pending 持久化进游标。
//!
//! db（session 级 estimated 口径）与 traces 兜底源不接入——避免低质量数据混入
//! 主指标。

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

use super::store::{Batch, Store};
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
    /// 主会话真实用户输入行（message role=user,开启新 turn)。
    UserInput,
    /// function_call 行带 rawUsage:（本地日, 本地小时, 模型, input, output, total)。
    Usage { day: String, hour: u8, model: String, input: i64, output: i64, total: i64 },
    None,
}

impl WorkBuddyAdapter {
    pub fn new() -> Self {
        let projects_dir = super::home_dir()
            .map(|h| h.join(".workbuddy").join("projects"))
            .unwrap_or_default();
        WorkBuddyAdapter { projects_dir }
    }

    /// 从一行提取：真实用户输入（置 pending)/ function_call usage 行 / 无关。
    fn parse_line(line: &str) -> WbLine {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return WbLine::None };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("message") => {
                // role=="user" = 真实用户输入（assistant 的 message 行是回复文本,不带 usage)
                if v.get("role").and_then(|r| r.as_str()) == Some("user") {
                    WbLine::UserInput
                } else {
                    WbLine::None
                }
            }
            Some("function_call") => {
                let Some((day, hour, model, input, output, total)) = Self::parse_usage(&v) else {
                    return WbLine::None;
                };
                WbLine::Usage { day, hour, model, input, output, total }
            }
            _ => WbLine::None,
        }
    }

    /// function_call 行的 usage 提取（原 parse_line 主体)。
    fn parse_usage(v: &Value) -> Option<(String, u8, String, i64, i64, i64)> {
        let raw = v.get("providerData")?.get("rawUsage")?;
        let prompt = clamp0(raw.get("prompt_tokens").and_then(|x| x.as_i64()).unwrap_or(0));
        let completion = clamp0(raw.get("completion_tokens").and_then(|x| x.as_i64()).unwrap_or(0));
        let cache_read = clamp0(raw.get("prompt_cache_hit_tokens").and_then(|x| x.as_i64()).unwrap_or(0));
        let input = (prompt - cache_read).max(0);
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
        Some((day, hour, model, input, completion, total))
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

        for path in files {
            let scope = path.display().to_string();
            let mut cursor = load_cursor(store, META.id, &scope);
            let Some(consume) = advance_file(&path, &mut cursor) else { continue };
            let mut cursor = if consume.reset { FileCursor::fresh() } else { cursor };

            for line in &consume.lines {
                match Self::parse_line(line) {
                    WbLine::UserInput => cursor.pending_turn = true,
                    WbLine::Usage { day, hour, model, input, output, total } => {
                        let turns = if cursor.pending_turn {
                            cursor.pending_turn = false;
                            1
                        } else {
                            0
                        };
                        batch.add_hour(&day, Some(hour), META.id, &model, input, output, total, turns);
                        months.insert(day[..7].to_string());
                    }
                    WbLine::None => {}
                }
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
            WbLine::Usage { day, hour, model, input, output, total } => (day, hour, model, input, output, total),
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
        // assistant 的 message 行 → 忽略
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
    fn request_model_id_takes_priority() {
        let (_, _, model, _, _, _) =
            expect_usage(&line(1_757_000_000_000.0, Some("req-model"), "raw-model", 10, 5, 0, 15));
        assert_eq!(model, "req-model");
    }
}
