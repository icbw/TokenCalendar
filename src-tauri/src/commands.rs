//! 前端命令面 = 冻结的数据契约（形状对齐旧项目 Wails 绑定，
//! 命名统一 snake_case）。起数据来源 = collector.db 聚合（fixture 退役为
//! 纯测试资产）；契约形状不变，仅 SourceSummary 增补契约已有的可选字段
//! schema_fingerprint。调整：可见性原语移入 visibility.rs。

use std::sync::atomic::Ordering;

use chrono::Local;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_autostart::ManagerExt as AutostartExt;

use crate::collector::store::{Store, days_in_month};
use crate::{AppState, collector};

// ---------- 契约类型 ----------

// bucket/metric：bucket 仅支持 day（week/cumulative 由前端聚合）；
// metric 支持 total/input/output（聚合表三列直供）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MatrixQuery {
    pub month: String,
    pub group_by: String, // "agent" | "model"
    pub bucket: String,
    pub metric: String,
    #[allow(dead_code)] // 契约保留位：global/perRow 归一化由前端处理
    pub normalization: String,
}

#[derive(Debug, Serialize)]
pub struct MatrixRow {
    pub key: String,
    pub label: String,
    /// 长度 = days_in_month；null=未来日期（≠0），单位=token 数。
    pub values: Vec<Option<i64>>,
    /// 契约扩展：与 values 平行的请求/对话数（未来日为 0）。
    pub message_counts: Vec<i64>,
    pub month_total: i64,
}

#[derive(Debug, Serialize)]
pub struct MatrixResult {
    pub month: String,
    pub days_in_month: u32,
    /// Unix 毫秒。
    pub generated_at: i64,
    pub rows: Vec<MatrixRow>,
}

#[derive(Debug, Serialize)]
pub struct BreakdownSlice {
    pub key: String,
    pub label: String,
    pub tokens: i64,
}

#[derive(Debug, Serialize)]
pub struct BreakdownDay {
    pub day: String,
    pub slices: Option<Vec<BreakdownSlice>>,
}

#[derive(Debug, Serialize)]
pub struct SourceSummary {
    pub id: String,
    pub adapter_id: String,
    pub adapter_name: String,
    pub location: String,
    pub kind: String,       // sqlite | jsonl | json | dir
    pub probe_status: String, // ready | partial | no_source | unsupported_schema | permission_denied | busy | corrupted
    pub schema_fingerprint: Option<String>,
    pub last_success_at: Option<String>,
    pub last_attempt_at: Option<String>,
    pub last_error_code: Option<String>,
    pub last_error_message: Option<String>,
    pub events_collected: i64,
    pub stale: bool,
}

#[derive(Debug, Serialize)]
pub struct ExportResult {
    pub path: String,
    pub rows: usize,
    pub format: String,
}

// ---------- 内部工具 ----------

