//! 只读查询面（命令层 `get_project_* / get_task_* / get_effort_series / get_gap_histogram`
//! 的存储侧）。只读 `session` / `turn` / `daily_project`,与 store.rs 的用量读入口并列,不改其 SQL。
//!
//! 口径：
//! - 项目维矩阵 / 钻取 / 时间成本曲线读 `daily_project`（已套离开阈值;token / turns 与 daily_usage
//!   逐格守恒）。metric = total / input / output / cache_read / cache_write（token）| turns | model_calls | tool_calls |
//!   wait（Σ wall_ms,只算根会话）| human（Σ idle_ms,只算根会话）。
//! - 任务列表 / 逐轮明细读物化层 `turn`（只含根会话,子会话已并入父轮）;任务 = 有轮的根会话,
//!   `turns` = 物化轮行数（含零调用轮）,`steps` = Σ model_calls。
//! - 空档直方图读 `turn.gap_ms`（原始值）,within = gap ≤ 阈值（与 daily_project.idle_ms 同判据）。
//! - 数据跨度（时间过滤用）:单个项目 = daily_project 中该键的首末日（项目生命周期,
//!   按轮的本地日）;不指定项目 = daily_usage ∪ daily_project 的首末日（「All」范围起点）。
//! - 中止与错误分列:TaskRow.aborted_count / TaskTurn.aborted 与 error_count 互不计入。
//! - 项目管理解析层:项目维（group_by / 切片 / 筛选 = project）一律经
//!   `project_meta:resolve_cte` 的 `pmap（raw_key, eff_key)`:合并归目标、短会话折叠进 `__scratch`、
//!   隐藏的原始键不出现;显示名 alias 优先。agent / model / total 维不经解析层,数字不受影响。
//! - `title` 是内容列（store:CONTENT_COLUMNS）:只随 IPC 回 UI 做本地可视化,本模块类型
//!   **禁止**进入任何导出 / 文件序列化路径。

use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::store::{
    agent_label, days_in_month, model_label, parse_month, RangeSeries, RangeSeriesPoint, Store, StoreBreakdownDay,
    StoreRow, StoreSlice,
};
use super::project_meta::{project_labels, resolve_cte, ScratchRule, HIDDEN_SLICE_KEY};

/// 项目维 metric → daily_project 列（白名单;SQL 只拼这里的常量）。
pub fn project_metric_col(metric: &str) -> Option<&'static str> {
    Some(match metric {
        "total" => "total_tokens",
        "input" => "input_tokens",
        "output" => "output_tokens",
        "cache_read" => "cache_read_tokens",
        "cache_write" => "cache_write_tokens",
        "turns" => "turns",
        "model_calls" => "model_calls",
        "tool_calls" => "tool_calls",
        "wait" => "wall_ms",
        "human" => "idle_ms",
        _ => return None,
    })
}

fn dim_col(dim: &str) -> Option<&'static str> {
    Some(match dim {
        "project" => "project_key",
        "agent" => "agent_key",
        "model" => "model_key",
        _ => return None,
    })
}

/// 一组 key 的展示名:project 维 alias 优先,无 alias 的同名不同路径退回完整路径。
fn labels_for(dim: &str, keys: &[String], aliases: &HashMap<String, String>) -> Vec<String> {
    match dim {
        "agent" => keys.iter().map(|k| agent_label(k)).collect(),
        "model" => keys.iter().map(|k| model_label(k)).collect(),
        "project" => project_labels(keys, aliases),
        _ => keys.to_vec(),
    }
}

// ---------- 任务列表契约（serde 形状直供命令层） ----------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TaskFilters {
    pub agent: Option<String>,
    pub project: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct TaskSort {
    /// started_at | turns | steps | tool_calls | wall_ms | model_ms | tool_ms | error_count |
    /// subagent_count | subagent_calls | total_tokens
    pub field: String,
    /// asc | desc
    pub direction: String,
}

