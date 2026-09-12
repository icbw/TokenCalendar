//! 前端命令面 = 冻结的数据契约（形状对齐旧项目 Wails 绑定，
//! 命名统一 snake_case）。起数据来源 = collector.db 聚合（fixture 退役为
//! 纯测试资产）；契约形状不变，仅 SourceSummary 增补契约已有的可选字段
//! schema_fingerprint。调整：可见性原语移入 visibility.rs。

use std::sync::atomic::Ordering;

use chrono::Local;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

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

/// credit 月报（request_model 对账表聚合）。口径见 store.credit_summary:
/// 总量=全表（含 WorkBuddy 共享池行);模型分布=非 WB 行;无数据≠0（has_data)。
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

// ---------- Collector 命令（真实源健康面板） ----------

/// 四源固定列出：probe 实时探测路径/schema,历史健康与计数读 source_state。
#[tauri::command]
pub fn list_sources(state: State<'_, AppState>) -> Result<Vec<SourceSummary>, String> {
    let adapters = collector::default_adapters();
    let summaries = with_reader(&state, |store| {
        let now = now_millis();
        Ok(adapters
            .iter()
            .map(|ad| {
                let meta = ad.meta();
                let probe = ad.probe();
                let st = store.source_state(meta.id);
                // stale：采集循环停摆（上次尝试距今 > 10 分钟）且源并非缺失
                let stale = probe.status != "no_source"
                    && st.last_attempt_at.map(|t| now - t > 10 * 60 * 1000).unwrap_or(false);
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
    pub imports_count: u64,
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
        imports_count: dir_file_count(&dr.imports_dir()),
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
    // 拷贝数据根下全部条目（db/偏好/窗口态/imports/exports;子目录递归）
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
    // imports 里的原始 xlsx/done 一并备份（体积小,保对账能力）
    if dr.imports_dir().is_dir() {
        let _ = copy_dir_recursive(&dr.imports_dir(), &dest.join("imports"));
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
    if src.join("imports").is_dir() {
        let _ = copy_dir_recursive(&src.join("imports"), &dr.imports_dir());
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

// ---------- CodeBuddy 官网导出手动导入 ----------

/// 手动导入一份官网导出 xlsx：解析校验 → 转存 imports 目录（文件名取自源
/// 文件,同名以 -1/-2 防覆盖,保留原始文件）→ 唤醒采集线程立即走既有
/// `process_imports` 管道（入库 request_model + 失效重扫 + usage:changed）。
/// 中间零延迟:唤醒后采集线程毫秒级消化,前端随后重查 credit/矩阵即见新数据。
/// 返回转存后的文件名（供 UI 展示）。重名内容重复导入由 request_model
/// upsert + `.xlsx.done` 后缀幂等兜底。
#[tauri::command]
pub fn import_codebuddy_file(app: AppHandle, source_path: String) -> Result<String, String> {
    use crate::collector::imports;

    let src = std::path::PathBuf::from(&source_path);
    if !src.is_file() {
        return Err("file not found".into());
    }
    if src.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("xlsx")) != Some(true) {
        return Err("only .xlsx exports are supported".into());
    }
    // 解析校验前置:坏文件/非导出表在这里报错给 UI,不落 imports 目录。
    let rows = imports::parse_export(&src)?;
    if rows.is_empty() {
        return Err("no usable rows in file".into());
    }

    let dr = crate::data_root::current(&app)?;
    let dir = dr.imports_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "codebuddy-export.xlsx".into());
    // 同名共存防覆盖:usage（1).xlsx → usage（1)-1.xlsx（-N 试到空位为止）。
    let mut dest = dir.join(&name);
    let mut nth = 0u32;
    while dest.exists() {
        nth += 1;
        let stem = name.strip_suffix(".xlsx").unwrap_or(&name);
        dest = dir.join(format!("{}-{}.xlsx", stem, nth));
    }
    std::fs::copy(&src, &dest).map_err(|e| format!("copy: {}", e))?;

    // 唤醒采集线程立即消化（sleep_interruptible 提前返回 → process_imports）。
    crate::collector::wake_imports();
    Ok(dest
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| name.clone()))
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
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
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
    // （16/16 vs 215/80）,裸 set_size 会让卡片在屏上平移。跨屏时按内容所在显示器
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