fn today() -> chrono::NaiveDate {
    Local::now().date_naive()
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn millis_to_rfc3339(m: Option<i64>) -> Option<String> {
    use chrono::TimeZone;
    m.and_then(|v| Local.timestamp_millis_opt(v).single())
        .map(|dt| dt.to_rfc3339())
}

/// 命令线程经 AppState 上的**读连接**查询采集库（写连接归采集线程,WAL 不互相阻塞）。
fn with_reader<T>(state: &State<'_, AppState>, f: impl FnOnce(&Store) -> Result<T, String>) -> Result<T, String> {
    let reader = state
        .collector_reader
        .get()
        .ok_or_else(|| "collector storage unavailable".to_string())?;
    let store = reader.lock().map_err(|_| "collector store poisoned".to_string())?;
    f(&store)
}

// ---------- Usage 命令 ----------

#[tauri::command]
pub fn get_monthly_matrix(query: MatrixQuery, state: State<'_, AppState>) -> Result<MatrixResult, String> {
    if query.bucket != "day" {
        // 契约保留位：week/cumulative 由前端聚合，这里硬失败以防误用
        return Err(format!("unsupported bucket: {} (only 'day')", query.bucket));
    }
    if !matches!(query.metric.as_str(), "total" | "input" | "output") {
        return Err(format!("unsupported metric: {}", query.metric));
    }
    let dim = month_dim(&query.month)?;
    let rows = with_reader(&state, |store| {
        Ok(store.month_rows(&query.month, &query.group_by, &query.metric, today()))
    })?
    .ok_or_else(|| format!("invalid month: {}", query.month))?;

    Ok(MatrixResult {
        month: query.month,
        days_in_month: dim,
        generated_at: now_millis(),
        rows: rows
            .into_iter()
            .map(|r| MatrixRow {
                key: r.key,
                label: r.label,
                values: r.values,
                message_counts: r.message_counts,
                month_total: r.month_total,
            })
            .collect(),
    })
}

fn month_dim(month: &str) -> Result<u32, String> {
    let (y, m) = month
        .split_once('-')
        .and_then(|(y, m)| Some((y.parse::<i32>().ok()?, m.parse::<u32>().ok()?)))
        .ok_or_else(|| format!("invalid month: {}", month))?;
    if !(1..=12).contains(&m) {
        return Err(format!("invalid month: {}", month));
    }
    Ok(days_in_month(y, m))
}

#[tauri::command]
pub fn get_breakdown(kind: String, key: String, month: String, state: State<'_, AppState>) -> Result<Vec<BreakdownDay>, String> {
    let days = with_reader(&state, |store| {
        Ok(store.breakdown(&kind, &key, &month, today()))
    })?
    .ok_or_else(|| format!("invalid month: {}", month))?;
    Ok(days
        .into_iter()
        .map(|d| BreakdownDay {
            day: d.day,
            slices: d.slices.map(|list| {
                list.into_iter()
                    .map(|s| BreakdownSlice { key: s.key, label: s.label, tokens: s.tokens })
                    .collect()
            }),
        })
        .collect())
}

// ---------- 数据洞察命令（credit 月报 + 时间范围序列,只读增量扩展） ----------

/// credit 月报（daily_usage 源本地积分聚合）。口径见 store.credit_summary:
/// 总量=CodeBuddy+WorkBuddy 共享积分池;模型分布=CodeBuddy 行;无数据≠0（has_data)。
#[derive(Debug, Serialize)]
pub struct CreditModelSlice {
    pub key: String,
    pub label: String,
    pub credit: f64,
    pub requests: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreditDayPoint {
    pub day: String,
    pub credit: f64,
}

/// credit 按模型×日序列（双组图）。
#[derive(Debug, Serialize)]
pub struct CreditModelDaySlice {
    pub key: String,
    pub label: String,
    pub by_day: Vec<CreditDayPoint>,
}

#[derive(Debug, Serialize)]
pub struct CreditSummaryResult {
    pub month: String,
    pub has_data: bool,
    pub total_credit: f64,
    pub total_requests: i64,
    pub by_model: Vec<CreditModelSlice>,
    pub by_day: Vec<CreditDayPoint>,
    /// 模型维逐日 credit:连续日轴（1 号 → min（月末, 今日)）已补零,便于前端直画。
    pub by_model_day: Vec<CreditModelDaySlice>,
}

#[tauri::command]
pub fn get_credit_summary(month: String, state: State<'_, AppState>) -> Result<CreditSummaryResult, String> {
    let sum = with_reader(&state, |store| {
        Ok(store.credit_summary(&month))
    })?
    .ok_or_else(|| format!("invalid month: {}", month))?;

    // 连续日轴:1 号 → min（月末, 今天)。积分账本只可能覆盖到今天（未来无行）,
    // 未来日不进轴——前端「最右 = 今日」语义只对 tokens 柱成立,积分曲线画到
    // 轴末（导出覆盖的最后一天）,缺口以断线表达,不伪装成 0。
    let axis_end = credit_axis_end(&month);
    let axis: Vec<String> = credit_month_axis(&month, &axis_end);
    let with_axis = |cells: Vec<CreditDayPoint>| -> Vec<CreditDayPoint> {
        axis.iter()
            .map(|d| {
                cells
                    .iter()
                    .find(|c| &c.day == d)
                    .cloned()
                    .unwrap_or(CreditDayPoint { day: d.clone(), credit: 0.0 })
            })
            .collect()
    };

    Ok(CreditSummaryResult {
        month,
        has_data: sum.has_data,
        total_credit: sum.total_credit,
        total_requests: sum.total_requests,
        by_model: sum
            .by_model
            .into_iter()
            .map(|r| CreditModelSlice { key: r.key, label: r.label, credit: r.credit, requests: r.requests })
            .collect(),
        by_day: with_axis(
            sum.by_day
                .into_iter()
                .map(|d| CreditDayPoint { day: d.day, credit: d.credit })
                .collect(),
        ),
        by_model_day: sum
            .by_model_day
            .into_iter()
            .map(|m| CreditModelDaySlice {
                key: m.key,
                label: m.label,
                by_day: with_axis(m.by_day.into_iter().map(|d| CreditDayPoint { day: d.day, credit: d.credit }).collect()),
            })
            .collect(),
    })
}

/// 积分月轴终点:min（月末, 今天)（未来日无行,不进轴）。
fn credit_axis_end(month: &str) -> String {
    let today = chrono::Local::now().date_naive().format("%Y-%m-%d").to_string();
    let month_end = {
        let (y, m) = parse_month_ym(month);
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        let first_next = chrono::NaiveDate::from_ymd_opt(ny, nm, 1).unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(y, m, 1).unwrap());
        (first_next - chrono::Duration::days(1)).format("%Y-%m-%d").to_string()
    };
    if today < month_end { today } else { month_end }
}

fn credit_month_axis(month: &str, end_day: &str) -> Vec<String> {
    let (y, m) = parse_month_ym(month);
    let mut out = Vec::new();
    let mut cur = match chrono::NaiveDate::from_ymd_opt(y, m, 1) {
        Some(d) => d,
        None => return out,
    };
    let end = chrono::NaiveDate::parse_from_str(end_day, "%Y-%m-%d").unwrap_or(cur);
    while cur <= end {
        out.push(cur.format("%Y-%m-%d").to_string());
        cur += chrono::Duration::days(1);
    }
    out
}

/// "YYYY-MM" → （year, month);非法返回 （0, 0)（调用方 NaiveDate:from_ymd_opt 会兜掉）。
fn parse_month_ym(month: &str) -> (i32, u32) {
    let bytes = month.as_bytes();
    if bytes.len() == 7 && bytes[4] == b'-' {
        if let (Ok(y), Ok(m)) = (month[..4].parse::<i32>(), month[5..7].parse::<u32>()) {
            if (1..=12).contains(&m) {
                return (y, m);
            }
        }
    }
    (0, 0)
}

/// 时间范围序列：后端按 bucket 聚合的连续序列 + 维度系列。
/// bucket: day|hour（hour 轴 = day+HH 本地时）;dimension: agent|model|total;
/// metric: total|input|output;filter_dimension/filter_key 可选收窄。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RangeSeriesQuery {
    pub start_day: String,
    pub end_day: String,
    pub bucket: String,
    pub dimension: String,
    pub metric: String,
    pub filter_dimension: Option<String>,
    pub filter_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RangeSeriesPointOut {
    pub bucket: String,
    pub values: Vec<i64>,
}

#[derive(Debug, Serialize)]
pub struct RangeSeriesResult {
    pub series_keys: Vec<String>,
    pub series_labels: Vec<String>,
    pub points: Vec<RangeSeriesPointOut>,
}

#[tauri::command]
pub fn get_range_series(query: RangeSeriesQuery, state: State<'_, AppState>) -> Result<RangeSeriesResult, String> {
    if query.bucket != "day" && query.bucket != "hour" {
        return Err(format!("unsupported bucket: {} (day|hour)", query.bucket));
    }
    if !matches!(query.metric.as_str(), "total" | "input" | "output") {
        return Err(format!("unsupported metric: {}", query.metric));
    }
    // hour 粒度防御性限长（24×366 ≈ 全年小时点内）
    let filter = match (query.filter_dimension, query.filter_key) {
        (Some(d), Some(k)) => {
            if d != "agent" && d != "model" {
                return Err(format!("unsupported filter dimension: {d}"));
            }
            Some((d, k))
        }
        _ => None,
    };
    let series = with_reader(&state, |store| {
        let filter = filter.as_ref().map(|(d, k)| (d.as_str(), k.as_str()));
        Ok(store.range_series(&query.start_day, &query.end_day, &query.bucket, &query.dimension, &query.metric, filter))
    })?
    .ok_or_else(|| "invalid range or dimension".to_string())?;
    Ok(RangeSeriesResult {
        series_keys: series.series_keys,
        series_labels: series.series_labels,
        points: series
            .points
            .into_iter()
            .map(|p| RangeSeriesPointOut { bucket: p.bucket, values: p.values })
            .collect(),
    })
}

// ---------- 项目维与任务命令（只读 daily_project / turn / session,与既有命令并列） ----------
//
// 口径见 collector/task_query.rs 文件头。任务类型里的 `title` 是内容列:只随 IPC 回 UI,
// 任何导出 / 文件序列化路径一律不得引用这些类型（export_month 只读 daily_usage）。

pub use crate::collector::task_query::{DaySpan, GapHistogram, TaskFilters, TaskPage, TaskPageReq, TaskSort, TaskTurn, TimelineResult};
use crate::collector::project_meta::{self, ProjectMetaInput, ProjectMetaList, ScratchRule};
use crate::collector::task_store;

/// 本地日闭区间 [start_day, end_day]（"YYYY-MM-DD"）。
#[derive(Debug, Deserialize)]
pub struct DayRange {
    pub start_day: String,
    pub end_day: String,
}

/// 范围上限（天）:防御性限长,约十年。
const MAX_RANGE_DAYS: i64 = 3660;

fn parse_range(range: &DayRange) -> Result<(chrono::NaiveDate, chrono::NaiveDate), String> {
    let parse = |d: &str| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| format!("invalid day: {d}"));
    let (start, end) = (parse(&range.start_day)?, parse(&range.end_day)?);
    if end < start {
        return Err("end_day before start_day".into());
    }
    if (end - start).num_days() > MAX_RANGE_DAYS {
        return Err(format!("range too long (max {MAX_RANGE_DAYS} days)"));
    }
    Ok((start, end))
}