impl Default for TaskSort {
    fn default() -> Self {
        TaskSort { field: "started_at".into(), direction: "desc".into() }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct TaskPageReq {
    pub offset: i64,
    pub limit: i64,
}

impl Default for TaskPageReq {
    fn default() -> Self {
        TaskPageReq { offset: 0, limit: 50 }
    }
}

pub const TASK_PAGE_MAX: i64 = 500;

#[derive(Debug, Clone, Serialize)]
pub struct TaskRow {
    pub agent: String,
    pub session_id: String,
    /// 解析后的有效项目键（合并目标 / `__scratch` / 原键）。
    pub project: String,
    /// 有效项目的展示名（alias 优先）。
    pub project_label: String,
    /// 会话首轮的原始目录键（hover 显示真实路径）。
    pub project_raw: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// 【内容列】仅本地可视化,禁止进入导出 / 文件序列化。
    pub title: Option<String>,
    pub turns: i64,
    /// = model_calls（含子代理）。
    pub steps: i64,
    pub tool_calls: i64,
    /// 源无该值时 null（CodeBuddy）;JSONL 族为估算。
    pub wall_ms: Option<i64>,
    pub model_ms: Option<i64>,
    pub tool_ms: Option<i64>,
    /// API / 工具错误（不含用户中止）。
    pub error_count: i64,
    /// 用户中止的轮数。
    pub aborted_count: i64,
    pub subagent_count: i64,
    pub subagent_calls: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Serialize)]
pub struct TaskPage {
    /// 过滤后的任务总数（分页前）。
    pub total: i64,
    pub rows: Vec<TaskRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskTurn {
    /// 按开始时间 1..n。
    pub turn_seq: i64,
    pub day: String,
    pub project: String,
    pub model: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub wall_ms: Option<i64>,
    pub model_ms: Option<i64>,
    pub tool_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub gap_ms: Option<i64>,
    pub steps: i64,
    pub tool_calls: i64,
    pub subagent_count: i64,
    pub subagent_calls: i64,
    pub error_count: i64,
    pub retry_count: i64,
    /// 用户中止（与 error_count 分列）。
    pub aborted: bool,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

/// 本地日闭区间（数据跨度 / 项目生命周期）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DaySpan {
    pub first_day: String,
    pub last_day: String,
}

// ---------- 项目推进时间轴契约（snake_case 直供命令层） ----------

/// Project × Day 看板的一格：当日该有效项目的活动摘要（只含有活动的日,未来日恒不出现）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimelineCell {
    pub day: String,
    pub turns: i64,
    pub tokens: i64,
    pub wall_ms: i64,
    pub idle_ms: i64,
    /// 当日有轮的根会话数（= items.len;物化层 `turn`,子会话已并入父轮）。
    pub sessions: i64,
    /// 当日出现的 agent 展示名（去重,按键排序）。
    pub agents: Vec<String>,
    /// 当日各根会话（**最新活动的在前**,前端纵向视图按会话向下拆格;横向取首条 + `+N`）。
    pub items: Vec<TimelineSession>,
}

/// 格子内的一条会话。`title` 是内容列,仅本地可视化,禁止进入导出。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimelineSession {
    pub agent: String,
    /// agent 键（双击跳转 `open_agent_session` 的入参;展示一律用 `agent`）。
    pub agent_key: String,
    pub session_id: String,
    /// 空 / NULL → 前端回退到 `started_at` 的时刻（与 Tasks 视图 `LabelMode='title'` 同口径）。
    pub title: Option<String>,
    /// 该会话当日首轮开始时刻（Unix 毫秒）。
    pub started_at: i64,
    /// 该会话当日最后活动时刻（末轮 ended_at,缺失时取 started_at）。排序依据是「最新活动的
    /// 会话」而不是「最新创建的会话」,与 Claude app 新消息置顶一致。
    pub last_active_at: i64,
    pub turns: i64,
    pub tokens: i64,
    pub wall_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimelineProject {
    /// 有效项目键（合并目标 / `__scratch` / 原键）。
    pub key: String,
    /// alias 优先;无 alias 的同名末段退回完整路径（与 Matrix 项目维同口径）。
    pub label: String,
    /// 该项目全生命周期出现过的 agent 展示名。
    pub agents: Vec<String>,
    /// 项目生命周期（daily_project 全量首末日,**不受 from / to 限制**）。
    pub first_day: Option<String>,
    pub last_day: Option<String>,
    /// today − last_day（天;last_day 在未来或缺失 → None）。
    pub inactive_days: Option<i64>,
    pub cells: Vec<TimelineCell>,
}

#[derive(Debug, Serialize)]
pub struct TimelineResult {
    pub today: String,
    /// from..to 逐日全列（含未来日）。
    pub days: Vec<String>,
    /// 可见项目,按 last_day 倒序（不裁剪数量——前端按 pin + 窗口容量裁）。
    pub projects: Vec<TimelineProject>,
}

/// `get_project_timeline` 日跨度上限（含端点;远大于看板实际跨度,只防误用）。
pub const TIMELINE_DAYS_MAX: usize = 366;

#[derive(Debug, Clone, Serialize)]
pub struct GapBucket {
    pub lo_ms: i64,
    /// None = 开放上界（最后一桶）。
    pub hi_ms: Option<i64>,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct GapHistogram {
    pub threshold_ms: i64,
    pub buckets: Vec<GapBucket>,
    /// 有 gap 的轮数（首轮 gap 为 NULL,不计）。
    pub total: i64,
    /// gap ≤ 阈值:计入 idle 的轮数与毫秒和（= 范围内 Σ daily_project.idle_ms）。
    pub within_count: i64,
    pub within_ms: i64,
    /// gap > 阈值:视为离开、不计 idle。
    pub beyond_count: i64,
    pub beyond_ms: i64,
}

/// 对数分桶边界（毫秒）:1s × 10^（k/4),k = 0..=24（1 秒 → 约 11.6 天,每十倍 4 桶）。
/// 桶 = [0, 1s)、[e_k, e_{k+1})…、[e_24, ∞),共 26 个。
pub fn gap_bucket_edges() -> Vec<i64> {
    (0..=24).map(|k| (1000.0 * 10f64.powf(k as f64 / 4.0)).round() as i64).collect()
}

fn sort_col(field: &str) -> Option<&'static str> {
    Some(match field {
        "started_at" => "started_at",
        "turns" => "turns",
        "steps" => "steps",
        "tool_calls" => "tool_calls",
        "wall_ms" => "wall_ms",
        "model_ms" => "model_ms",
        "tool_ms" => "tool_ms",
        "error_count" => "error_count",
        "aborted_count" => "aborted_count",
        "subagent_count" => "subagent_count",
        "subagent_calls" => "subagent_calls",
        "total_tokens" => "total_tokens",
        _ => return None,
    })
}

fn day_axis(start_day: &str, end_day: &str) -> Option<Vec<String>> {
    let start = NaiveDate::parse_from_str(start_day, "%Y-%m-%d").ok()?;
    let end = NaiveDate::parse_from_str(end_day, "%Y-%m-%d").ok()?;
    let mut out = Vec::new();
    let mut cur = start;
    while cur <= end {
        out.push(cur.format("%Y-%m-%d").to_string());
        cur += chrono::Duration::days(1);
    }
    Some(out)
}

impl Store {
    /// 项目维月度矩阵（形状同 `month_rows`）:group_by = project | agent | model;metric 见模块头。
    /// 键集 = 月内 daily_project 有记录的 key;过去无记录日 Some（0)、未来 None;
    /// message_counts = Σ turns;行按月合计降序。
    pub fn project_month_rows(&self, month: &str, group_by: &str, metric: &str, today: NaiveDate, rule: &ScratchRule) -> Option<Vec<StoreRow>> {
        let (y, m) = parse_month(month)?;
        let first = NaiveDate::from_ymd_opt(y, m, 1)?;
        let dim = days_in_month(y, m) as usize;
        let metric_col = project_metric_col(metric)?;
        let key_col = dim_col(group_by)?;
        let sql = if group_by == "project" {
            format!(
                "WITH {} SELECT p.eff_key AS k, substr(d.day, 9) AS dd, SUM(d.{metric_col}) AS v, SUM(d.turns) AS rc
                 FROM daily_project d JOIN pmap p ON p.raw_key = d.project_key WHERE d.day LIKE ?1 GROUP BY k, dd",
                resolve_cte(rule)
            )
        } else {
            format!(
                "SELECT {key_col} AS k, substr(day, 9) AS d, SUM({metric_col}) AS v, SUM(turns) AS rc
                 FROM daily_project WHERE day LIKE ?1 GROUP BY k, d"
            )
        };
        let mut cells: BTreeMap<String, Vec<(usize, i64, i64)>> = BTreeMap::new();
        let mut stmt = self.conn().prepare(&sql).ok()?;
        let rows = stmt
            .query_map([format!("{month}-%")], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
            })
            .ok()?;
        for (k, d, v, rc) in rows.flatten() {
            if let Ok(n) = d.parse::<usize>() {
                if (1..=dim).contains(&n) {
                    cells.entry(k).or_default().push((n - 1, v, rc));
                }
            }
        }
        drop(stmt);
        let keys: Vec<String> = cells.keys().cloned().collect();
        let labels = labels_for(group_by, &keys, &self.project_aliases());
        let mut out: Vec<StoreRow> = keys
            .into_iter()
            .zip(labels)
            .map(|(k, label)| {
                let mut values: Vec<Option<i64>> =
                    (0..dim).map(|i| if first + chrono::Duration::days(i as i64) > today { None } else { Some(0) }).collect();
                let mut message_counts = vec![0i64; dim];
                let mut month_total = 0;
                for (i, v, rc) in cells.remove(&k).unwrap_or_default() {
                    values[i] = Some(v);
                    message_counts[i] = rc;
                    month_total += v;
                }
                StoreRow { key: k, label, values, message_counts, month_total }
            })
            .collect();
        out.sort_by(|a, b| b.month_total.cmp(&a.month_total).then(a.key.cmp(&b.key)));
        Some(out)
    }

