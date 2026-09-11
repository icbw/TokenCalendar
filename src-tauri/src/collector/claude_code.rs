//! Claude Code 适配器：`~/.claude/projects/**/*.jsonl` 递归（CLAUDE_CONFIG_DIR 可覆盖）。
//!
//! 口径（跟随旧项目）：仅 `type=="assistant"` 且 `message.usage` 存在的行；
//! input/output 取 `message.usage.input_tokens / output_tokens` 原始值（cache 分项
//! 独立字段,不计入 total）；total = input + output（Anthropic 无 total 字段的旧约定）；
//! 时间取顶层 `timestamp`（RFC3339）→ 本地日。模型缺失填 "unknown"。
//!
//! 对话轮计数：「真实用户输入行」置 pending 标志（type=="user" 且无
//! toolUseResult——工具结果回传行都带它——且非 sidechain/meta）,下一条 assistant
//! usage 行按其模型计 1 turn 并清位;pending 持久化进游标,跨批次/轮次正确。
//! 一个 turn 工具循环产生多条 assistant 行,只计首条。

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

use super::store::{Batch, Store};
use super::{
    Adapter, AdapterError, AdapterMeta, CollectOutcome, CollectResult, FileCursor, ProbeOutcome,
    advance_file, clamp0, load_cursor, rfc3339_to_local_day_hour, seal_cursor,
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

enum ClaudeLine {
    /// 主会话真实用户输入行（开启新 turn)。
    UserInput,
    /// assistant 行带 usage:（本地日, 本地小时, 模型, input, output)。
    Usage { day: String, hour: u8, model: String, input: i64, output: i64 },
    None,
}

impl ClaudeCodeAdapter {
    pub fn new() -> Self {
        let base = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| super::home_dir().map(|h| h.join(".claude")));
        let projects_dir = base.map(|b| b.join("projects")).unwrap_or_default();
        ClaudeCodeAdapter { projects_dir }
    }

    /// 从一行 JSONL 提取：真实用户输入（置 pending)/ assistant usage 行 / 无关。
    fn parse_line(line: &str) -> ClaudeLine {
        let Ok(v) = serde_json::from_str::<Value>(line) else { return ClaudeLine::None };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => {
                // 真实用户输入：无 toolUseResult（工具结果回传行都带)、非 sidechain/meta
                let is_real = !v.get("toolUseResult").map_or(false, |x| !x.is_null())
                    && !v.get("isSidechain").and_then(|x| x.as_bool()).unwrap_or(false)
                    && !v.get("isMeta").and_then(|x| x.as_bool()).unwrap_or(false);
                if is_real { ClaudeLine::UserInput } else { ClaudeLine::None }
            }
            Some("assistant") => {
                let usage = match v.get("message").and_then(|m| m.get("usage")) {
                    Some(u) => u,
                    None => return ClaudeLine::None,
                };
                let input = clamp0(usage.get("input_tokens").and_then(|x| x.as_i64()).unwrap_or(0));
                let output = clamp0(usage.get("output_tokens").and_then(|x| x.as_i64()).unwrap_or(0));
                if input == 0 && output == 0 {
                    return ClaudeLine::None;
                }
                let model = v
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|m| m.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("unknown")
                    .to_string();
                let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) else { return ClaudeLine::None };
                let Some((day, hour)) = rfc3339_to_local_day_hour(ts) else { return ClaudeLine::None };
                ClaudeLine::Usage { day, hour, model, input, output }
            }
            _ => ClaudeLine::None,
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

            for line in &consume.lines {
                match Self::parse_line(line) {
                    ClaudeLine::UserInput => cursor.pending_turn = true,
                    ClaudeLine::Usage { day, hour, model, input, output } => {
                        let turns = if cursor.pending_turn {
                            cursor.pending_turn = false;
                            1
                        } else {
                            0
                        };
                        batch.add_hour(&day, Some(hour), META.id, &model, input, output, input + output, turns);
                        months.insert(day[..7].to_string());
                    }
                    ClaudeLine::None => {}
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

    #[test]
    fn parses_assistant_usage_line() {
        let line = r#"{"type":"assistant","timestamp":"2026-09-05T18:30:00.123Z","message":{"id":"msg_01","model":"claude-sonnet-4-5","usage":{"input_tokens":1234,"output_tokens":567,"cache_read_input_tokens":890,"cache_creation_input_tokens":12}}}"#;
        let ClaudeLine::Usage { day, hour, model, input, output } = ClaudeCodeAdapter::parse_line(line) else {
            panic!("expected usage");
        };
        assert_eq!(input, 1234);
        assert_eq!(output, 567);
        assert_eq!(model, "claude-sonnet-4-5");
        assert_eq!(day.len(), 10);
        assert!(hour <= 23);
    }

    #[test]
    fn real_user_input_vs_tool_result() {
        // 真实用户输入(parentUuid=null,无 toolUseResult)→ UserInput
        assert!(matches!(
            ClaudeCodeAdapter::parse_line(r#"{"type":"user","parentUuid":null,"message":{"role":"user","content":"hi"}}"#),
            ClaudeLine::UserInput
        ));
        // 工具结果回传(带 toolUseResult)→ 忽略
        assert!(matches!(
            ClaudeCodeAdapter::parse_line(r#"{"type":"user","parentUuid":"x","toolUseResult":"Error: exit 2","message":{}}"#),
            ClaudeLine::None
        ));
        // sidechain / meta 行 → 忽略
        assert!(matches!(
            ClaudeCodeAdapter::parse_line(r#"{"type":"user","isSidechain":true,"message":{}}"#),
            ClaudeLine::None
        ));
        assert!(matches!(
            ClaudeCodeAdapter::parse_line(r#"{"type":"user","isMeta":true,"message":{}}"#),
            ClaudeLine::None
        ));
    }

    #[test]
    fn skips_non_assistant_and_usageless_lines() {
        // assistant 但无 usage
        assert!(matches!(
            ClaudeCodeAdapter::parse_line(r#"{"type":"assistant","timestamp":"2026-09-05T18:30:00Z","message":{"model":"m"}}"#),
            ClaudeLine::None
        ));
        // assistant 有 usage 但缺 timestamp
        assert!(matches!(
            ClaudeCodeAdapter::parse_line(r#"{"type":"assistant","message":{"model":"m","usage":{"input_tokens":1,"output_tokens":1}}}"#),
            ClaudeLine::None
        ));
        // 坏 JSON
        assert!(matches!(ClaudeCodeAdapter::parse_line("not json"), ClaudeLine::None));
        // 全零 usage 跳过
        assert!(matches!(
            ClaudeCodeAdapter::parse_line(r#"{"type":"assistant","timestamp":"2026-09-05T18:30:00Z","message":{"model":"m","usage":{"input_tokens":0,"output_tokens":0}}}"#),
            ClaudeLine::None
        ));
    }

    #[test]
    fn missing_model_falls_back_to_unknown() {
        let line = r#"{"type":"assistant","timestamp":"2026-09-05T18:30:00Z","message":{"usage":{"input_tokens":5,"output_tokens":1}}}"#;
        let ClaudeLine::Usage { model, .. } = ClaudeCodeAdapter::parse_line(line) else {
            panic!("expected usage");
        };
        assert_eq!(model, "unknown");
    }
}