/// 日闭区间 → 本地时间 [start 0 点, end 次日 0 点) 毫秒（DST 缺失的本地 0 点取最早有效时刻）。
fn range_millis(range: &DayRange) -> Result<(i64, i64), String> {
    use chrono::TimeZone;
    let (start, end) = parse_range(range)?;
    let midnight = |d: chrono::NaiveDate| -> Result<i64, String> {
        let naive = d.and_hms_opt(0, 0, 0).ok_or("invalid day")?;
        Local
            .from_local_datetime(&naive)
            .earliest()
            .map(|dt| dt.timestamp_millis())
            .ok_or_else(|| "invalid local midnight".to_string())
    };
    Ok((midnight(start)?, midnight(end + chrono::Duration::days(1))?))
}

/// 项目维月度矩阵（形状 = get_monthly_matrix）。group_by: project | agent | model;
/// metric: total | input | output | turns | model_calls | tool_calls | wait（Σ wall_ms）| human（Σ idle_ms）。
/// message_counts = 该格对话轮次（Σ turns）。
#[tauri::command(rename_all = "snake_case")]
pub fn get_project_month_rows(month: String, group_by: String, metric: String, state: State<'_, AppState>) -> Result<MatrixResult, String> {
    if !matches!(group_by.as_str(), "project" | "agent" | "model") {
        return Err(format!("unsupported group_by: {group_by}"));
    }
    if crate::collector::task_query::project_metric_col(&metric).is_none() {
        return Err(format!("unsupported metric: {metric}"));
    }
    let dim = month_dim(&month)?;
    let rows = with_reader(&state, |store| Ok(store.project_month_rows(&month, &group_by, &metric, today(), &project_meta::scratch_rule())))?
        .ok_or_else(|| format!("invalid month: {month}"))?;
    Ok(MatrixResult {
        month,
        days_in_month: dim,
        generated_at: now_millis(),
        rows: rows
            .into_iter()
            .map(|r| MatrixRow { key: r.key, label: r.label, values: r.values, message_counts: r.message_counts, month_total: r.month_total })
            .collect(),
    })
}

/// 项目维行钻取（形状 = get_breakdown,tokens = total）:kind = project → 每日 Agent 构成;
/// kind = agent | model → 每日项目构成。
#[tauri::command]
pub fn get_project_breakdown(kind: String, key: String, month: String, state: State<'_, AppState>) -> Result<Vec<BreakdownDay>, String> {
    month_dim(&month)?;
    let days = with_reader(&state, |store| Ok(store.project_breakdown(&kind, &key, &month, today(), &project_meta::scratch_rule())))?
        .ok_or_else(|| format!("unsupported kind: {kind}"))?;
    Ok(days
        .into_iter()
        .map(|d| BreakdownDay {
            day: d.day,
            slices: d.slices.map(|list| list.into_iter().map(|s| BreakdownSlice { key: s.key, label: s.label, tokens: s.tokens }).collect()),
        })
        .collect())
}

/// 任务列表（有轮的根会话,会话开始时间落在范围内;子会话已并入父任务、不单独出现）。
/// filters / sort / page 缺省:不过滤 / started_at 降序 / 前 50 条（limit ≤ 500）。
#[tauri::command]
pub fn get_task_list(
    range: DayRange,
    filters: Option<TaskFilters>,
    sort: Option<TaskSort>,
    page: Option<TaskPageReq>,
    state: State<'_, AppState>,
) -> Result<TaskPage, String> {
    let (start_ms, end_ms) = range_millis(&range)?;
    let sort = sort.unwrap_or_default();
    with_reader(&state, |store| {
        Ok(store.task_list(start_ms, end_ms, &filters.unwrap_or_default(), &sort, &page.unwrap_or_default(), &project_meta::scratch_rule()))
    })?
    .ok_or_else(|| format!("unsupported sort field: {}", sort.field))
}

/// 单会话逐轮明细（物化层,按开始时间 1..n;子会话 id → 空列表）。
#[tauri::command(rename_all = "snake_case")]
pub fn get_task_turns(agent: String, session_id: String, state: State<'_, AppState>) -> Result<Vec<TaskTurn>, String> {
    with_reader(&state, |store| store.task_turns(&agent, &session_id))
}

/// 曲线维度筛选（可选）:dimension = agent | model | project。
#[derive(Debug, Deserialize)]
pub struct SeriesFilter {
    pub dimension: String,
    pub key: String,
}

/// 时间成本曲线（形状 = get_range_series,读 daily_project）。bucket 仅 day（不支持 hour）;
/// dimension: agent | model | project | total;metric 同 get_project_month_rows。
#[tauri::command]
pub fn get_effort_series(
    range: DayRange,
    bucket: String,
    dimension: String,
    metric: String,
    filter: Option<SeriesFilter>,
    state: State<'_, AppState>,
) -> Result<RangeSeriesResult, String> {
    if bucket != "day" {
        return Err(format!("unsupported bucket: {bucket} (only 'day')"));
    }
    if crate::collector::task_query::project_metric_col(&metric).is_none() {
        return Err(format!("unsupported metric: {metric}"));
    }
    parse_range(&range)?;
    let series = with_reader(&state, |store| {
        let f = filter.as_ref().map(|f| (f.dimension.as_str(), f.key.as_str()));
        Ok(store.effort_series(&range.start_day, &range.end_day, &bucket, &dimension, &metric, f, &project_meta::scratch_rule()))
    })?
    .ok_or_else(|| "invalid dimension or filter".to_string())?;
    Ok(RangeSeriesResult {
        series_keys: series.series_keys,
        series_labels: series.series_labels,
        points: series.points.into_iter().map(|p| RangeSeriesPointOut { bucket: p.bucket, values: p.values }).collect(),
    })
}

/// 数据跨度（S4-R,时间过滤）:`project` 给定 → 该项目生命周期（daily_project 首末日）;
/// 省略 → 全部数据首末日（daily_usage ∪ daily_project,「All」范围起点）。无数据 → null。
#[tauri::command]
pub fn get_project_span(project: Option<String>, state: State<'_, AppState>) -> Result<Option<DaySpan>, String> {
    with_reader(&state, |store| Ok(store.data_span(project.as_deref().filter(|p| !p.is_empty()), &project_meta::scratch_rule())))
}

/// 项目推进时间轴:from..to 含端点（本地日 YYYY-MM-DD）;项目集 = 全部可见有效项目
/// （前端按 pin + 窗口容量裁剪）;today = 本地日历日（与 Matrix 同口径）。只读。
#[tauri::command]
pub fn get_project_timeline(from: String, to: String, state: State<'_, AppState>) -> Result<TimelineResult, String> {
    with_reader(&state, |store| Ok(store.project_timeline(&from, &to, today(), &project_meta::scratch_rule())))?
        .ok_or_else(|| "invalid range".to_string())
}

/// 注意力快照:会话级 running / waiting / tool_pending,项目键为原始目录键
/// （前端经 effective_key 折叠到项目行）。只读内存表,不碰库。
#[tauri::command]
pub fn get_attention(state: State<'_, AppState>) -> Result<Vec<collector::attention::AttentionItem>, String> {
    let table = state.attention.lock().map_err(|_| "attention lock poisoned".to_string())?;
    Ok(table.items(collector::store::now_millis(), task_store::idle_threshold_ms()))
}