    /// 项目维行钻取（形状同 `breakdown`,tokens = total_tokens）:kind = project（key = 有效项目键）→
    /// 每日 Agent 构成;kind = agent | model → 每日项目构成（有效键;隐藏项目归 `__hidden` 切片,切片和 = 格值）。
    /// 未来日不产出。
    pub fn project_breakdown(&self, kind: &str, key: &str, month: &str, today: NaiveDate, rule: &ScratchRule) -> Option<Vec<StoreBreakdownDay>> {
        let (y, m) = parse_month(month)?;
        let first = NaiveDate::from_ymd_opt(y, m, 1)?;
        let cte = resolve_cte(rule);
        let (sql, slice_dim) = match kind {
            "project" => (
                format!(
                    "WITH {cte} SELECT d.day, d.agent_key AS k, SUM(d.total_tokens) AS v
                     FROM daily_project d JOIN pmap p ON p.raw_key = d.project_key
                     WHERE p.eff_key = ?1 AND d.day LIKE ?2 GROUP BY d.day, k"
                ),
                "agent",
            ),
            "agent" | "model" => (
                format!(
                    "WITH {cte} SELECT d.day, COALESCE(p.eff_key, '{HIDDEN_SLICE_KEY}') AS k, SUM(d.total_tokens) AS v
                     FROM daily_project d LEFT JOIN pmap p ON p.raw_key = d.project_key
                     WHERE d.{} = ?1 AND d.day LIKE ?2 GROUP BY d.day, k",
                    dim_col(kind)?
                ),
                "project",
            ),
            _ => return None,
        };
        let aliases = self.project_aliases();
        let mut per_day: BTreeMap<String, Vec<(String, i64)>> = BTreeMap::new();
        let mut stmt = self.conn().prepare(&sql).ok()?;
        let rows = stmt
            .query_map(rusqlite::params![key, format!("{month}-%")], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .ok()?;
        for (day, k, v) in rows.flatten() {
            if v > 0 {
                per_day.entry(day).or_default().push((k, v));
            }
        }
        drop(stmt);
        let mut out = Vec::new();
        for d in 0..days_in_month(y, m) {
            let date = first + chrono::Duration::days(d as i64);
            if date > today {
                break;
            }
            let day = date.format("%Y-%m-%d").to_string();
            let cells = per_day.remove(&day).unwrap_or_default();
            let keys: Vec<String> = cells.iter().map(|(k, _)| k.clone()).collect();
            let mut slices: Vec<StoreSlice> = cells
                .into_iter()
                .zip(labels_for(slice_dim, &keys, &aliases))
                .map(|((k, v), label)| StoreSlice { key: k, label, tokens: v })
                .collect();
            slices.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.key.cmp(&b.key)));
            out.push(StoreBreakdownDay { day, slices: if slices.is_empty() { None } else { Some(slices) } });
        }
        Some(out)
    }

    /// 时间成本曲线（形状同 `range_series`,读 daily_project）。bucket 仅 day（turn 按日切分,
    /// 不入小时表）;dimension = agent | model | project | total;filter 维 = agent | model | project。
    pub fn effort_series(
        &self,
        start_day: &str,
        end_day: &str,
        bucket: &str,
        dimension: &str,
        metric: &str,
        filter: Option<(&str, &str)>,
        rule: &ScratchRule,
    ) -> Option<RangeSeries> {
        if bucket != "day" {
            return None;
        }
        let metric_col = project_metric_col(metric)?;
        // project 维（系列或筛选）经解析层:键 = 有效项目键,隐藏项目不参与
        let resolved = dimension == "project" || filter.is_some_and(|(d, _)| d == "project");
        let col = |d: &str| -> Option<String> {
            Some(if d == "project" && resolved { "p.eff_key".to_string() } else { format!("d.{}", dim_col(d)?) })
        };
        let key_col = match dimension {
            "total" => "'__total__'".to_string(),
            d => col(d)?,
        };
        let axis = day_axis(start_day, end_day)?;
        let (with, join) = if resolved {
            (format!("WITH {} ", resolve_cte(rule)), " JOIN pmap p ON p.raw_key = d.project_key")
        } else {
            (String::new(), "")
        };
        let mut sql = format!(
            "{with}SELECT {key_col} AS k, d.day AS day, SUM(d.{metric_col}) AS v FROM daily_project d{join}
             WHERE d.day >= ?1 AND d.day <= ?2"
        );
        let mut params = vec![start_day.to_string(), end_day.to_string()];
        if let Some((fdim, fkey)) = filter {
            sql.push_str(&format!(" AND {} = ?3", col(fdim)?));
            params.push(fkey.to_string());
        }
        sql.push_str(" GROUP BY k, d.day");
        let mut per_series: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
        let mut stmt = self.conn().prepare(&sql).ok()?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .ok()?;
        for (k, day, v) in rows.flatten() {
            if v > 0 {
                *per_series.entry(k).or_default().entry(day).or_default() += v;
            }
        }
        drop(stmt);
        let mut series: Vec<(String, i64)> = per_series.iter().map(|(k, c)| (k.clone(), c.values().sum())).collect();
        series.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        if dimension == "total" {
            series = vec![("__total__".to_string(), 0)];
        }
        let series_keys: Vec<String> = series.into_iter().map(|(k, _)| k).collect();
        let series_labels =
            if dimension == "total" { vec!["全部".to_string()] } else { labels_for(dimension, &series_keys, &self.project_aliases()) };
        let points = axis
            .into_iter()
            .map(|b| {
                let values = series_keys
                    .iter()
                    .map(|k| per_series.get(k).and_then(|c| c.get(&b)).copied().unwrap_or(0))
                    .collect();
                RangeSeriesPoint { bucket: b, values }
            })
            .collect();
        Some(RangeSeries { series_keys, series_labels, points })
    }

    /// 任务列表:有轮的根会话,`session.started_at ∈ [start_ms, end_ms)`;过滤 / 排序 / 分页在 SQL 做。
    /// 未知排序字段 → None。
    /// 项目经解析层:隐藏项目的任务不出现;project 筛选 = 有效项目键（合并目标 / `__scratch`）。
    pub fn task_list(&self, start_ms: i64, end_ms: i64, filters: &TaskFilters, sort: &TaskSort, page: &TaskPageReq, rule: &ScratchRule) -> Option<TaskPage> {
        let col = sort_col(&sort.field)?;
        let dir = if sort.direction.eq_ignore_ascii_case("asc") { "ASC" } else { "DESC" };
        let cte = resolve_cte(rule);
        let mut where_sql = String::from(
            "s.parent_id IS NULL AND s.started_at >= ?1 AND s.started_at < ?2
             AND EXISTS (SELECT 1 FROM turn t WHERE t.agent_key = s.agent_key AND t.session_id = s.session_id)",
        );
        let mut params: Vec<rusqlite::types::Value> = vec![start_ms.into(), end_ms.into()];
        if let Some(agent) = filters.agent.as_ref().filter(|a| !a.is_empty()) {
            params.push(agent.clone().into());
            where_sql.push_str(&format!(" AND s.agent_key = ?{}", params.len()));
        }
        if let Some(project) = filters.project.as_ref().filter(|p| !p.is_empty()) {
            params.push(project.clone().into());
            where_sql.push_str(&format!(" AND p.eff_key = ?{}", params.len()));
        }
        let total: i64 = self
            .conn()
            .query_row(
                &format!("WITH {cte} SELECT COUNT(*) FROM session s JOIN pmap p ON p.raw_key = s.project_key WHERE {where_sql}"),
                rusqlite::params_from_iter(params.iter()),
                |r| r.get(0),
            )
            .ok()?;
        let limit = page.limit.clamp(1, TASK_PAGE_MAX);
        let offset = page.offset.max(0);
        params.push(limit.into());
        params.push(offset.into());
        let sql = format!(
            "WITH {cte}
             SELECT s.agent_key, s.session_id, p.eff_key, s.started_at AS started_at, s.ended_at, s.title,
                    agg.turns AS turns, agg.steps AS steps, agg.tool_calls AS tool_calls, agg.wall_ms AS wall_ms, agg.model_ms AS model_ms,
                    agg.tool_ms AS tool_ms, agg.error_count AS error_count, s.subagent_count AS subagent_count,
                    s.subagent_calls AS subagent_calls, agg.total_tokens AS total_tokens, agg.aborted_count AS aborted_count,
                    s.project_key
             FROM session s
             JOIN pmap p ON p.raw_key = s.project_key
             JOIN (SELECT agent_key, session_id, COUNT(*) AS turns, SUM(model_calls) AS steps, SUM(tool_calls) AS tool_calls,
                          SUM(wall_ms) AS wall_ms, SUM(model_ms) AS model_ms, SUM(tool_ms) AS tool_ms,
                          SUM(error_count) AS error_count, SUM(total_tokens) AS total_tokens, SUM(aborted) AS aborted_count
                   FROM turn GROUP BY agent_key, session_id) agg
               ON agg.agent_key = s.agent_key AND agg.session_id = s.session_id
             WHERE {where_sql}
             ORDER BY {col} {dir}, s.started_at DESC, s.agent_key, s.session_id
             LIMIT ?{} OFFSET ?{}",
            params.len() - 1,
            params.len()
        );
        let mut stmt = self.conn().prepare(&sql).ok()?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                Ok(TaskRow {
                    agent: r.get(0)?,
                    session_id: r.get(1)?,
                    project: r.get(2)?,
                    project_label: String::new(),
                    project_raw: r.get(17)?,
                    started_at: r.get(3)?,
                    ended_at: r.get(4)?,
                    title: r.get(5)?,
                    turns: r.get(6)?,
                    steps: r.get(7)?,
                    tool_calls: r.get(8)?,
                    wall_ms: r.get(9)?,
                    model_ms: r.get(10)?,
                    tool_ms: r.get(11)?,
                    error_count: r.get(12)?,
                    subagent_count: r.get(13)?,
                    subagent_calls: r.get(14)?,
                    total_tokens: r.get(15)?,
                    aborted_count: r.get(16)?,
                })
            })
            .ok()?;
        let mut rows: Vec<TaskRow> = rows.flatten().collect();
        drop(stmt);
        let mut keys: Vec<String> = rows.iter().map(|r| r.project.clone()).collect();
        keys.sort();
        keys.dedup();
        let labels: HashMap<String, String> = keys.iter().cloned().zip(project_labels(&keys, &self.project_aliases())).collect();
        for r in &mut rows {
            r.project_label = labels[&r.project].clone();
        }
        Some(TaskPage { total, rows })
    }

    /// 单会话逐轮明细（物化层,按开始时间重排 1..n）。子会话 / 不存在的会话 → 空。
    pub fn task_turns(&self, agent: &str, session_id: &str) -> Result<Vec<TaskTurn>, String> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT day, project_key, model_key, started_at, ended_at, wall_ms, model_ms, tool_ms, ttft_ms, gap_ms,
                        model_calls, tool_calls, subagent_count, subagent_calls, error_count, retry_count,
                        input_tokens, output_tokens, total_tokens, aborted
                 FROM turn WHERE agent_key = ?1 AND session_id = ?2 ORDER BY started_at, turn_seq",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([agent, session_id], |r| {
                Ok(TaskTurn {
                    turn_seq: 0,
                    day: r.get(0)?,
                    project: r.get(1)?,
                    model: r.get(2)?,
                    started_at: r.get(3)?,
                    ended_at: r.get(4)?,
                    wall_ms: r.get(5)?,
                    model_ms: r.get(6)?,
                    tool_ms: r.get(7)?,
                    ttft_ms: r.get(8)?,
                    gap_ms: r.get(9)?,
                    steps: r.get(10)?,
                    tool_calls: r.get(11)?,
                    subagent_count: r.get(12)?,
                    subagent_calls: r.get(13)?,
                    error_count: r.get(14)?,
                    retry_count: r.get(15)?,
                    input_tokens: r.get(16)?,
                    output_tokens: r.get(17)?,
                    total_tokens: r.get(18)?,
                    aborted: r.get::<_, i64>(19)? != 0,
                })
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.flatten().enumerate().map(|(i, t)| TaskTurn { turn_seq: i as i64 + 1, ..t }).collect())
    }

    /// 空档直方图:范围内（按轮的日）根会话轮的 gap_ms 对数分桶 + 阈值两侧合计;隐藏项目的轮不计。
    /// 日期非法 → None。
    pub fn gap_histogram(&self, start_day: &str, end_day: &str, threshold_ms: i64, rule: &ScratchRule) -> Option<GapHistogram> {
        NaiveDate::parse_from_str(start_day, "%Y-%m-%d").ok()?;
        NaiveDate::parse_from_str(end_day, "%Y-%m-%d").ok()?;
        let edges = gap_bucket_edges();
        let mut buckets: Vec<GapBucket> = std::iter::once(GapBucket { lo_ms: 0, hi_ms: Some(edges[0]), count: 0 })
            .chain(edges.iter().enumerate().map(|(i, lo)| GapBucket { lo_ms: *lo, hi_ms: edges.get(i + 1).copied(), count: 0 }))
            .collect();
        let mut h = GapHistogram { threshold_ms, buckets: Vec::new(), total: 0, within_count: 0, within_ms: 0, beyond_count: 0, beyond_ms: 0 };
        let mut stmt = self
            .conn()
            .prepare(&format!(
                "WITH {} SELECT t.gap_ms FROM turn t JOIN pmap p ON p.raw_key = t.project_key
                 WHERE t.day >= ?1 AND t.day <= ?2 AND t.gap_ms IS NOT NULL",
                resolve_cte(rule)
            ))
            .ok()?;
        let rows = stmt.query_map([start_day, end_day], |r| r.get::<_, i64>(0)).ok()?;
        for gap in rows.flatten() {
            let gap = gap.max(0);
            let idx = edges.iter().rposition(|e| *e <= gap).map_or(0, |i| i + 1);
            buckets[idx].count += 1;
            h.total += 1;
            if gap <= threshold_ms {
                h.within_count += 1;
                h.within_ms += gap;
            } else {
                h.beyond_count += 1;
                h.beyond_ms += gap;
            }
        }
        h.buckets = buckets;
        Some(h)
    }

    /// 数据跨度:`project` = Some（有效项目键）→ 该项目（含合并成员,不含隐藏键）在 daily_project 的首末日;
    /// None → daily_usage ∪ daily_project 的首末日。无数据 → None。
    pub fn data_span(&self, project: Option<&str>, rule: &ScratchRule) -> Option<DaySpan> {
        let row: (Option<String>, Option<String>) = match project {
            Some(p) => self
                .conn()
                .query_row(
                    &format!(
                        "WITH {} SELECT MIN(d.day), MAX(d.day) FROM daily_project d JOIN pmap p ON p.raw_key = d.project_key
                         WHERE p.eff_key = ?1",
                        resolve_cte(rule)
                    ),
                    [p],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok()?,
            None => self
                .conn()
                .query_row(
                    "SELECT MIN(d0), MAX(d1) FROM (
                         SELECT MIN(day) AS d0, MAX(day) AS d1 FROM daily_usage
                         UNION ALL SELECT MIN(day), MAX(day) FROM daily_project)",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok()?,
        };
        match row {
            (Some(first_day), Some(last_day)) => Some(DaySpan { first_day, last_day }),
            _ => None,
        }
    }

    /// 项目推进时间轴:from..to（含端点,本地日）内每个可见有效项目的逐日格子。
    /// - 格子读 `daily_project`（token / turns 与 Matrix 项目维逐格守恒）,会话数与标题读物化层 `turn` + `session`;
    /// - 项目集 = 经解析层的全部可见有效键（合并归目标 / 隐藏剔除 / Scratch 折叠与 Matrix 一致）,
    ///   生命周期取 daily_project 全量首末日,按 last_day 倒序;
    /// - 标题 = 当日 Σwall 最大的根会话（并列取先开始者）的 `session.title`;
    /// - 未来日不产生格子（daily_project 本就没有未来行）;`days` 仍逐日全列。
    /// 参数非法（日期格式 / from > to / 跨度超上限）→ None。只读,不动 schema。
    pub fn project_timeline(&self, from: &str, to: &str, today: NaiveDate, rule: &ScratchRule) -> Option<TimelineResult> {
        let days = day_axis(from, to)?;
        if days.is_empty() || days.len() > TIMELINE_DAYS_MAX {
            return None;
        }
        let cte = resolve_cte(rule);
        // 1) 项目行:全生命周期首末日 + 出现过的 agent（不受范围限制）
        let sql = format!(
            "WITH {cte}
             SELECT p.eff_key, MIN(d.day), MAX(d.day), d.agent_key
             FROM daily_project d JOIN pmap p ON p.raw_key = d.project_key
             GROUP BY p.eff_key, d.agent_key"
        );
        let mut stmt = self.conn().prepare(&sql).ok()?;
        struct Row {
            first: String,
            last: String,
            agents: Vec<String>,
        }
        let mut projects: BTreeMap<String, Row> = BTreeMap::new();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))
            .ok()?;
        for (key, first, last, agent) in rows.flatten() {
            let row = projects.entry(key).or_insert_with(|| Row { first: first.clone(), last: last.clone(), agents: Vec::new() });
            if first < row.first {
                row.first = first;
            }
            if last > row.last {
                row.last = last;
            }
            row.agents.push(agent);
        }
        drop(stmt);
        // 2) 格子:按 （eff_key, day, agent) 汇总 daily_project
        let sql = format!(
            "WITH {cte}
             SELECT p.eff_key, d.day, d.agent_key, SUM(d.turns), SUM(d.total_tokens), SUM(d.wall_ms), SUM(d.idle_ms)
             FROM daily_project d JOIN pmap p ON p.raw_key = d.project_key
             WHERE d.day >= ?1 AND d.day <= ?2
             GROUP BY p.eff_key, d.day, d.agent_key"
        );
        let mut stmt = self.conn().prepare(&sql).ok()?;
        let mut cells: BTreeMap<(String, String), TimelineCell> = BTreeMap::new();
        let rows = stmt
            .query_map([from, to], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })
            .ok()?;
        for (key, day, agent, turns, tokens, wall_ms, idle_ms) in rows.flatten() {
            let cell = cells.entry((key, day.clone())).or_insert_with(|| TimelineCell {
                day,
                turns: 0,
                tokens: 0,
                wall_ms: 0,
                idle_ms: 0,
                sessions: 0,
                agents: Vec::new(),
                items: Vec::new(),
            });
            cell.turns += turns;
            cell.tokens += tokens;
            cell.wall_ms += wall_ms;
            cell.idle_ms += idle_ms;
            cell.agents.push(agent);
        }
        drop(stmt);
        // 3) 会话列表:按 （eff_key, day, 会话) 汇总物化层 turn,关联 session.title。
        //    Claude Code 续聊 / fork 会把整份历史复制成新 session_id（各副本 `session.started_at` 逐毫秒相同,
        //    轮数递增）,否则一天内同一对话重复占条目:同 （agent, session.started_at) 的会话视为同一对话,
        //    只保留**最后活动最新**的那份;排序按最后活动时刻（末轮 ended_at）而不是创建时刻。
        let sql = format!(
            "WITH {cte}
             SELECT p.eff_key, t.day, t.agent_key, t.session_id, s.title, MIN(t.started_at),
                    MAX(COALESCE(t.ended_at, t.started_at)), COUNT(*), SUM(t.total_tokens), SUM(COALESCE(t.wall_ms, 0)),
                    COALESCE(s.started_at, MIN(t.started_at))
             FROM turn t JOIN pmap p ON p.raw_key = t.project_key
                  JOIN session s ON s.agent_key = t.agent_key AND s.session_id = t.session_id
             WHERE t.day >= ?1 AND t.day <= ?2
             GROUP BY p.eff_key, t.day, t.agent_key, t.session_id"
        );
        let mut stmt = self.conn().prepare(&sql).ok()?;
        let rows = stmt
            .query_map([from, to], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(10)?,
                    TimelineSession {
                        agent: r.get(2)?,
                        agent_key: String::new(),
                        session_id: r.get(3)?,
                        title: r.get::<_, Option<String>>(4)?.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
                        started_at: r.get(5)?,
                        last_active_at: r.get(6)?,
                        turns: r.get(7)?,
                        tokens: r.get(8)?,
                        wall_ms: r.get(9)?,
                    },
                ))
            })
            .ok()?;
        // 去重键 （eff_key, day, agent, session.started_at) → 保留最后活动最新（并列取轮数多）的一份
        let mut dedup: BTreeMap<(String, String, String, i64), TimelineSession> = BTreeMap::new();
        for (key, day, session_started, item) in rows.flatten() {
            let k = (key, day, item.agent.clone(), session_started);
            match dedup.get(&k) {
                Some(cur) if (cur.last_active_at, cur.turns) >= (item.last_active_at, item.turns) => {}
                _ => {
                    dedup.insert(k, item);
                }
            }
        }
        let mut items: Vec<((String, String), TimelineSession)> = dedup.into_iter().map(|((key, day, _, _), it)| ((key, day), it)).collect();
        // 最新活动在前（并列按 agent / session_id 稳定）
        items.sort_by(|a, b| b.1.last_active_at.cmp(&a.1.last_active_at).then_with(|| a.1.agent.cmp(&b.1.agent)).then_with(|| a.1.session_id.cmp(&b.1.session_id)));
        for (k, item) in items {
            if let Some(cell) = cells.get_mut(&k) {
                cell.sessions += 1;
                cell.items.push(item);
            }
        }
        drop(stmt);
        // 4) 组装:标签、agent 展示名、未动天数、按 last_day 倒序
        let keys: Vec<String> = projects.keys().cloned().collect();
        let labels: HashMap<String, String> = keys.iter().cloned().zip(project_labels(&keys, &self.project_aliases())).collect();
        let mut cells_by_key: BTreeMap<String, Vec<TimelineCell>> = BTreeMap::new();
        for ((key, _), mut cell) in cells {
            cell.agents.sort();
            cell.agents.dedup();
            cell.agents = cell.agents.iter().map(|a| agent_label(a)).collect();
            for it in &mut cell.items {
                it.agent_key = std::mem::take(&mut it.agent);
                it.agent = agent_label(&it.agent_key);
            }
            cells_by_key.entry(key).or_default().push(cell);
        }
        let mut out: Vec<TimelineProject> = projects
            .into_iter()
            .map(|(key, mut row)| {
                row.agents.sort();
                row.agents.dedup();
                let last = NaiveDate::parse_from_str(&row.last, "%Y-%m-%d").ok();
                let inactive_days = last.map(|d| (today - d).num_days()).filter(|n| *n >= 0);
                TimelineProject {
                    label: labels.get(&key).cloned().unwrap_or_else(|| key.clone()),
                    agents: row.agents.iter().map(|a| agent_label(a)).collect(),
                    first_day: Some(row.first),
                    last_day: Some(row.last),
                    inactive_days,
                    cells: cells_by_key.remove(&key).unwrap_or_default(),
                    key,
                }
            })
            .collect();
        out.sort_by(|a, b| b.last_day.cmp(&a.last_day).then_with(|| a.key.cmp(&b.key)));
        Some(TimelineResult { today: today.format("%Y-%m-%d").to_string(), days, projects: out })
    }

    /// 会话最近一轮所在的**原始**目录键（双击跳转用;物化层没有该会话 → None）。
    pub fn session_project_key(&self, agent: &str, session_id: &str) -> Option<String> {
        self.conn()
            .query_row(
                "SELECT project_key FROM turn WHERE agent_key = ?1 AND session_id = ?2 ORDER BY started_at DESC LIMIT 1",
                [agent, session_id],
                |r| r.get(0),
            )
            .ok()
    }

    /// 会话的桌面宿主线索（采集游标里的 `turn.host`,见 `TurnState.host`;目前只有 Claude Code 写）。
    /// 游标已不在（源文件被清理）→ None,调用方按「宿主未知」处理。
    pub fn session_host(&self, agent: &str, session_id: &str) -> Option<String> {
        let mut stmt = self
            .conn()
            .prepare("SELECT cursor_json FROM source_cursor WHERE source_id = ?1 AND instr(cursor_json, ?2) > 0 ORDER BY updated_at DESC")
            .ok()?;
        let rows = stmt.query_map([agent, session_id], |r| r.get::<_, String>(0)).ok()?;
        let host = rows.flatten().find_map(|json| {
            let v: serde_json::Value = serde_json::from_str(&json).ok()?;
            let turn = v.get("turn")?;
            (turn.get("session_id")?.as_str()? == session_id).then(|| turn.get("host")?.as_str().map(str::to_string)).flatten()
        });
        host
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::store::{Batch, SessionRow, Tokens, TurnPart, TurnRow};
    use crate::collector::project_meta::{ProjectMetaInput, ScratchRule};

    const OFF: &ScratchRule = &ScratchRule::OFF;

    const T: i64 = 1_788_602_400_000; // 本地 2026-09-05 日内

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
    }

    fn day_of(ms: i64) -> String {
        crate::collector::millis_to_local_day(ms).unwrap()
    }

    struct Turn<'a> {
        agent: &'a str,
        sid: &'a str,
        seq: i64,
        start: i64,
        gap: Option<i64>,
        project: &'a str,
        model: &'a str,
        tokens: i64,
        mark: i64,
    }

    /// 同源双写 daily_usage + turn_raw / turn_part（模拟适配器 response() 路径）。
    fn add(b: &mut Batch, t: Turn) {
        let day = day_of(t.start);
        b.add_usage(&day, Some(9), t.agent, t.model, Tokens { input: t.tokens, output: 0, total: t.tokens, cache_read: 0, cache_write: 0 }, t.mark);
        b.add_turn(
            t.agent,
            TurnRow {
                session_id: t.sid.into(),
                turn_seq: t.seq,
                day: day.clone(),
                project_key: t.project.into(),
                model_key: t.model.into(),
                started_at: t.start,
                ended_at: t.start + 2_000,
                wall_ms: Some(2_000),
                model_ms: Some(1_500),
                tool_ms: Some(300),
                ttft_ms: None,
                gap_ms: t.gap,
                model_calls: 1,
                tool_calls: 2,
                error_count: 0,
                retry_count: 0,
                aborted: false,
                parts: vec![TurnPart {
                    day,
                    model: t.model.into(),
                    input: t.tokens,
                    output: 0,
                    total: t.tokens,
                    cache_read: 0,
                    cache_write: 0,
                    model_calls: 1,
                    turn_mark: t.mark,
                }],
            },
        );
    }

    fn session(b: &mut Batch, agent: &str, sid: &str, project: &str, parent: Option<&str>, title: Option<&str>) {
        b.upsert_session(
            agent,
            SessionRow {
                session_id: sid.into(),
                project_key: Some(project.into()),
                project_authoritative: false,
                parent_id: parent.map(str::to_string),
                title: title.map(str::to_string),
                ..SessionRow::default()
            },
        );
    }

    /// 两个项目（同名末段 `app` 不同路径）+ 一个子会话 + 一个无项目维的源（只写 daily_usage）。
    fn fixture() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        let mut b = Batch::default();
        session(&mut b, "claude-code", "s1", "e:/work/app", None, Some("<title 1>"));
        add(&mut b, Turn { agent: "claude-code", sid: "s1", seq: 1, start: T, gap: None, project: "e:/work/app", model: "m1", tokens: 100, mark: 1 });
        add(&mut b, Turn { agent: "claude-code", sid: "s1", seq: 2, start: T + 60_000, gap: Some(58_000), project: "e:/work/app", model: "m1", tokens: 50, mark: 1 });
        add(&mut b, Turn { agent: "claude-code", sid: "s1", seq: 3, start: T + 7_260_000, gap: Some(7_198_000), project: "e:/work/app", model: "m2", tokens: 10, mark: 1 });
        session(&mut b, "claude-code", "sub", "e:/work/app", Some("s1"), None);
        add(&mut b, Turn { agent: "claude-code", sid: "sub", seq: 1, start: T + 61_000, gap: Some(1_000), project: "e:/work/app", model: "m3", tokens: 7, mark: 0 });
        session(&mut b, "codex", "s2", "d:/other/app", None, None);
        add(&mut b, Turn { agent: "codex", sid: "s2", seq: 1, start: T + 86_400_000, gap: None, project: "d:/other/app", model: "m1", tokens: 300, mark: 1 });
        s.commit("claude-code", &b).unwrap();
        let mut plain = Batch::default();
        plain.add("2026-09-05", "codebuddy", "unknown", 5, 0, 5, 1);
        s.commit("codebuddy", &plain).unwrap();
        s
    }

    #[test]
    fn empty_store_queries() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.project_month_rows("2026-09", "project", "total", today(), OFF).unwrap().is_empty());
        let bd = s.project_breakdown("project", "e:/x", "2026-09", today(), OFF).unwrap();
        assert_eq!(bd.len(), 30);
        assert!(bd.iter().all(|d| d.slices.is_none()));
        let series = s.effort_series("2026-09-01", "2026-09-03", "day", "project", "wait", None, OFF).unwrap();
        assert!(series.series_keys.is_empty());
        assert_eq!(series.points.len(), 3);
        let total = s.effort_series("2026-09-01", "2026-09-03", "day", "total", "human", None, OFF).unwrap();
        assert_eq!((total.series_keys.len(), total.points[0].values.clone()), (1, vec![0]), "total 维无数据也补基线");
        let page = s.task_list(0, i64::MAX, &TaskFilters::default(), &TaskSort::default(), &TaskPageReq::default(), OFF).unwrap();
        assert_eq!((page.total, page.rows.len()), (0, 0));
        assert!(s.task_turns("claude-code", "nope").unwrap().is_empty());
        let h = s.gap_histogram("2026-09-01", "2026-09-30", 1_800_000, OFF).unwrap();
        assert_eq!((h.total, h.buckets.len()), (0, 26));
    }

    #[test]
    fn project_rows_conserve_with_daily_usage_and_label_duplicates() {
        let s = fixture();
        let rows = s.project_month_rows("2026-09", "project", "total", today(), OFF).unwrap();
        assert_eq!(rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(), vec!["d:/other/app", "e:/work/app"]);
        assert_eq!(rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), vec!["d:/other/app", "e:/work/app"], "同名末段退回完整路径");
        assert_eq!(rows[1].month_total, 100 + 50 + 10 + 7, "子会话 token 计入其项目");
        assert_eq!(rows[1].values[30 - 1], Some(0), "过去无记录日 = 0");
        assert!(s.project_month_rows("2026-09", "project", "total", NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(), OFF).unwrap()[0].values[20].is_none(), "未来 = null");
        // agent / model 维 token 与 turns 与既有矩阵守恒（无项目维的 codebuddy 不出现）
        for dim in ["agent", "model"] {
            let project_view = s.project_month_rows("2026-09", dim, "total", today(), OFF).unwrap();
            let usage_view: Vec<StoreRow> = s.month_rows("2026-09", dim, "total", today()).unwrap().into_iter().filter(|r| r.key != "codebuddy" && r.key != "unknown").collect();
            assert_eq!(project_view.len(), usage_view.len(), "{dim}");
            for (p, u) in project_view.iter().zip(&usage_view) {
                assert_eq!((&p.key, &p.values, &p.message_counts, p.month_total), (&u.key, &u.values, &u.message_counts, u.month_total), "{dim}");
            }
        }
        let turns = s.project_month_rows("2026-09", "project", "turns", today(), OFF).unwrap();
        assert_eq!(turns.iter().map(|r| r.month_total).sum::<i64>(), 4);
        let wait = s.project_month_rows("2026-09", "agent", "wait", today(), OFF).unwrap();
        let cc = wait.iter().find(|r| r.key == "claude-code").unwrap();
        assert_eq!(cc.month_total, 3 * 2_000, "wait 只算根会话");
        let human = s.project_month_rows("2026-09", "project", "human", today(), OFF).unwrap();
        assert_eq!(human.iter().find(|r| r.key == "e:/work/app").unwrap().month_total, 58_000, "超阈值 gap 与子会话 gap 不计");
        let calls = s.project_month_rows("2026-09", "project", "model_calls", today(), OFF).unwrap();
        assert_eq!(calls.iter().map(|r| r.month_total).sum::<i64>(), 5);
        assert!(s.project_month_rows("2026-09", "project", "bogus", today(), OFF).is_none());
        assert!(s.project_month_rows("2026-09", "bogus", "total", today(), OFF).is_none());
        assert!(s.project_month_rows("2026-13", "project", "total", today(), OFF).is_none());
    }

    #[test]
    fn project_only_source_absent_has_no_project_rows() {
        let mut s = Store::open_in_memory().unwrap();
        let mut plain = Batch::default();
        plain.add("2026-09-05", "codebuddy", "unknown", 5, 0, 5, 1);
        s.commit("codebuddy", &plain).unwrap();
        assert_eq!(s.month_rows("2026-09", "agent", "total", today()).unwrap().len(), 1);
        assert!(s.project_month_rows("2026-09", "project", "total", today(), OFF).unwrap().is_empty(), "无项目维数据 → 空行集");
        assert!(s.effort_series("2026-09-01", "2026-09-30", "day", "project", "total", None, OFF).unwrap().series_keys.is_empty());
    }

    #[test]
    fn project_breakdown_slices_conserve_cells() {
        let s = fixture();
        let rows = s.project_month_rows("2026-09", "project", "total", today(), OFF).unwrap();
        for r in &rows {
            let bd = s.project_breakdown("project", &r.key, "2026-09", today(), OFF).unwrap();
            for (i, d) in bd.iter().enumerate() {
                let sum: i64 = d.slices.as_ref().map_or(0, |v| v.iter().map(|x| x.tokens).sum());
                assert_eq!(Some(sum), r.values[i], "{} {}", r.key, d.day);
            }
        }
        let by_agent = s.project_breakdown("project", "e:/work/app", "2026-09", today(), OFF).unwrap();
        let d5 = by_agent.iter().find(|d| d.day == day_of(T)).unwrap().slices.as_ref().unwrap();
        assert_eq!((d5[0].key.as_str(), d5[0].label.as_str()), ("claude-code", "Claude Code"));
        let agent_projects = s.project_breakdown("agent", "claude-code", "2026-09", today(), OFF).unwrap();
        let slices = agent_projects.iter().find(|d| d.day == day_of(T)).unwrap().slices.as_ref().unwrap();
        assert_eq!((slices[0].label.as_str(), slices[0].tokens), ("app", 167), "单项目时取末段");
        assert!(s.project_breakdown("bogus", "x", "2026-09", today(), OFF).is_none());
    }

    #[test]
    fn effort_series_axis_filters_and_hour_unsupported() {
        let s = fixture();
        let d1 = day_of(T);
        let d2 = day_of(T + 86_400_000);
        let series = s.effort_series(&d1, &d2, "day", "agent", "wait", None, OFF).unwrap();
        assert_eq!(series.points.len(), 2);
        assert_eq!(series.series_keys, vec!["claude-code".to_string(), "codex".to_string()]);
        assert_eq!(series.points[0].values, vec![6_000, 0]);
        assert_eq!(series.points[1].values, vec![0, 2_000]);
        let filtered = s.effort_series(&d1, &d2, "day", "model", "total", Some(("project", "e:/work/app")), OFF).unwrap();
        assert_eq!(filtered.series_keys, vec!["m1".to_string(), "m2".to_string(), "m3".to_string()]);
        let total = s.effort_series(&d1, &d2, "day", "total", "turns", None, OFF).unwrap();
        assert_eq!(total.points.iter().map(|p| p.values[0]).collect::<Vec<_>>(), vec![3, 1]);
        assert!(s.effort_series(&d1, &d2, "hour", "agent", "wait", None, OFF).is_none(), "不支持 hour");
        assert!(s.effort_series(&d1, &d2, "day", "agent", "wait", Some(("bogus", "x")), OFF).is_none());
    }

    #[test]
    fn task_list_roots_only_sort_filter_page() {
        let s = fixture();
        let all = s.task_list(0, i64::MAX, &TaskFilters::default(), &TaskSort::default(), &TaskPageReq::default(), OFF).unwrap();
        assert_eq!(all.total, 2, "子会话不出现");
        assert!(all.rows.iter().all(|r| r.session_id != "sub"));
        assert_eq!(all.rows[0].session_id, "s2", "默认 started_at 降序");
        let s1 = all.rows.iter().find(|r| r.session_id == "s1").unwrap();
        assert_eq!((s1.turns, s1.steps, s1.tool_calls, s1.subagent_count, s1.subagent_calls), (3, 4, 8, 1, 1), "子会话调用并入");
        assert_eq!((s1.wall_ms, s1.model_ms, s1.total_tokens, s1.title.as_deref()), (Some(6_000), Some(6_000), 167, Some("<title 1>")));
        let by_tokens = s
            .task_list(0, i64::MAX, &TaskFilters::default(), &TaskSort { field: "total_tokens".into(), direction: "asc".into() }, &TaskPageReq { offset: 0, limit: 1 }, OFF)
            .unwrap();
        assert_eq!((by_tokens.total, by_tokens.rows.len(), by_tokens.rows[0].session_id.as_str()), (2, 1, "s1"));
        let page2 = s
            .task_list(0, i64::MAX, &TaskFilters::default(), &TaskSort { field: "total_tokens".into(), direction: "asc".into() }, &TaskPageReq { offset: 1, limit: 1 }, OFF)
            .unwrap();
        assert_eq!(page2.rows[0].session_id, "s2");
        let codex = s.task_list(0, i64::MAX, &TaskFilters { agent: Some("codex".into()), project: None }, &TaskSort::default(), &TaskPageReq::default(), OFF).unwrap();
        assert_eq!((codex.total, codex.rows[0].project.as_str()), (1, "d:/other/app"));
        let proj = s.task_list(0, i64::MAX, &TaskFilters { agent: None, project: Some("e:/work/app".into()) }, &TaskSort::default(), &TaskPageReq::default(), OFF).unwrap();
        assert_eq!(proj.total, 1);
        let ranged = s.task_list(T + 1, i64::MAX, &TaskFilters::default(), &TaskSort::default(), &TaskPageReq::default(), OFF).unwrap();
        assert_eq!(ranged.total, 1, "按会话开始时间过滤");
        assert!(s.task_list(0, i64::MAX, &TaskFilters::default(), &TaskSort { field: "title".into(), direction: "asc".into() }, &TaskPageReq::default(), OFF).is_none(), "排序字段白名单");
    }

    #[test]
    fn task_turns_renumbered_and_children_empty() {
        let s = fixture();
        let turns = s.task_turns("claude-code", "s1").unwrap();
        assert_eq!(turns.iter().map(|t| t.turn_seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!((turns[1].steps, turns[1].subagent_count, turns[1].gap_ms), (2, 1, Some(58_000)));
        assert!(turns.windows(2).all(|w| w[0].started_at <= w[1].started_at));
        assert!(s.task_turns("claude-code", "sub").unwrap().is_empty(), "子会话无独立明细");
    }

    #[test]
    fn gap_histogram_buckets_and_threshold_recompute_conserve() {
        let mut s = fixture();
        let (d1, d2) = (day_of(T), day_of(T + 86_400_000));
        let idle_sum = |s: &Store| -> i64 {
            s.conn().query_row("SELECT COALESCE(SUM(idle_ms), 0) FROM daily_project WHERE day >= ?1 AND day <= ?2", [&d1, &d2], |r| r.get(0)).unwrap()
        };
        let tokens_turns = |s: &Store| -> (i64, i64) {
            s.conn().query_row("SELECT SUM(total_tokens), SUM(turns) FROM daily_project", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        };
        let h = s.gap_histogram(&d1, &d2, IDLE_DEFAULT, OFF).unwrap();
        assert_eq!(h.total, 2, "根会话有 gap 的轮;子会话不计");
        assert_eq!(h.buckets.iter().map(|b| b.count).sum::<i64>(), 2);
        assert_eq!((h.within_count, h.within_ms, h.beyond_count, h.beyond_ms), (1, 58_000, 1, 7_198_000));
        assert_eq!(h.within_ms, idle_sum(&s), "直方图 within = daily_project.idle_ms");
        let b58 = h.buckets.iter().find(|b| b.lo_ms <= 58_000 && b.hi_ms.map_or(true, |hi| 58_000 < hi)).unwrap();
        assert_eq!(b58.count, 1);
        assert!(h.buckets.last().unwrap().hi_ms.is_none());
        assert!(s.project_threshold_marker().is_none(), "未全表重算过");

        let before = tokens_turns(&s);
        // 阈值调到 3 小时:超阈值的 2h gap 也计入;token / turns 不变
        let n = s.recompute_projects(3 * 3_600_000).unwrap();
        assert!(n >= 2);
        assert_eq!(s.project_threshold_marker(), Some(3 * 3_600_000));
        let h3 = s.gap_histogram(&d1, &d2, 3 * 3_600_000, OFF).unwrap();
        assert_eq!((h3.within_count, h3.within_ms), (2, 58_000 + 7_198_000));
        assert_eq!(idle_sum(&s), h3.within_ms);
        assert_eq!(tokens_turns(&s), before, "阈值重算不动 token / turns");
        let mismatches = s.test_project_conservation();
        assert!(mismatches.iter().all(|m| m.contains("codebuddy")), "只剩无项目维的 codebuddy 格:{mismatches:?}");
        // 调到 1 分钟:两条都超阈值
        s.recompute_projects(60_000).unwrap();
        assert_eq!((idle_sum(&s), s.gap_histogram(&d1, &d2, 60_000, OFF).unwrap().within_ms), (58_000, 58_000));
        s.recompute_projects(30_000).unwrap();
        assert_eq!(idle_sum(&s), 0);
        assert!(s.gap_histogram("bad", &d2, 1, OFF).is_none());
    }

    const IDLE_DEFAULT: i64 = crate::collector::store::IDLE_THRESHOLD_MS;

    #[test]
    fn data_span_for_project_and_all() {
        let s = fixture();
        let d0 = day_of(T);
        let d1 = day_of(T + 86_400_000);
        assert_eq!(s.data_span(Some("e:/work/app"), OFF), Some(DaySpan { first_day: d0.clone(), last_day: day_of(T + 7_260_000) }));
        assert_eq!(s.data_span(Some("d:/other/app"), OFF), Some(DaySpan { first_day: d1.clone(), last_day: d1.clone() }));
        assert_eq!(s.data_span(Some("nope"), OFF), None);
        // All = daily_usage ∪ daily_project（codebuddy 只写 daily_usage 的 2026-09-05 也计入）
        let all = s.data_span(None, OFF).unwrap();
        assert_eq!((all.first_day.as_str() <= d0.as_str(), all.last_day), (true, d1));
        assert_eq!(Store::open_in_memory().unwrap().data_span(None, OFF), None);
    }

    #[test]
    fn aborted_and_errors_are_separate_columns() {
        let mut s = Store::open_in_memory().unwrap();
        let mut b = Batch::default();
        session(&mut b, "codex", "r", "e:/p", None, None);
        session(&mut b, "codex", "c", "e:/p", Some("r"), None);
        let row = |sid: &str, seq: i64, start: i64, calls: i64, errors: i64, aborted: bool| TurnRow {
            session_id: sid.into(),
            turn_seq: seq,
            day: day_of(start),
            project_key: "e:/p".into(),
            model_key: "m".into(),
            started_at: start,
            ended_at: start + 1_000,
            wall_ms: Some(1_000),
            model_ms: Some(500),
            tool_ms: Some(0),
            ttft_ms: None,
            gap_ms: None,
            model_calls: calls,
            tool_calls: 0,
            error_count: errors,
            retry_count: 0,
            aborted,
            parts: Vec::new(),
        };
        b.add_turn("codex", row("r", 1, T, 0, 0, true));
        b.add_turn("codex", row("r", 2, T + 60_000, 0, 1, false));
        // 子会话在父轮 2 内被中止:错误并入父轮,中止不并入
        b.add_turn("codex", row("c", 1, T + 61_000, 0, 2, true));
        s.commit("codex", &b).unwrap();
        let page = s.task_list(0, i64::MAX, &TaskFilters::default(), &TaskSort::default(), &TaskPageReq::default(), OFF).unwrap();
        assert_eq!((page.rows[0].error_count, page.rows[0].aborted_count), (3, 1));
        let turns = s.task_turns("codex", "r").unwrap();
        assert_eq!(turns.iter().map(|t| (t.error_count, t.aborted)).collect::<Vec<_>>(), vec![(0, true), (3, false)]);
        let (aborted, errors): (i64, i64) = s
            .conn()
            .query_row("SELECT SUM(aborted_count), SUM(error_count) FROM daily_project WHERE agent_key = 'codex'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((aborted, errors), (1, 3), "aborted_count 只算根会话;error_count 含子会话");
        let sorted = s
            .task_list(0, i64::MAX, &TaskFilters::default(), &TaskSort { field: "aborted_count".into(), direction: "desc".into() }, &TaskPageReq::default(), OFF)
            .unwrap();
        assert_eq!(sorted.total, 1);
    }

    // ---------- 项目推进时间轴 ----------

    fn tl(s: &Store, from: &str, to: &str) -> TimelineResult {
        s.project_timeline(from, to, today(), OFF).unwrap()
    }

    #[test]
    fn timeline_cells_conserve_and_future_days_are_empty() {
        let s = fixture();
        let d0 = day_of(T);
        let d1 = day_of(T + 86_400_000);
        let r = tl(&s, "2026-09-01", "2026-10-01");
        assert_eq!((r.today.as_str(), r.days.len(), r.days[0].as_str()), ("2026-09-30", 31, "2026-09-01"));
        assert_eq!(r.projects.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(), vec!["d:/other/app", "e:/work/app"], "按 last_day 倒序");
        let app = &r.projects[1];
        assert_eq!((app.first_day.as_deref(), app.last_day.as_deref()), (Some(d0.as_str()), Some(d0.as_str())));
        assert_eq!(app.cells.len(), 1, "只含有活动的日;未来日与无记录日不出格子");
        let c = &app.cells[0];
        // token / turns 与 Matrix 项目维同日守恒（子会话 token 计入其项目,轮只算根会话）
        assert_eq!((c.day.as_str(), c.tokens, c.turns, c.sessions), (d0.as_str(), 100 + 50 + 10 + 7, 3, 1));
        assert_eq!(c.agents, vec!["Claude Code"]);
        assert_eq!(c.items.len(), 1, "子会话不单列");
        assert_eq!((c.items[0].title.as_deref(), c.items[0].started_at, c.items[0].turns, c.items[0].agent.as_str()), (Some("<title 1>"), T, 3, "Claude Code"));
        let other = &r.projects[0];
        assert_eq!((other.cells[0].day.as_str(), other.cells[0].tokens, other.cells[0].items[0].title.as_deref()), (d1.as_str(), 300, None), "无标题会话 title=None,前端回退时刻");
        assert_eq!(other.cells[0].items[0].started_at, T + 86_400_000);
        // 范围裁剪:只查 d1 一天 → app 无格子但仍在项目列表（生命周期不受范围限制）
        let narrow = tl(&s, &d1, &d1);
        assert_eq!(narrow.projects.iter().map(|p| (p.key.as_str(), p.cells.len())).collect::<Vec<_>>(), vec![("d:/other/app", 1), ("e:/work/app", 0)]);
        // 非法参数
        assert!(s.project_timeline("2026-09-10", "2026-09-01", today(), OFF).is_none());
        assert!(s.project_timeline("2026-09-x", "2026-09-01", today(), OFF).is_none());
        assert!(s.project_timeline("2020-01-01", "2026-09-01", today(), OFF).is_none(), "跨度超上限");
        assert!(Store::open_in_memory().unwrap().project_timeline("2026-09-01", "2026-09-02", today(), OFF).unwrap().projects.is_empty());
    }

    #[test]
    fn timeline_inactive_days_from_today() {
        let s = fixture();
        let r = tl(&s, "2026-09-01", "2026-09-30");
        // today = 2026-09-30;d:/other/app 末日 = T+1 天
        let d1 = NaiveDate::parse_from_str(&day_of(T + 86_400_000), "%Y-%m-%d").unwrap();
        assert_eq!(r.projects[0].inactive_days, Some((today() - d1).num_days()));
        let d0 = NaiveDate::parse_from_str(&day_of(T), "%Y-%m-%d").unwrap();
        assert_eq!(r.projects[1].inactive_days, Some((today() - d0).num_days()));
        // today 早于 last_day（时钟回拨）→ None 而不是负数
        let back = s.project_timeline("2026-09-01", "2026-09-30", NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(), OFF).unwrap();
        assert!(back.projects.iter().all(|p| p.inactive_days.is_none()));
    }

    #[test]
    fn timeline_sessions_listed_latest_first() {
        let mut s = Store::open_in_memory().unwrap();
        let mut b = Batch::default();
        // 同项目同日两会话:a 一轮（先开始）,b 两轮（后开始）→ items = [b, a]（最新在前）
        session(&mut b, "claude-code", "a", "e:/p", None, Some("short one"));
        add(&mut b, Turn { agent: "claude-code", sid: "a", seq: 1, start: T, gap: None, project: "e:/p", model: "m", tokens: 1, mark: 1 });
        session(&mut b, "claude-code", "b", "e:/p", None, Some("long one"));
        add(&mut b, Turn { agent: "claude-code", sid: "b", seq: 1, start: T + 10_000, gap: None, project: "e:/p", model: "m", tokens: 1, mark: 1 });
        add(&mut b, Turn { agent: "claude-code", sid: "b", seq: 2, start: T + 20_000, gap: Some(8_000), project: "e:/p", model: "m", tokens: 1, mark: 1 });
        s.commit("claude-code", &b).unwrap();
        let r = tl(&s, "2026-09-01", "2026-09-30");
        let c = &r.projects[0].cells[0];
        assert_eq!((c.sessions, c.turns), (2, 3));
        assert_eq!(c.items.iter().map(|i| (i.title.as_deref(), i.started_at, i.turns, i.tokens)).collect::<Vec<_>>(), vec![(Some("long one"), T + 10_000, 2, 2), (Some("short one"), T, 1, 1)]);
        assert_eq!(c.items[0].last_active_at, T + 22_000, "最后活动 = 末轮 ended_at");
    }

    #[test]
    fn timeline_forked_sessions_collapse_into_one_item() {
        // Claude Code 续聊 fork：三个 session_id 复制同一份历史（session.started_at 相同,轮数递增）
        let mut s = Store::open_in_memory().unwrap();
        let mut b = Batch::default();
        for (sid, turns) in [("f1", 1), ("f2", 3), ("f3", 2)] {
            b.upsert_session("claude-code", SessionRow { session_id: sid.into(), project_key: Some("e:/p".into()), title: Some("same chat".into()), started_at: Some(T), ..SessionRow::default() });
            for i in 0..turns {
                add(&mut b, Turn { agent: "claude-code", sid, seq: i + 1, start: T + i * 60_000, gap: None, project: "e:/p", model: "m", tokens: 1, mark: 1 });
            }
        }
        // 另一条真正独立的会话（不同 started_at）
        session(&mut b, "claude-code", "g", "e:/p", None, Some("other"));
        add(&mut b, Turn { agent: "claude-code", sid: "g", seq: 1, start: T + 5_000, gap: None, project: "e:/p", model: "m", tokens: 1, mark: 1 });
        s.commit("claude-code", &b).unwrap();
        let r = tl(&s, "2026-09-01", "2026-09-30");
        let c = &r.projects[0].cells[0];
        assert_eq!(c.sessions, 2, "三份 fork 折成一条 + 一条独立会话");
        assert_eq!(c.items.iter().map(|i| (i.session_id.as_str(), i.turns)).collect::<Vec<_>>(), vec![("f2", 3), ("g", 1)], "保留最后活动最新的 fork,最新活动在前");
    }

    #[test]
    fn timeline_merge_folds_cells_into_target() {
        let mut s = fixture();
        let d0 = day_of(T);
        let d1 = day_of(T + 86_400_000);
        s.merge_projects(&["d:/other/app".to_string()], "e:/work/app").unwrap();
        let r = tl(&s, "2026-09-01", "2026-09-30");
        assert_eq!(r.projects.len(), 1);
        let p = &r.projects[0];
        assert_eq!((p.key.as_str(), p.first_day.as_deref(), p.last_day.as_deref()), ("e:/work/app", Some(d0.as_str()), Some(d1.as_str())));
        assert_eq!(p.agents, vec!["Claude Code", "Codex"]);
        assert_eq!(p.cells.iter().map(|c| (c.day.as_str(), c.tokens)).collect::<Vec<_>>(), vec![(d0.as_str(), 167), (d1.as_str(), 300)]);
        assert_eq!(p.cells[1].agents, vec!["Codex"]);
        assert_eq!(p.label, "app", "合并后无同名冲突,退回末段");
    }

    #[test]
    fn timeline_hidden_project_is_absent() {
        let mut s = fixture();
        s.set_project_meta(&ProjectMetaInput { project_key: "d:/other/app".into(), hidden: true, ..Default::default() }).unwrap();
        let r = tl(&s, "2026-09-01", "2026-09-30");
        assert_eq!(r.projects.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(), vec!["e:/work/app"]);
    }

    #[test]
    fn gap_edges_are_log_spaced() {
        let e = gap_bucket_edges();
        assert_eq!((e.len(), e[0], e[4], e[8], e[24]), (25, 1_000, 10_000, 100_000, 1_000_000_000));
        assert!(e.windows(2).all(|w| w[0] < w[1]));
    }

}