/// 确认一个会话当前这一段等待（同一会话下一次进入等待自动复位）。有变化才广播。
#[tauri::command(rename_all = "snake_case")]
pub fn ack_attention(agent: String, session_id: String, app: AppHandle, state: State<'_, AppState>) -> Result<bool, String> {
    let changed = state
        .attention
        .lock()
        .map_err(|_| "attention lock poisoned".to_string())?
        .ack(&agent, &session_id, collector::store::now_millis(), task_store::idle_threshold_ms());
    if changed {
        collector::notify_attention(&app);
    }
    Ok(changed)
}

/// 轮间空档直方图（gap_ms 对数分桶 + 当前阈值两侧合计;按轮的本地日过滤）。
#[tauri::command]
pub fn get_gap_histogram(range: DayRange, state: State<'_, AppState>) -> Result<GapHistogram, String> {
    parse_range(&range)?;
    with_reader(&state, |store| Ok(store.gap_histogram(&range.start_day, &range.end_day, task_store::idle_threshold_ms(), &project_meta::scratch_rule())))?
        .ok_or_else(|| "invalid range".to_string())
}

#[derive(Debug, Serialize)]
pub struct IdleThresholdInfo {
    pub minutes: u32,
    pub default_minutes: u32,
    pub min_minutes: u32,
    pub max_minutes: u32,
}

#[derive(Debug, Serialize)]
pub struct IdleThresholdApplied {
    pub minutes: u32,
    /// 重算覆盖的 （agent, 日) 格数。
    pub recomputed_days: usize,
    /// 全表重算耗时（毫秒,不含 prefs 落盘）。
    pub elapsed_ms: u64,
}

fn idle_threshold_info() -> IdleThresholdInfo {
    IdleThresholdInfo {
        minutes: (task_store::idle_threshold_ms() / 60_000) as u32,
        default_minutes: (crate::collector::store::IDLE_THRESHOLD_MS / 60_000) as u32,
        min_minutes: task_store::IDLE_THRESHOLD_MIN_MINUTES,
        max_minutes: task_store::IDLE_THRESHOLD_MAX_MINUTES,
    }
}

/// 启动时从 prefs.json 载入离开阈值（缺键 / 非法 → 默认 30 分钟）。须早于采集线程 spawn。
pub fn load_idle_threshold(app: &AppHandle) {
    let Ok(dr) = crate::data_root::current(app) else { return };
    if let Some(m) = std::fs::read_to_string(dr.prefs_path()).ok().as_deref().and_then(task_store::threshold_from_prefs) {
        task_store::set_idle_threshold_minutes(m);
    }
}

#[tauri::command]
pub fn get_idle_threshold() -> IdleThresholdInfo {
    idle_threshold_info()
}

/// 设置离开阈值（分钟）:合并写入 prefs.json `idleThresholdMin`（其余键原样）→ 下发运行时值 →
/// 同步重算 daily_project 全表（只读原始层,不动游标）→ emit `usage:changed`。
/// 重算走独立短连接的 IMMEDIATE 事务,与采集写串行（锁忙重试 3 次）;失败时运行时值与 prefs
/// 已生效,采集线程下次启动按阈值标记自愈。
#[tauri::command]
pub async fn set_idle_threshold(app: AppHandle, minutes: u32) -> Result<IdleThresholdApplied, String> {
    if !(task_store::IDLE_THRESHOLD_MIN_MINUTES..=task_store::IDLE_THRESHOLD_MAX_MINUTES).contains(&minutes) {
        return Err(format!(
            "minutes out of range ({}..={})",
            task_store::IDLE_THRESHOLD_MIN_MINUTES,
            task_store::IDLE_THRESHOLD_MAX_MINUTES
        ));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let path = crate::data_root::current(&app)?.prefs_path();
        let raw = std::fs::read_to_string(&path).ok();
        write_prefs_atomic(&path, &task_store::prefs_with_threshold(raw.as_deref(), minutes))?;
        task_store::set_idle_threshold_minutes(minutes);
        let started = std::time::Instant::now();
        let mut store = collector::open_store(&app)?;
        let mut attempt = 0;
        let recomputed_days = loop {
            match store.recompute_projects(task_store::idle_threshold_ms()) {
                Ok(n) => break n,
                Err(e) if attempt < 3 && (e.contains("locked") || e.contains("busy")) => attempt += 1,
                Err(e) => return Err(e),
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        crate::dev_log!("[collector] idle threshold {} min: recomputed {} day(s) in {} ms", minutes, recomputed_days, elapsed_ms);
        collector::notify_usage_changed(&app, &std::collections::BTreeSet::new());
        Ok(IdleThresholdApplied { minutes, recomputed_days, elapsed_ms })
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------- 项目管理 ----------

/// 独立短连接执行一次项目映射写入（IMMEDIATE 事务,与采集写串行;锁忙重试 3 次）→ emit `usage:changed`。
fn write_project_meta<T>(app: &AppHandle, f: impl Fn(&mut Store) -> Result<T, String>) -> Result<T, String> {
    let mut store = collector::open_store(app)?;
    let mut attempt = 0;
    let out = loop {
        match f(&mut store) {
            Ok(v) => break v,
            Err(e) if attempt < 3 && (e.contains("locked") || e.contains("busy")) => attempt += 1,
            Err(e) => return Err(e),
        }
    };
    collector::notify_usage_changed(app, &std::collections::BTreeSet::new());
    Ok(out)
}

/// 目录键 → 本机路径（只对形如盘符路径 / 绝对路径的键;unknown / Scratch → None）。
fn project_folder(key: &str) -> Option<std::path::PathBuf> {
    let b = key.as_bytes();
    let is_drive = b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic();
    if !(is_drive || key.starts_with('/')) {
        return None;
    }
    let path = std::path::PathBuf::from(if is_drive { key.replace('/', "\\") } else { key.to_string() });
    path.is_dir().then_some(path)
}

/// 管理面板列表:每个目录键一行（key / alias / hidden / merged_into / 状态 / agents / 会话 / 轮 / tokens / 首末日 /
/// 目录是否存在）+ Scratch 伪项目的隐藏态。状态按当前自动折叠规则解析。
#[tauri::command]
pub fn list_project_meta(state: State<'_, AppState>) -> Result<ProjectMetaList, String> {
    let mut list = with_reader(&state, |store| store.list_project_meta(&project_meta::scratch_rule()))?;
    for r in &mut list.rows {
        r.folder_exists = project_folder(&r.key).is_some();
    }
    Ok(list)
}

/// 单条 upsert:alias / note（trim 后 ≤ 120 字符,空 = 清除）、hidden;`reset = true` 删行回到自动态。
#[tauri::command]
pub async fn set_project_meta(app: AppHandle, input: ProjectMetaInput) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || write_project_meta(&app, |store| store.set_project_meta(&input)))
        .await
        .map_err(|e| e.to_string())?
}

/// 把 keys 合并进 into（一层;拒绝并入自身、目标已合并、源是合并目标、Scratch）。返回合并键数。
#[tauri::command]
pub async fn merge_projects(app: AppHandle, keys: Vec<String>, into: String) -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(move || write_project_meta(&app, |store| store.merge_projects(&keys, &into)))
        .await
        .map_err(|e| e.to_string())?
}

/// 取消合并（变空的 meta 行删除,回到自动态）。返回实际取消的键数。
#[tauri::command]
pub async fn unmerge_projects(app: AppHandle, keys: Vec<String>) -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(move || write_project_meta(&app, |store| store.unmerge_projects(&keys)))
        .await
        .map_err(|e| e.to_string())?
}

#[derive(Debug, Serialize)]
pub struct ScratchRuleInfo {
    pub rule: ScratchRule,
    pub defaults: ScratchRule,
    pub min_sessions_bounds: (u32, u32),
    pub min_turns_bounds: (u32, u32),
}

fn scratch_rule_info() -> ScratchRuleInfo {
    ScratchRuleInfo {
        rule: project_meta::scratch_rule(),
        defaults: ScratchRule::DEFAULT,
        min_sessions_bounds: project_meta::SCRATCH_MIN_SESSIONS_BOUNDS,
        min_turns_bounds: project_meta::SCRATCH_MIN_TURNS_BOUNDS,
    }
}

/// 启动时从 prefs.json 载入自动折叠规则（缺键 / 非法逐键回落默认）。与离开阈值同处调用。
pub fn load_scratch_rule(app: &AppHandle) {
    let Ok(dr) = crate::data_root::current(app) else { return };
    if let Ok(raw) = std::fs::read_to_string(dr.prefs_path()) {
        project_meta::set_scratch_rule(ScratchRule::from_prefs(&raw));
    }
}

#[tauri::command]
pub fn get_scratch_rule() -> ScratchRuleInfo {
    scratch_rule_info()
}

/// 设置自动折叠规则:校验 → 合并写 prefs.json `scratch*` 四键（其余键原样）→ 下发运行时值 → emit
/// `usage:changed`。规则只作用于查询时的解析层,不重算任何表。前端成功后须同步 setDesignPrefs 四键。
#[tauri::command]
pub async fn set_scratch_rule(app: AppHandle, rule: ScratchRule) -> Result<ScratchRuleInfo, String> {
    rule.validate()?;
    tauri::async_runtime::spawn_blocking(move || {
        let path = crate::data_root::current(&app)?.prefs_path();
        let raw = std::fs::read_to_string(&path).ok();
        write_prefs_atomic(&path, &rule.merge_into_prefs(raw.as_deref()))?;
        project_meta::set_scratch_rule(rule);
        collector::notify_usage_changed(&app, &std::collections::BTreeSet::new());
        Ok(scratch_rule_info())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 在文件管理器中打开项目目录（只对库中已知且本机存在的目录键）。
#[tauri::command]
pub fn open_project_folder(key: String, state: State<'_, AppState>) -> Result<(), String> {
    if !with_reader(&state, |store| Ok(store.project_key_known(&key)))? {
        return Err("unknown project".into());
    }
    let path = project_folder(&key).ok_or_else(|| "folder not found".to_string())?;
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer").arg(path).spawn().map_err(|e| e.to_string())?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err("unsupported platform".into())
    }
}

// ---------- Collector 命令（真实源健康面板） ----------

/// 四源固定列出：probe 实时探测路径/schema,历史健康与计数读 source_state。
#[tauri::command]
pub fn list_sources(state: State<'_, AppState>) -> Result<Vec<SourceSummary>, String> {
    let adapters = collector::default_adapters();
    let summaries = with_reader(&state, |store| {
        let now = now_millis();
        let stale_after_ms = (10 * 60 * 1000).max(3 * collector::poll_interval().as_millis() as i64);
        Ok(adapters
            .iter()
            .map(|ad| {
                let meta = ad.meta();
                let probe = ad.probe();
                let st = store.source_state(meta.id);
                // stale：采集循环停摆（上次尝试距今 > max（10 分钟, 3 × 采集频率)）且源并非缺失
                let stale = probe.status != "no_source"
                    && st.last_attempt_at.map(|t| now - t > stale_after_ms).unwrap_or(false);
                SourceSummary {
                    id: meta.id.to_string(),
                    adapter_id: meta.id.to_string(),
                    adapter_name: meta.name.to_string(),
                    location: meta.location.to_string(),
                    kind: meta.kind.to_string(),
                    probe_status: probe.status,
                    schema_fingerprint: probe.fingerprint.or(st.schema_fingerprint),
                    last_success_at: millis_to_rfc3339(st.last_success_at),
                    last_attempt_at: millis_to_rfc3339(st.last_attempt_at),
                    last_error_code: st.last_error_code,
                    last_error_message: st.last_error_message,
                    events_collected: st.events_collected,
                    stale,
                }
            })
            .collect())
    })?;
    Ok(summaries)
}

#[tauri::command]
pub fn get_paused(state: State<'_, AppState>) -> bool {
    state.paused.load(Ordering::SeqCst)
}

#[tauri::command]
pub fn set_paused(paused: bool, state: State<'_, AppState>) {
    state.paused.store(paused, Ordering::SeqCst);
}

// ---------- 采集频率（设置·General Collection 组;运行时值 = collector 原子量,持久化 = prefs.json） ----------

#[derive(Debug, Serialize)]
pub struct CollectIntervalInfo {
    pub secs: u64,
    pub default_secs: u64,
    pub choices: Vec<u64>,
}

fn collect_interval_info() -> CollectIntervalInfo {
    CollectIntervalInfo {
        secs: collector::poll_interval().as_secs(),
        default_secs: collector::POLL_INTERVAL_DEFAULT_SECS,
        choices: collector::POLL_INTERVAL_CHOICES_SECS.to_vec(),
    }
}

/// 启动时从 prefs.json 载入采集频率（缺键 / 非档位值 → 默认 30s）。须早于采集线程 spawn。
pub fn load_collect_interval(app: &AppHandle) {
    let Ok(dr) = crate::data_root::current(app) else { return };
    if let Some(secs) = std::fs::read_to_string(dr.prefs_path()).ok().as_deref().and_then(collector::poll_interval_from_prefs) {
        collector::set_poll_interval_secs(secs);
    }
}

#[tauri::command]
pub fn get_collect_interval() -> CollectIntervalInfo {
    collect_interval_info()
}

/// 设置采集频率:校验档位 → 合并写 prefs.json `collectIntervalSecs`（其余键原样）→ 下发运行时值。
/// 采集线程睡眠按秒分片重读频率,改档即时生效（不触发额外一轮采集）。
#[tauri::command]
pub fn set_collect_interval(app: AppHandle, secs: u64) -> Result<CollectIntervalInfo, String> {
    if !collector::POLL_INTERVAL_CHOICES_SECS.contains(&secs) {
        return Err(format!("interval must be one of {:?} seconds", collector::POLL_INTERVAL_CHOICES_SECS));
    }
    let path = crate::data_root::current(&app)?.prefs_path();
    let raw = std::fs::read_to_string(&path).ok();
    write_prefs_atomic(&path, &collector::prefs_with_poll_interval(raw.as_deref(), secs))?;
    collector::set_poll_interval_secs(secs);
    Ok(collect_interval_info())
}

// ---------- Snap 开关（状态单一源 = AppState + window-state.json） ----------

#[tauri::command]
pub fn get_snap_enabled(app: AppHandle) -> bool {
    app.state::<AppState>().widget_snap_enabled.load(Ordering::SeqCst)
}

/// 开关即时生效：写 AppState + 即时落盘（下次拖动松手按新值判定，无需重启）。
#[tauri::command]
pub fn set_snap_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.widget_snap_enabled.store(enabled, Ordering::SeqCst);
    crate::window_state::persist(&app, &state);
    Ok(())
}

// ---------- 开机自启（设置·General；系统启动项是唯一事实源） ----------

/// dev 构建不给改自启（同「应用更新」的 dev 纪律）：`tauri dev` 跑的是
/// target/debug 下的开发二进制，注册成自启等于每次开机拉起一个开发进程。
const AUTOSTART_EDITABLE: bool = !cfg!(debug_assertions);

/// 自启状态载荷。**前端不落 prefs.json 镜像**——勾选态现查现显示，与
/// 「可见性单一源」同款口径：系统启动项被外部改动（Windows 设置·启动应用）
/// 也能如实反映。
#[derive(Debug, Serialize)]
pub struct AutostartInfo {
    /// 是否已注册自启。插件口径 = HKCU Run 存在该值 **且** 未被 Windows
    /// 「启动应用」禁用（StartupApproved 覆写）；值名 = productName，与 NSIS
    /// 卸载清理的 ${PRODUCTNAME} 同名，卸载即清干净（更新安装不动它）。
    pub enabled: bool,
    /// 当前构建是否允许改动（false = dev，前端据此禁用勾选框）。
    pub supported: bool,
}

#[tauri::command]
pub fn get_autostart(app: AppHandle) -> Result<AutostartInfo, String> {
    Ok(AutostartInfo {
        enabled: app.autolaunch().is_enabled().map_err(|e| e.to_string())?,
        supported: AUTOSTART_EDITABLE,
    })
}

/// 把自启设成 `enabled` 目标态（幂等：已是目标态不重复写注册表），写完回读。
/// 写入值 = 当前 exe 路径（插件取 current_exe，不附加参数）——启动落点与前一次
/// 会话一致：挂件/悬浮球按 window-state.json 恢复，主窗口保持隐藏。
#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<AutostartInfo, String> {
    if !AUTOSTART_EDITABLE {
        return Err("autostart is not editable in dev builds".into());
    }
    let manager = app.autolaunch();
    if manager.is_enabled().map_err(|e| e.to_string())? != enabled {
        let res = if enabled { manager.enable() } else { manager.disable() };
        res.map_err(|e| e.to_string())?;
    }
    Ok(AutostartInfo {
        enabled: manager.is_enabled().map_err(|e| e.to_string())?,
        supported: true,
    })
}

// ---------- Export 命令（聚合明细落盘） ----------

fn export_month(app: &AppHandle, month: &str, format: &str) -> Result<ExportResult, String> {
    // 校验月份格式（与旧契约一致）
    month_dim(month)?;

    let rows: Vec<(String, String, String, i64)> = {
        let state = app.state::<AppState>();
        with_reader(&state, |store| store.export_rows(month))?
    };

    let dir = crate::data_root::current(app)?.exports_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(format!("tokencalendar-{}.{}", month, format));

    match format {
        "csv" => {
            let mut out = String::from("day,agent,model,total_tokens\n");
            for (day, agent, model, tokens) in &rows {
                out.push_str(&format!("{},{},{},{}\n", day, agent, model, tokens));
            }
            std::fs::write(&file, out).map_err(|e| e.to_string())?;
        }
        _ => {
            let payload = serde_json::json!({
                "month": month,
                "generated_at": now_millis(),
                "rows": rows.iter().map(|(day, agent, model, tokens)| serde_json::json!({
                    "day": day, "agent": agent, "model": model, "total_tokens": tokens
                })).collect::<Vec<_>>(),
            });
            std::fs::write(&file, serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(ExportResult { path: file.display().to_string(), rows: rows.len(), format: format.to_string() })
}

#[tauri::command]
pub fn export_month_csv(app: AppHandle, month: String) -> Result<ExportResult, String> {
    export_month(&app, &month, "csv")
}

#[tauri::command]
pub fn export_month_json(app: AppHandle, month: String) -> Result<ExportResult, String> {
    export_month(&app, &month, "json")
}

// ---------- 数据管理命令 ----------

#[derive(Debug, Serialize)]
pub struct DataInfo {
    /// 当前生效数据根。
    pub root: String,
    /// 用户自定义根（指针记录;null = 默认根）。
    pub custom_root: Option<String>,
    /// 默认根（exe 旁 data）。
    pub default_root: String,
    /// 生效根是否为权限回退根（默认根不可写 → LOCALAPPDATA）。
    pub fell_back: bool,
    /// 各项占用（字节;粗粒度诊断展示）。
    pub db_bytes: u64,
    pub exports_count: u64,
}

fn dir_file_count(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir).map(|it| it.filter_map(Result::ok).count() as u64).unwrap_or(0)
}

#[tauri::command]
pub fn get_data_info(app: AppHandle) -> Result<DataInfo, String> {
    let dr = crate::data_root::current(&app)?;
    let db_bytes = std::fs::metadata(dr.db_path()).map(|m| m.len()).unwrap_or(0)
        + std::fs::metadata(dr.db_path().with_extension("db-wal")).map(|m| m.len()).unwrap_or(0);
    Ok(DataInfo {
        root: dr.root.display().to_string(),
        custom_root: dr.custom.as_ref().map(|p| p.display().to_string()),
        default_root: dr.default_root.display().to_string(),
        fell_back: dr.fell_back,
        db_bytes,
        exports_count: dir_file_count(&dr.exports_dir()),
    })
}

/// 迁移数据根到新目录：复制全部自有数据（db 含 wal/shm 一并拷贝,目标先建
/// 目录）→ 写指针 → 返回提示（重启生效;旧目录留给用户自管,程序不删）。
/// 迁移期间采集可能仍在写库 → 先要求暂停（前端先 set_paused（true)）,
/// 且拷贝的 wal 以「拷贝后首启由 SQLite 回放」为准,不做在线 checkpoint。
#[tauri::command]
pub fn migrate_data_root(app: AppHandle, new_root: String) -> Result<String, String> {
    let old = crate::data_root::current(&app)?;
    let dest = std::path::PathBuf::from(&new_root);
    if !dest.is_absolute() {
        return Err("path must be absolute".into());
    }
    if dest == old.root {
        return Err("new root equals current root".into());
    }
    std::fs::create_dir_all(&dest).map_err(|e| format!("create dest: {}", e))?;
    let paused = app.state::<AppState>();
    if !paused.paused.load(Ordering::SeqCst) {
        return Err("pause collecting before migration".into());
    }
    // 拷贝数据根下全部条目（db/偏好/窗口态/exports 等;子目录递归）
    copy_dir_recursive(&old.root, &dest)?;
    crate::data_root::write_pointer(&app, &dest)?;
    Ok(format!("migrated to {}; restart to take effect", dest.display()))
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())?.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if let Err(e) = std::fs::copy(&from, &to) {
            // 单文件失败不拖垮整体（容错铁律）:记录并继续
            eprintln!("[migrate] copy {} failed: {}", from.display(), e);
        }
    }
    Ok(())
}

/// 备份：SQLite `VACUUM INTO` 导出一致性快照（运行中安全）+ prefs.json +
/// window-state.json → 指定目录。返回快照文件路径。
#[tauri::command]
pub fn backup_data(app: AppHandle, dest_dir: String) -> Result<ExportResult, String> {
    let dr = crate::data_root::current(&app)?;
    let dest = std::path::PathBuf::from(&dest_dir);
    if !dest.is_absolute() {
        return Err("path must be absolute".into());
    }
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let snapshot = dest.join(format!("tokencalendar-backup-{}.db", stamp));
    // VACUUM INTO 经命令线程读连接执行（读锁与采集写互不阻塞）
    {
        let state = app.state::<AppState>();
        with_reader(&state, |store| store.vacuum_into(&snapshot))?;
    }
    for f in ["prefs.json", "window-state.json"] {
        let from = dr.root.join(f);
        if from.is_file() {
            let _ = std::fs::copy(&from, dest.join(f));
        }
    }
    Ok(ExportResult { path: snapshot.display().to_string(), rows: 0, format: "backup".into() })
}

/// 恢复：从备份目录读取快照 + 偏好 → 覆盖当前数据根。要求先暂停采集。
/// 返回重启提示。
#[tauri::command]
pub fn restore_data(app: AppHandle, backup_dir: String, snapshot: Option<String>) -> Result<String, String> {
    let dr = crate::data_root::current(&app)?;
    let src = std::path::PathBuf::from(&backup_dir);
    if !src.is_absolute() || !src.is_dir() {
        return Err("backup dir invalid".into());
    }
    let paused = app.state::<AppState>();
    if !paused.paused.load(Ordering::SeqCst) {
        return Err("pause collecting before restore".into());
    }
    // 快照定位:显式给定 > 目录内最新 tokencalendar-backup-*.db
    let snap = match snapshot {
        Some(s) => std::path::PathBuf::from(s),
        None => {
            let mut cands: Vec<std::path::PathBuf> = std::fs::read_dir(&src)
                .map_err(|e| e.to_string())?
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name().map(|n| {
                        n.to_string_lossy().starts_with("tokencalendar-backup-")
                            && n.to_string_lossy().ends_with(".db")
                    }).unwrap_or(false)
                })
                .collect();
            cands.sort();
            cands.pop().ok_or("no backup snapshot in dir")?
        }
    };
    if !snap.is_file() {
        return Err("snapshot file missing".into());
    }
    std::fs::copy(&snap, dr.db_path()).map_err(|e| format!("copy snapshot: {}", e))?;
    for f in ["prefs.json", "window-state.json"] {
        let from = src.join(f);
        if from.is_file() {
            let _ = std::fs::copy(&from, dr.root.join(f));
        }
    }
    Ok("restored; restart to take effect".into())
}

/// 在系统文件管理器中打开数据根（诊断/手动备份入口）。
#[tauri::command]
pub fn open_data_dir(app: AppHandle) -> Result<(), String> {
    let dr = crate::data_root::current(&app)?;
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(dr.root.display().to_string())
            .spawn()
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("unsupported platform".into())
    }
}

/// 系统浏览器打开外部链接（设置·About 的版本号按钮 → 公开仓 Releases 页）。
/// 只放行 https（前端传的是常量,仍做防御性校验;其余协议一律拒绝）。
/// 走 ShellExecuteW 而非 `cmd /C start`：URL 不经 shell 解析,无转义面。
#[tauri::command]
pub fn open_external_url(url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("only https urls are allowed".into());
    }
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        // SW_SHOWNORMAL 本地定义：与 snap.rs 的 WM_* 同款,不引
        // Win32_UI_WindowsAndMessaging feature。
        const SW_SHOWNORMAL: i32 = 1;
        let to_wide =
            |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
        let op = to_wide("open");
        let target = to_wide(&url);
        let ret = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                op.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        // 返回值 <= 32 为错误码（> 32 成功）；0 = 无关联程序。
        if ret as isize <= 32 {
            return Err(format!("ShellExecuteW failed (code {})", ret as isize));
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err("unsupported platform".into())
    }
}

// ---------- 用户偏好持久化（prefs.json,数据根内） ----------

/// 读偏好原文（JSON 串;文件不存在 → None）。前端负责 schema 清洗
/// （designPrefs.sanitize 单一源在 TS 侧,Rust 只做透明存取）。
#[tauri::command]
pub fn get_prefs_raw(app: AppHandle) -> Result<Option<String>, String> {
    let path = crate::data_root::current(&app)?.prefs_path();
    std::fs::read_to_string(path).map(Some).or_else(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(e.to_string())
        }
    })
}

/// 写偏好原文（前端 sanitize 后的完整 JSON;原子替换防写坏）。
#[tauri::command]
pub fn set_prefs_raw(app: AppHandle, json: String) -> Result<(), String> {
    // JSON 合法性防御（坏串不落盘）
    serde_json::from_str::<serde_json::Value>(&json).map_err(|e| format!("invalid json: {}", e))?;
    let path = crate::data_root::current(&app)?.prefs_path();
    write_prefs_atomic(&path, &json)
}

/// prefs.json 原子替换写（tmp + rename）。
fn write_prefs_atomic(path: &std::path::Path, json: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ---------- Window 命令（可见性原语在 visibility.rs，此处仅挂件尺寸） ----------

/// 恢复设计默认 widget 尺寸（重置按钮 / 宽高比锁回吸共用）。
/// 起直接作用于 widget 窗口，无模式判断；几何落盘由 Resized
/// 事件的节流持久化兜底。
///
/// `snap_anchor` = true 且吸附状态有效时，set_size
/// 后以**停靠顶点为锚**重算位置——窗口右上角保持在该顶点（-2 原点语义），
/// 停靠后切档位不再锚左上角漂移。缺省 None = 现状行为（只 set_size 不动位）；
/// 比例锁回写/重置按钮不传参（手动拉伸例外，F5 分流）。
///
/// 坐标口径：SnapState.work/pitch 为物理像素快照；显示器变化时按当前工作区
/// 重算顶点绝对坐标（顶点索引相对右上角不变）。set_size 走 LogicalSize，
/// 新尺寸物理值 = 逻辑档位 × scale_factor。
#[tauri::command]
pub fn set_widget_size(
    app: AppHandle,
    width: f64,
    height: f64,
    snap_anchor: Option<bool>,
) -> Result<(), String> {
    let window = app.get_webview_window("widget").ok_or("widget window not found")?;
    window.set_size(tauri::LogicalSize::new(width, height)).map_err(|e| e.to_string())?;
    if snap_anchor == Some(true) {
        anchor_to_snap_vertex(&app, &window, width, height);
    }
    Ok(())
}

/// 前置：悬浮球尺寸切换——**仅程序化路径**（窗口
/// resizable=false 禁手动拉伸：悬浮球不需要任何形变功能）。
/// 无 snap_anchor 参数——orb 不参与格网吸附，尺寸切换不做顶点锚定。
///
/// 调用方 = **手动形态切换**（自由态展开/折叠、挂载时按当前态校准）。拖到屏幕
/// 边缘的自动形态切换（dock 收起 / undock 展开）不走这里——那两条路径的尺寸与
/// 位置归位由 orb_dock 原子完成（见 orb_dock:orb_undock / place_docked）；
/// 前端若在事件回调里再调本命令，位置补偿会被算两遍。
#[tauri::command]
pub fn set_orb_size(app: AppHandle, width: f64, height: f64) -> Result<(), String> {
    let window = app
        .get_webview_window(crate::visibility::ORB_LABEL)
        .ok_or("orb window not found")?;
    // 尺寸切换走「内容锚定」版本：容器绕表盘视觉中心对称后,两态内容偏移差很大
    // （16/16 vs 235/100）,裸 set_size 会让卡片在屏上平移。跨屏时按内容所在显示器
    // 的 scale 换算,并同步 Rust 侧形态状态（几何判定不再从窗口尺寸反推）。
    #[cfg(windows)]
    {
        crate::orb_dock::set_size_anchored(&window, width, height);
        Ok(())
    }
    #[cfg(not(windows))]
    window.set_size(tauri::LogicalSize::new(width, height)).map_err(|e| e.to_string())
}

/// 顶点锚重算：set_size 后右上角回贴 SnapState 停靠顶点。仅挂件使用
///。任何一步
/// 数据缺失（无吸附状态/无工作区）都静默跳过——锚定失败退化为现状行为
/// （只改尺寸），不阻塞档位切换。
fn anchor_to_snap_vertex(app: &AppHandle, window: &tauri::WebviewWindow, w: f64, _h: f64) {
    let Some(state) = crate::window_state::snap_state_for(app, window.label()) else { return };
    if state.pitch <= 0 {
        return;
    }
    let scale = window.scale_factor().unwrap_or(1.0);
    let Some(work) = current_work_rect(window) else { return };

    // 顶点绝对坐标（物理）= work.right − col·pitch, work.top + row·pitch。
    // work 取当前工作区：显示器/DPI 变化时顶点索引语义不变、绝对坐标随环境重算
    // （落盘快照仅作参考，不参与计算——避免陈旧矩形把窗口拉回旧屏）。
    let vx = work[2] - (state.vertex.col as i32) * state.pitch;
    let vy = work[1] + (state.vertex.row as i32) * state.pitch;
    // 新宽度物理值 = 逻辑档位 × scale（与 set_size 的 LogicalSize 同一换算）；
    // 高度无需参与定位——右上角锚定下 y 目标 = 顶点 y，set_size 自顶向下延展
    let new_w = (w * scale).round() as i32;
    let _ = window.set_position(tauri::PhysicalPosition::new(vx - new_w, vy));
}

/// 当前窗口所在显示器工作区矩形 [left, top, right, bottom]（物理像素）。
/// tauri:Monitor 无工作区，Win32 rcWork 直取；失败返回 None。
#[cfg(windows)]
fn current_work_rect(window: &tauri::WebviewWindow) -> Option<[i32; 4]> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    let hwnd = window.hwnd().ok()?;
    let hwnd = hwnd.0 as windows_sys::Win32::Foundation::HWND;
    let empty = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        rcMonitor: empty,
        rcWork: empty,
        dwFlags: 0,
    };
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() || GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
    }
    let rc = info.rcWork;
    Some([rc.left, rc.top, rc.right, rc.bottom])
}

/// 非 Windows 无 Win32 工作区来源，锚点重算静默跳过（与吸附检测同哲学）。
#[cfg(not(windows))]
fn current_work_rect(_window: &tauri::WebviewWindow) -> Option<[i32; 4]> {
    None
}

// ---------- 主窗口控制（自绘标题栏） ----------
// 按项目约定窗口操作 Rust：前端只发意图命令，不放开 core:window 权限面。

#[tauri::command]
pub fn main_minimize(app: AppHandle) -> Result<(), String> {
    let window = app.get_webview_window("main").ok_or("main window not found")?;
    window.minimize().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn main_toggle_maximize(app: AppHandle) -> Result<(), String> {
    let window = app.get_webview_window("main").ok_or("main window not found")?;
    if window.is_maximized().map_err(|e| e.to_string())? {
        window.unmaximize().map_err(|e| e.to_string())
    } else {
        window.maximize().map_err(|e| e.to_string())
    }
}

/// 关闭 = 隐藏（与 CloseRequested 语义一致，退出唯一入口在托盘）。
#[tauri::command]
pub fn main_close(app: AppHandle) -> Result<(), String> {
    crate::visibility::set_visible(&app, crate::visibility::MAIN_LABEL, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(a: &str, b: &str) -> DayRange {
        DayRange { start_day: a.into(), end_day: b.into() }
    }

    #[test]
    fn day_range_validation_and_local_bounds() {
        let (lo, hi) = range_millis(&range("2026-09-01", "2026-09-01")).unwrap();
        assert!((23 * 3_600_000..=25 * 3_600_000).contains(&(hi - lo)), "单日区间 ≈ 24h（DST 日 23/25h）");
        assert_eq!(crate::collector::millis_to_local_day(lo).as_deref(), Some("2026-09-01"));
        assert_eq!(crate::collector::millis_to_local_day(hi - 1).as_deref(), Some("2026-09-01"));
        assert!(parse_range(&range("2026-09-02", "2026-09-01")).is_err());
        assert!(parse_range(&range("2026-9-x", "2026-09-01")).is_err());
        assert!(parse_range(&range("2000-01-01", "2026-09-01")).is_err(), "超长范围");
    }

    #[test]
    fn prefs_atomic_write_merges_threshold() {
        let dir = std::env::temp_dir().join(format!("tc_prefs_{}", std::process::id()));
        let path = dir.join("prefs.json");
        write_prefs_atomic(&path, r#"{"locked":true}"#).unwrap();
        let raw = std::fs::read_to_string(&path).ok();
        write_prefs_atomic(&path, &task_store::prefs_with_threshold(raw.as_deref(), 45)).unwrap();
        let back = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(task_store::threshold_from_prefs(&back), Some(45));
        assert!(back.contains(r#""locked":true"#));
    }

    #[test]
    fn project_folder_only_for_existing_path_keys() {
        assert!(project_folder("unknown").is_none());
        assert!(project_folder(crate::collector::project_meta::SCRATCH_KEY).is_none());
        assert!(project_folder("relative/dir").is_none());
        let dir = std::env::temp_dir();
        let key = crate::collector::turns::normalize_project(&dir.display().to_string());
        assert!(project_folder(&key).is_some(), "{key}");
        assert!(project_folder(&format!("{key}/tc-no-such-dir-{}", std::process::id())).is_none());
    }

    #[test]
    fn idle_threshold_info_defaults() {
        let info = idle_threshold_info();
        assert_eq!((info.default_minutes, info.min_minutes, info.max_minutes), (30, 1, 1440));
        assert!((info.min_minutes..=info.max_minutes).contains(&info.minutes));
    }
}
