//! ZCode 适配器：`~/.zcode/cli/db/db.sqlite` 的 `model_usage` 表（rowid 增量）。
//!
//! 口径（跟随旧项目）：total = `provider_total_tokens`，为 0/NULL 时兜底
//! `computed_total_tokens`；input/output 取原始列（cache 分项独立列,不并入）；
//! 仅 `status='completed'` 计入；时间优先 `started_at`（Unix 毫秒）→ 本地日。
//! 数据库被 ZCode 进程实时写入（WAL）：只读打开 + busy_timeout,BUSY 视为可重试降级。
//!
//! 请求数（S4-R 正,待观察 C）：行级增量**不再**按行写 request_count 粗值——旧做法「每行 +1、
//! 10 分钟节流后 distinct turn 重算覆盖」在活跃长轮里会把当天一格虚高,
//! 且节流窗口内最后一批之后若再无新行,虚高值一直留着。现在每次有新行都对涉及的日期范围
//! （非整月）即时重算 distinct turn（同一只读连接,无节流）;重算失败的日期挂起到下次采集重试。
//!
//! 轮与时间（精确值,不走累加器）：按会话重算覆盖——turn_usage（每轮一行:
//! started / completed / duration_ms = wall、time_to_first_token_ms = ttft、model_retry_count、
//! tool_call_count;`cancelled_by_user = 1` 或 `error_type = turn_cancelled` 记**中止**（S4-R,不计错）,
//! 其余 error_type 非空计错）+ model_usage（completed 行 = 模型调用与 token,
//! Σ duration_ms = model_ms;turn_usage 缺行的 turn_id 由 model_usage 补轮）+ tool_usage
//! （Σ duration_ms = tool_ms）+ session（parent_id 子会话 / directory 项目 / title 内容列）。
//! turn_mark 镜像 `apply_recalc` 的 COUNT（DISTINCT turn_id) GROUP BY （日, 模型),与 request_count 守恒。
//!
//! 子会话不计轮：`session.parent_id` 非空的会话,其轮不进
//! request_count / turn_mark,token 照常计入。session 表缺 parent_id 列（schema 漂移）时退回全计。
//!
//! 子会话项目：继承根会话的 `directory`（子代理可在子目录运行,自身目录不代表项目）。

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{Local, TimeZone};
use rusqlite::{OpenFlags, Connection};
use serde_json::json;

use super::store::{Batch, SessionRow, Store, Tokens, TurnPart, TurnRow};
use super::attention::{LivePhase, LiveTurn};
use super::turns::{normalize_project, UNKNOWN_PROJECT};
use super::{
    Adapter, AdapterError, AdapterMeta, CollectOutcome, CollectResult, ProbeOutcome, clamp0,
    millis_to_local_day_hour,
};

const REQUIRED_COLUMNS: &[&str] = &[
    "model_id",
    "started_at",
    "completed_at",
    "status",
    "input_tokens",
    "output_tokens",
    "provider_total_tokens",
    "computed_total_tokens",
];

const BATCH_SIZE: i64 = 500;
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

pub struct ZcodeAdapter {
    db_path: PathBuf,
    /// 上次 distinct turn 重算失败的日期（下次采集即使无新行也重试）。
    recalc_pending: Mutex<BTreeSet<String>>,
}

static META: AdapterMeta = AdapterMeta {
    id: "zcode",
    name: "ZCode",
    location: "~/.zcode/cli/db/db.sqlite",
    kind: "sqlite",
};

impl ZcodeAdapter {
    pub fn new() -> Self {
        let db_path = super::home_dir()
            .map(|h| h.join(".zcode").join("cli").join("db").join("db.sqlite"))
            .unwrap_or_else(|| PathBuf::from("db.sqlite"));
        ZcodeAdapter { db_path, recalc_pending: Mutex::new(BTreeSet::new()) }
    }

    /// 只读连接（WAL 库 + busy_timeout）。失败按错误码分类。
    fn open_ro(&self) -> Result<Connection, AdapterError> {
        if !self.db_path.exists() {
            return Err(AdapterError::new("no_source", format!("missing {}", self.db_path.display())));
        }
        Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| classify_open_err(e.to_string()))
            .and_then(|conn| {
                conn.busy_timeout(BUSY_TIMEOUT)
                    .map_err(|e| AdapterError::new("error", e.to_string()))?;
                Ok(conn)
            })
    }

    /// schema 校验：必需列齐全才算 ready,否则 unsupported_schema。
    fn check_schema(conn: &Connection) -> Result<String, AdapterError> {
        let mut stmt = conn
            .prepare("PRAGMA table_info(model_usage)")
            .map_err(|e| AdapterError::new("unsupported_schema", e.to_string()))?;
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| AdapterError::new("unsupported_schema", e.to_string()))?
            .flatten()
            .collect();
        let missing: Vec<&str> = REQUIRED_COLUMNS.iter().filter(|c| !cols.iter().any(|x| x == *c)).copied().collect();
        if !missing.is_empty() {
            return Err(AdapterError::new(
                "unsupported_schema",
                format!("model_usage missing columns: {}", missing.join(",")),
            ));
        }
        Ok(format!("model_usage:{}", cols.join(",")))
    }
}

fn classify_open_err(msg: String) -> AdapterError {
    if msg.contains("unable to open") || msg.contains("No such file") {
        AdapterError::new("no_source", msg)
    } else if msg.contains("locked") || msg.contains("busy") {
        AdapterError::new("busy", msg)
    } else {
        AdapterError::new("error", msg)
    }
}

impl Adapter for ZcodeAdapter {
    fn meta(&self) -> &'static AdapterMeta {
        &META
    }

    fn probe(&self) -> ProbeOutcome {
        match self.open_ro() {
            Err(e) => ProbeOutcome { status: e.code, fingerprint: None },
            Ok(conn) => match Self::check_schema(&conn) {
                Ok(fp) => ProbeOutcome { status: "ready".into(), fingerprint: Some(fp) },
                Err(e) => ProbeOutcome { status: e.code, fingerprint: None },
            },
        }
    }

    fn collect(&self, store: &mut Store) -> CollectResult {
        let conn = self.open_ro()?;
        let fingerprint = Self::check_schema(&conn)?; // 采集前再校验一次 schema（可能漂移）
        // cache 分项为可选列:缺失时按 0 采,不升格为 unsupported_schema。
        let optional_col = |c: &str| {
            let present = fingerprint.split_once(':').map_or(false, |(_, cols)| cols.split(',').any(|x| x == c));
            if present { format!("COALESCE({c}, 0)") } else { "0".to_string() }
        };
        let (cache_read_col, cache_write_col) =
            (optional_col("cache_read_input_tokens"), optional_col("cache_creation_input_tokens"));

        let scope = "db";
        let mut last_rowid: i64 = store
            .get_cursor(META.id, scope)
            .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
            .and_then(|v| v.get("rowid").and_then(|x| x.as_i64()))
            .unwrap_or(0);

        let mut batch = Batch::default();
        let mut months = std::collections::BTreeSet::new();
        let mut days: BTreeSet<String> = BTreeSet::new();
        let mut total_events = 0u64;
        let sql = format!(
            "SELECT rowid, model_id, started_at, completed_at, status,
                    input_tokens, output_tokens, provider_total_tokens, computed_total_tokens,
                    {cache_read_col}, {cache_write_col}
             FROM model_usage WHERE rowid > ?1 ORDER BY rowid ASC LIMIT {BATCH_SIZE}"
        );

        loop {
            let mut stmt = conn.prepare(&sql).map_err(|e| AdapterError::new("error", e.to_string()))?;
            let rows = stmt
                .query_map([last_rowid], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, Option<i64>>(6)?,
                        r.get::<_, Option<i64>>(7)?,
                        r.get::<_, Option<i64>>(8)?,
                        r.get::<_, Option<i64>>(9)?,
                        r.get::<_, Option<i64>>(10)?,
                    ))
                })
                .map_err(|e| AdapterError::new("error", e.to_string()))?;

            let mut fetched = 0usize;
            let mut last_in_batch = last_rowid;
            for row in rows {
                let (rowid, model, started_at, completed_at, status, input, output, provider_total, computed_total, cache_read, cache_write) =
                    row.map_err(|e| AdapterError::new("error", e.to_string()))?;
                fetched += 1;
                last_in_batch = last_in_batch.max(rowid);

                // 行级过滤（与旧项目一致）：非 completed 不计,但游标照常推进
                if status.as_deref() != Some("completed") {
                    continue;
                }
                let millis = started_at.or(completed_at);
                let Some(millis) = millis else { continue };
                let Some((day, hour)) = millis_to_local_day_hour(millis) else { continue };
                let model = model.filter(|m| !m.is_empty()).unwrap_or_else(|| "unknown".into());

                // total 口径：provider 优先,0/NULL 兜底 computed（防真实用量误报 0）
                let tokens = row_tokens(input, output, provider_total, computed_total, cache_read, cache_write);
                // request_count 不写行级粗值（= 模型调用行数）,由下方 distinct turn 重算落权威值
                batch.add_usage(&day, Some(hour), META.id, &model, tokens, 0);
                months.insert(day[..7].to_string());
                days.insert(day);
            }
            drop(stmt);

            let advanced = last_in_batch > last_rowid;
            last_rowid = last_in_batch;
            total_events += batch.events;
            batch.cursors.push((scope.to_string(), json!({ "rowid": last_rowid }).to_string()));
            store.commit(META.id, &batch).map_err(|e| AdapterError::new("error", e))?;
            batch = Batch::default();

            if fetched < BATCH_SIZE as usize || !advanced {
                break;
            }
        }

        // request_count 权威化：涉及日期即时按 distinct turn 重算。
        self.recalc_turn_counts(&conn, store, days);

        // 任务层:失败只记日志,不影响已提交的 token 聚合（单源内再隔离）。
        if let Err(e) = self.sync_tasks(&conn, store) {
            crate::dev_log!("[collector] zcode task sync failed: {} {}", e.code, e.message);
        }

        Ok(CollectOutcome { events: total_events, months })
    }
}

impl ZcodeAdapter {
    /// 对本次涉及的日期（∪ 上次失败挂起的日期）一次范围查询,按 （本地日, model) 重算 distinct turn
    /// 覆盖聚合表计数。范围 = [最早日 0 点, 最晚日次日 0 点);区间内无数据的日 GROUP BY 自然为空,不受影响。
    fn recalc_turn_counts(&self, conn: &Connection, store: &mut Store, mut days: BTreeSet<String>) {
        days.extend(std::mem::take(&mut *self.recalc_pending.lock().unwrap()));
        let (Some(first), Some(last)) = (days.iter().next().cloned(), days.iter().next_back().cloned()) else {
            return;
        };
        let midnight = |d: chrono::NaiveDate| -> Option<i64> {
            Local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest().map(|dt| dt.timestamp_millis())
        };
        let parse = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok();
        let range = parse(&first)
            .and_then(midnight)
            .zip(parse(&last).and_then(|d| d.succ_opt()).and_then(midnight));
        let ok = match range {
            Some((start_ms, end_ms)) => Self::apply_recalc(conn, store, start_ms, end_ms),
            None => false,
        };
        if !ok {
            self.recalc_pending.lock().unwrap().extend(days);
        }
    }

    /// 范围内按 （本地日, model) 重算 distinct turn 并覆盖聚合表。子会话（parent_id 非空）的轮不计。
    /// 返回 false = 查询失败（调用方挂起日期重试）。
    fn apply_recalc(conn: &Connection, store: &mut Store, start_ms: i64, end_ms: i64) -> bool {
        let child_filter = if Self::can_exclude_children(conn) {
            "AND NOT EXISTS (SELECT 1 FROM session s WHERE s.id = model_usage.session_id
                                AND s.parent_id IS NOT NULL AND s.parent_id <> '')"
        } else {
            ""
        };
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT date(started_at/1000, 'unixepoch', 'localtime') AS d, model_id,
                    COUNT(DISTINCT turn_id) AS turns
             FROM model_usage
             WHERE status = 'completed' AND started_at IS NOT NULL
               AND started_at >= ?1 AND started_at < ?2 {child_filter}
             GROUP BY d, model_id"
        )) else {
            return false;
        };
        let Ok(rows) = stmt.query_map(rusqlite::params![start_ms, end_ms], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?))
        }) else {
            return false;
        };
        // 按 day 聚合后覆盖
        let mut per_day: HashMap<String, Vec<(String, i64)>> = HashMap::new();
        for row in rows.flatten() {
            let (d, model, turns) = row;
            per_day
                .entry(d)
                .or_default()
                .push((model.unwrap_or_else(|| "unknown".into()), turns));
        }
        for (d, counts) in per_day {
            store.apply_turn_counts(META.id, &d, &counts);
        }
        true
    }
}

impl ZcodeAdapter {
    /// 子会话排除所需列齐全:model_usage.session_id + session.id / parent_id。
    fn can_exclude_children(conn: &Connection) -> bool {
        let has = |table: &str, names: &[&str]| {
            let cols = table_columns(conn, table);
            names.iter().all(|n| cols.iter().any(|c| c == n))
        };
        has("model_usage", &["session_id"]) && has("session", &["id", "parent_id"])
    }
}

/// model_usage 一行的 token 口径（采集行级路径与任务同步共用,保证 daily_usage 与 turn_part 同源）：
/// total = provider 优先,0/NULL 兜底 computed;cache 两列为可选列（缺失传 None）。
fn row_tokens(input: Option<i64>, output: Option<i64>, provider_total: Option<i64>, computed_total: Option<i64>, cache_read: Option<i64>, cache_write: Option<i64>) -> Tokens {
    let provider = provider_total.unwrap_or(0);
    Tokens {
        input: clamp0(input.unwrap_or(0)),
        output: clamp0(output.unwrap_or(0)),
        total: if provider > 0 { provider } else { computed_total.unwrap_or(0).max(0) },
        cache_read: clamp0(cache_read.unwrap_or(0)),
        cache_write: clamp0(cache_write.unwrap_or(0)),
    }
}

/// 路径类 TEXT 列按字节读:UTF-8 优先,非法段按系统 ANSI 代码页回退（S4-R 缺陷 A;
/// rusqlite 的 String 读取遇非法 UTF-8 直接报错,会让整行元数据丢失 → 项目落 unknown）。
fn text_col(r: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Option<String>> {
    use rusqlite::types::ValueRef;
    Ok(match r.get_ref(idx)? {
        ValueRef::Text(b) | ValueRef::Blob(b) => Some(super::text::decode_bytes(b).into_owned()),
        _ => None,
    })
}

/// 子会话的项目目录 = 根会话目录（沿 parent_id 上溯取最上层非空 directory;深度上限防环）。
/// 子代理常在子目录运行,按自身目录
/// 会拆出一个没有根会话轮次的伪项目,Scratch 规则按 0 会话 0 轮把它整个折进 Scratch。
fn root_directory(conn: &Connection, parent: &str) -> Option<String> {
    let mut cur = parent.to_string();
    let mut dir = None;
    for _ in 0..16 {
        let Ok((next, d)) = conn.query_row("SELECT parent_id, directory FROM session WHERE id = ?1", [&cur], |r| {
            Ok((r.get::<_, Option<String>>(0)?, text_col(r, 1)?))
        }) else {
            break;
        };
        if d.as_deref().map_or(false, |d| !d.trim().is_empty()) {
            dir = d;
        }
        match next.filter(|p| !p.is_empty() && *p != cur) {
            Some(p) => cur = p,
            None => break,
        }
    }
    dir
}

/// 表的列名集合（表不存在 = 空）。
fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else { return Vec::new() };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) else { return Vec::new() };
    rows.flatten().collect()
}

/// 任务同步游标（scope = "tasks"）：model_usage rowid 水位 + turn_usage 完成时间水位。
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct TaskCursor {
    #[serde(default)]
    rowid: i64,
    #[serde(default)]
    turn_ts: i64,
}

/// 一轮的同步构建态。
#[derive(Default)]
struct TurnBuild {
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    ttft_ms: Option<i64>,
    retry_count: i64,
    tool_call_count: Option<i64>,
    error: bool,
    /// 用户中止（cancelled_by_user = 1 / error_type = turn_cancelled;S4-R 与 error 分列）。
    aborted: bool,
    model_ms: i64,
    model_calls: i64,
    min_model_at: Option<i64>,
    max_model_end: Option<i64>,
    /// （millis, day, model) of 首条完成的响应
    first: Option<(i64, String, String)>,
    parts: Vec<TurnPart>,
    tool_count: i64,
    tool_ms: Option<i64>,
}

impl ZcodeAdapter {
    /// 任务同步（按会话重算覆盖）：受影响会话 = model_usage 新 rowid 涉及的会话 ∪
    /// turn_usage 完成时间越过水位的会话;每个会话从 turn_usage / model_usage / tool_usage /
    /// session 整体重建 turn_raw / turn_part / session（精确值,不走累加器）,与水位同事务提交。
    /// 表缺失（schema 漂移）→ 跳过任务层,token 链路不受影响。
    fn sync_tasks(&self, conn: &Connection, store: &mut Store) -> Result<(), AdapterError> {
        let turn_cols = table_columns(conn, "turn_usage");
        let session_cols = table_columns(conn, "session");
        let model_cols = table_columns(conn, "model_usage");
        let need = |cols: &[String], names: &[&str]| names.iter().all(|n| cols.iter().any(|c| c == n));
        if !need(&turn_cols, &["session_id", "turn_id", "started_at", "completed_at", "duration_ms"])
            || !need(&session_cols, &["id", "parent_id", "directory"])
            || !need(&model_cols, &["session_id", "turn_id", "duration_ms"])
        {
            crate::dev_log!("[collector] zcode task tables incomplete, skip task sync");
            return Ok(());
        }
        let scope = "tasks";
        let mut cur: TaskCursor = store
            .get_cursor(META.id, scope)
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        let e = |x: rusqlite::Error| AdapterError::new("error", x.to_string());

        let mut affected: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let (max_rowid, max_ts): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT COALESCE(MAX(rowid), 0) FROM model_usage),
                        (SELECT COALESCE(MAX(COALESCE(completed_at, started_at)), 0) FROM turn_usage)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(e)?;
        if max_rowid <= cur.rowid && max_ts <= cur.turn_ts {
            return Ok(());
        }
        {
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT session_id FROM model_usage WHERE rowid > ?1 AND session_id IS NOT NULL
                     UNION SELECT DISTINCT session_id FROM turn_usage
                     WHERE COALESCE(completed_at, started_at) > ?2 AND session_id IS NOT NULL",
                )
                .map_err(e)?;
            let rows = stmt.query_map(rusqlite::params![cur.rowid, cur.turn_ts], |r| r.get::<_, String>(0)).map_err(e)?;
            affected.extend(rows.flatten());
        }

        let fp = model_cols.join(",");
        let optional = |c: &str| if fp.split(',').any(|x| x == c) { format!("COALESCE({c}, 0)") } else { "0".to_string() };
        let (cr, cw) = (optional("cache_read_input_tokens"), optional("cache_creation_input_tokens"));
        let tool_ok = need(&table_columns(conn, "tool_usage"), &["session_id", "turn_id", "duration_ms"]);
        let has_title = session_cols.iter().any(|c| c == "title");
        let turn_opt = |c: &str| if turn_cols.iter().any(|x| x == c) { c.to_string() } else { "NULL".to_string() };

        let mut batch = Batch::default();
        for sid in &affected {
            self.build_session(conn, &mut batch, sid, &cr, &cw, tool_ok, has_title, &turn_opt).map_err(e)?;
        }
        cur.rowid = max_rowid;
        cur.turn_ts = max_ts;
        batch.cursors.push((scope.to_string(), serde_json::to_string(&cur).unwrap_or_default()));
        store.commit(META.id, &batch).map_err(|x| AdapterError::new("error", x))?;
        crate::dev_log!("[collector] zcode task sync: {} session(s)", affected.len());
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn build_session(
        &self,
        conn: &Connection,
        batch: &mut Batch,
        sid: &str,
        cr: &str,
        cw: &str,
        tool_ok: bool,
        has_title: bool,
        turn_opt: &dyn Fn(&str) -> String,
    ) -> rusqlite::Result<()> {
        use std::collections::BTreeMap;
        let meta: Option<(Option<String>, Option<String>, Option<String>)> = conn
            .query_row(
                &format!("SELECT parent_id, directory, {} FROM session WHERE id = ?1", if has_title { "title" } else { "NULL" }),
                [sid],
                |r| Ok((r.get(0)?, text_col(r, 1)?, r.get(2)?)),
            )
            .ok();
        let (parent, directory, title) = meta.unwrap_or((None, None, None));
        let parent = parent.filter(|p| !p.is_empty());
        let directory = parent.as_deref().and_then(|p| root_directory(conn, p)).or(directory);
        let project = directory.as_deref().map(normalize_project).unwrap_or_else(|| UNKNOWN_PROJECT.to_string());

        let mut turns: BTreeMap<String, TurnBuild> = BTreeMap::new();
        {
            let sql = format!(
                "SELECT turn_id, started_at, completed_at, duration_ms, {}, {}, {}, {}, {} FROM turn_usage WHERE session_id = ?1",
                turn_opt("time_to_first_token_ms"),
                turn_opt("model_retry_count"),
                turn_opt("tool_call_count"),
                turn_opt("error_type"),
                turn_opt("cancelled_by_user")
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([sid], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                ))
            })?;
            for (turn_id, started, completed, duration, ttft, retry, tool_calls, error_type, cancelled) in rows.flatten() {
                let b = turns.entry(turn_id.unwrap_or_default()).or_default();
                b.started_at = started;
                b.completed_at = completed;
                b.duration_ms = duration;
                b.ttft_ms = ttft;
                b.retry_count = retry.unwrap_or(0);
                b.tool_call_count = tool_calls;
                // 本机:status=cancelled ⇔ cancelled_by_user=1 ⇔ error_type=turn_cancelled（75 轮）;
                // 旧库缺 cancelled_by_user 列时靠 error_type 判别。
                b.aborted = cancelled.unwrap_or(0) != 0 || error_type.as_deref() == Some("turn_cancelled");
                b.error = error_type.is_some() && !b.aborted;
            }
        }
        {
            let sql = format!(
                "SELECT turn_id, status, started_at, completed_at, duration_ms, model_id,
                        input_tokens, output_tokens, provider_total_tokens, computed_total_tokens, {cr}, {cw}
                 FROM model_usage WHERE session_id = ?1 ORDER BY COALESCE(started_at, completed_at), rowid"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([sid], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                    r.get::<_, Option<i64>>(10)?,
                    r.get::<_, Option<i64>>(11)?,
                ))
            })?;
            for (turn_id, status, started, completed, duration, model_id, i, o, pt, ct, c_r, c_w) in rows.flatten() {
                let has_turn_id = turn_id.is_some();
                let b = turns.entry(turn_id.unwrap_or_default()).or_default();
                b.model_ms += duration.unwrap_or(0).max(0);
                if let Some(at) = started.or(completed) {
                    b.min_model_at = Some(b.min_model_at.map_or(at, |m| m.min(at)));
                    let end = completed.unwrap_or(at + duration.unwrap_or(0).max(0));
                    b.max_model_end = Some(b.max_model_end.map_or(end, |m| m.max(end)));
                }
                // 与行级采集同一过滤:completed + 有时间;day / model / tokens 同源
                if status.as_deref() != Some("completed") {
                    continue;
                }
                let Some(millis) = started.or(completed) else { continue };
                let Some((day, _)) = millis_to_local_day_hour(millis) else { continue };
                let model_empty = model_id.as_deref() == Some("");
                let model = model_id.filter(|m| !m.is_empty()).unwrap_or_else(|| "unknown".into());
                let t = row_tokens(i, o, pt, ct, c_r, c_w);
                if t.total == 0 && t.input == 0 && t.output == 0 && t.cache_read == 0 && t.cache_write == 0 {
                    continue;
                }
                b.model_calls += 1;
                if b.first.as_ref().map_or(true, |(m, _, _)| millis < *m) {
                    b.first = Some((millis, day.clone(), model.clone()));
                }
                // turn_mark 镜像 apply_recalc:COUNT（DISTINCT turn_id) GROUP BY date（started_at), model_id,子会话不计
                let mark = (started.is_some() && has_turn_id && !model_empty && parent.is_none()) as i64;
                match b.parts.iter_mut().find(|p| p.day == day && p.model == model) {
                    Some(p) => {
                        p.input += t.input;
                        p.output += t.output;
                        p.total += t.total;
                        p.cache_read += t.cache_read;
                        p.cache_write += t.cache_write;
                        p.model_calls += 1;
                        p.turn_mark = p.turn_mark.max(mark);
                    }
                    None => b.parts.push(TurnPart {
                        day,
                        model,
                        input: t.input,
                        output: t.output,
                        total: t.total,
                        cache_read: t.cache_read,
                        cache_write: t.cache_write,
                        model_calls: 1,
                        turn_mark: mark,
                    }),
                }
            }
        }
        if tool_ok {
            let mut stmt = conn.prepare(
                "SELECT turn_id, COUNT(*), COALESCE(SUM(duration_ms), 0) FROM tool_usage WHERE session_id = ?1 GROUP BY turn_id",
            )?;
            let rows = stmt.query_map([sid], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?;
            for (turn_id, n, ms) in rows.flatten() {
                if let Some(b) = turns.get_mut(&turn_id.unwrap_or_default()) {
                    b.tool_count = n;
                    b.tool_ms = Some(ms.max(0));
                }
            }
        }

        // 按开始时间排序编号;gap = 开始 − 上一轮结束（同会话,原始值）
        let mut ordered: Vec<(i64, String, TurnBuild)> = turns
            .into_iter()
            .filter_map(|(k, b)| b.started_at.or(b.min_model_at).map(|s| (s, k, b)))
            .collect();
        ordered.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        batch.replace_session(META.id, sid);
        let mut prev_end: Option<i64> = None;
        let mut span: (Option<i64>, Option<i64>) = (None, None);
        // 末轮现状——turn_usage 已写 completed_at = 答完在等用户（用户中止除外）;
        // 缺 turn_usage 行 / 未完成 = 模型在处理（工具明细不分,tool_pending 不适用）。
        let mut live_phase = LivePhase::Idle;
        let mut live_last = 0;
        let mut live_input = None;
        for (seq, (start, _key, b)) in ordered.into_iter().enumerate() {
            let end = b
                .completed_at
                .or(b.duration_ms.map(|d| start + d))
                .or(b.max_model_end)
                .unwrap_or(start)
                .max(start);
            let start_day = millis_to_local_day_hour(start).map(|(d, _)| d).unwrap_or_default();
            let (day, model) = match &b.first {
                Some((_, d, m)) => (d.clone(), m.clone()),
                None => (start_day, "unknown".to_string()),
            };
            batch.add_turn(
                META.id,
                TurnRow {
                    session_id: sid.to_string(),
                    turn_seq: seq as i64 + 1,
                    day,
                    project_key: project.clone(),
                    model_key: model,
                    started_at: start,
                    ended_at: end,
                    wall_ms: Some(b.duration_ms.unwrap_or(end - start).max(0)),
                    model_ms: Some(b.model_ms),
                    tool_ms: b.tool_ms.or(tool_ok.then_some(0)),
                    ttft_ms: b.ttft_ms,
                    gap_ms: prev_end.map(|e| (start - e).max(0)),
                    model_calls: b.model_calls,
                    tool_calls: b.tool_call_count.unwrap_or(b.tool_count),
                    error_count: b.error as i64,
                    retry_count: b.retry_count,
                    aborted: b.aborted,
                    parts: b.parts,
                },
            );
            live_phase = if b.aborted {
                LivePhase::Idle
            } else if b.completed_at.is_some() {
                LivePhase::Done { exact: true }
            } else {
                LivePhase::Busy
            };
            live_last = end;
            live_input = Some(start);
            prev_end = Some(end);
            span.0 = Some(span.0.map_or(start, |x| x.min(start)));
            span.1 = Some(span.1.map_or(end, |x| x.max(end)));
        }
        let title = title.filter(|t| !t.trim().is_empty());
        batch.live.insert(
            (META.id.to_string(), sid.to_string()),
            LiveTurn {
                project_key: project.clone(),
                parent_id: parent.clone(),
                title: title.clone(),
                host: None,
                phase: live_phase,
                last_event: live_last,
                last_input: live_input,
                // ZCode 源是 SQLite 库,无单会话文件可探,吃采集节拍
                watch: None,
            },
        );
        batch.upsert_session(
            META.id,
            SessionRow {
                session_id: sid.to_string(),
                project_key: Some(project),
                project_authoritative: false,
                parent_id: parent,
                title,
                started_at: span.0,
                ended_at: span.1,
            },
        );
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use crate::collector::store::days_in_month;
    use crate::collector::millis_to_local_day;

    fn setup_table(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE model_usage (
                rowid_alias INTEGER PRIMARY KEY,
                id TEXT, model_id TEXT, turn_id TEXT, started_at INTEGER, completed_at INTEGER,
                status TEXT, input_tokens INTEGER, output_tokens INTEGER,
                provider_total_tokens INTEGER, computed_total_tokens INTEGER
             );",
        )
        .unwrap();
    }

    fn insert(conn: &Connection, model: &str, started: i64, status: &str, i: i64, o: i64, pt: Option<i64>, ct: i64) {
        // turn_id 派生自 model+started:同参数两次调用 = 同 turn(供重算去重)
        let turn = format!("{}_{}", model, started / 10_000);
        conn.execute(
            "INSERT INTO model_usage (model_id, turn_id, started_at, completed_at, status, input_tokens, output_tokens, provider_total_tokens, computed_total_tokens)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![model, turn, started, status, i, o, pt, ct],
        )
        .unwrap();
    }

    /// collect() 依赖 store 游标与真实 home 路径——这里直接测 SQL 语义等价片段。
    #[test]
    fn sql_semantics_via_temp_db() {
        // 内存库模拟 model_usage,验证行级过滤与 total 兜底口径
        let conn = Connection::open_in_memory().unwrap();
        setup_table(&conn);
        // 2026-09-05 12:00 本地 → 用固定毫秒;本地日断言经由 millis_to_local_day
        let millis = 1_757_000_000_000i64; // 2025-09-05T12:26:40Z 附近,仅验证非空日产出
        insert(&conn, "glm-5.3", millis, "completed", 10, 5, Some(20), 22);
        insert(&conn, "glm-5.3", millis, "error", 99, 99, Some(99), 99); // 非 completed 跳过
        insert(&conn, "gpt-5", millis, "completed", 1, 1, Some(0), 7); // provider=0 → computed 兜底
        insert(&conn, "", millis, "completed", 1, 1, Some(3), 3); // 空 model → unknown

        let mut stmt = conn
            .prepare("SELECT rowid, model_id, status, input_tokens, output_tokens, provider_total_tokens, computed_total_tokens FROM model_usage WHERE rowid > 0 ORDER BY rowid")
            .unwrap();
        let rows: Vec<(i64, String, String, i64, i64, i64, i64)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                ))
            })
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(rows.len(), 4);

        let mut batch = Batch::default();
        for (_rowid, model, status, i, o, pt, ct) in rows {
            if status != "completed" {
                continue;
            }
            let day = millis_to_local_day(millis).unwrap();
            let total = if pt > 0 { pt } else { ct.max(0) };
            let model = if model.is_empty() { "unknown".to_string() } else { model };
            batch.add(&day, "zcode", &model, clamp0(i), clamp0(o), total, 1);
        }
        assert_eq!(batch.events, 3);
        let total: i64 = batch.entries.values().map(|e| e[2]).sum();
        assert_eq!(total, 20 + 7 + 3);
        assert!(batch.entries.keys().all(|k| k.0.len() == 10)); // YYYY-MM-DD
        let _ = days_in_month(2026, 9);
    }

    /// PHASE12 S2:真实列结构的临时 ZCode 库走完整 collect——轮 / 会话精确同步、子会话并入、守恒。
    #[test]
    fn s2_task_sync_from_zcode_tables() {
        let dir = std::env::temp_dir().join(format!("tc_zcode_s2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("db.sqlite");
        let _ = std::fs::remove_file(&db);
        const T: i64 = 1_788_602_400_000;
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, slug TEXT, directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER);
                 CREATE TABLE model_usage (id TEXT, session_id TEXT, turn_id TEXT, model_id TEXT, status TEXT, started_at INTEGER, completed_at INTEGER,
                     duration_ms INTEGER, time_to_first_token_ms INTEGER, input_tokens INTEGER, output_tokens INTEGER,
                     cache_creation_input_tokens INTEGER, cache_read_input_tokens INTEGER, provider_total_tokens INTEGER, computed_total_tokens INTEGER, retry_count INTEGER);
                 CREATE TABLE turn_usage (session_id TEXT, turn_id TEXT, status TEXT, started_at INTEGER, completed_at INTEGER, duration_ms INTEGER,
                     time_to_first_token_ms INTEGER, model_request_count INTEGER, model_retry_count INTEGER, tool_call_count INTEGER, tool_error_count INTEGER, error_type TEXT);
                 CREATE TABLE tool_usage (id TEXT, session_id TEXT, turn_id TEXT, tool_name TEXT, status TEXT, started_at INTEGER, completed_at INTEGER, duration_ms INTEGER);",
            )
            .unwrap();
            c.execute(r"INSERT INTO session (id, parent_id, directory, title) VALUES ('ses_p', NULL, 'E:\Work\Demo', '<title>'), ('ses_c', 'ses_p', 'E:\Work\Demo\tools\indexer', '<sub title>')", []).unwrap();
            let tu = |sid: &str, tid: &str, st: &str, s: i64, d: i64, ttft: Option<i64>, retry: i64, tools: Option<i64>, err: Option<&str>| {
                c.execute(
                    "INSERT INTO turn_usage VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, 0, ?10)",
                    rusqlite::params![sid, tid, st, T + s, T + s + d, d, ttft, retry, tools, err],
                )
                .unwrap();
            };
            tu("ses_p", "t1", "completed", 0, 20_000, Some(1_500), 1, Some(2), None);
            tu("ses_p", "t2", "cancelled", 80_000, 5_000, None, 0, Some(0), Some("turn_cancelled"));
            tu("ses_c", "c1", "completed", 5_000, 7_000, Some(900), 0, None, None);
            let mu = |sid: &str, tid: &str, model: &str, status: &str, s: i64, d: i64, i: i64, o: i64| {
                c.execute(
                    "INSERT INTO model_usage (session_id, turn_id, model_id, status, started_at, completed_at, duration_ms, input_tokens, output_tokens,
                         cache_creation_input_tokens, cache_read_input_tokens, provider_total_tokens, computed_total_tokens)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, 7, ?10, ?10)",
                    rusqlite::params![sid, tid, model, status, T + s, T + s + d, d, i, o, i + o],
                )
                .unwrap();
            };
            mu("ses_p", "t1", "glm-5.3", "completed", 100, 3_000, 100, 10);
            mu("ses_p", "t1", "glm-5.3", "completed", 9_000, 2_000, 200, 20);
            mu("ses_p", "t1", "glm-5.3", "error", 15_000, 500, 9, 9);
            mu("ses_c", "c1", "glm-5.3-flash", "completed", 6_000, 4_000, 50, 5);
            mu("ses_p", "t2", "glm-5.3", "cancelled", 81_000, 1_000, 0, 0);
            c.execute("INSERT INTO tool_usage (session_id, turn_id, status, duration_ms) VALUES ('ses_p','t1','completed',1000), ('ses_p','t1','error',2500)", []).unwrap();
        }
        let adapter = ZcodeAdapter { db_path: db.clone(), recalc_pending: Mutex::new(BTreeSet::new()) };
        let mut store = crate::collector::store::Store::open_in_memory().unwrap();
        let ok = adapter.collect(&mut store).is_ok();
        // 二次采集:水位未动 → 任务层不重建,结果不变
        let ok2 = adapter.collect(&mut store).is_ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok && ok2);
        assert!(store.test_project_conservation().is_empty(), "行级不写粗值 + 即时重算 → request_count 与 turns 守恒");

        let turns = store.test_turns(META.id);
        assert_eq!(turns.len(), 2, "子会话不单独出轮");
        let t1 = &turns[0];
        assert_eq!((t1.model_calls, t1.subagent_count, t1.subagent_calls, t1.tool_calls), (3, 1, 1, 2));
        assert_eq!((t1.wall_ms, t1.ttft_ms, t1.retry_count, t1.error_count), (Some(20_000), Some(1_500), 1, 0), "ZCode 精确值");
        assert_eq!(t1.model_ms, Some(3_000 + 2_000 + 500 + 4_000), "Σ model_usage.duration_ms（含 error 行）+ 子会话");
        assert_eq!(t1.tool_ms, Some(3_500));
        assert_eq!(t1.total_tokens, 110 + 220 + 55);
        assert_eq!((t1.project_key.as_str(), t1.model_key.as_str(), t1.gap_ms), ("e:/Work/Demo", "glm-5.3", None));
        let t2 = &turns[1];
        assert_eq!((t2.model_calls, t2.error_count, t2.aborted, t2.wall_ms, t2.gap_ms), (0, 0, true, Some(5_000), Some(60_000)), "取消轮 = 中止,不计错");
        assert!(!t1.aborted);
        let sessions = store.test_sessions(META.id);
        let p = sessions.iter().find(|s| s.session_id == "ses_p").unwrap();
        assert_eq!((p.title.as_deref(), p.subagent_count, p.subagent_calls), (Some("<title>"), 1, 1));
        let c = sessions.iter().find(|s| s.session_id == "ses_c").unwrap();
        assert_eq!(c.parent_id.as_deref(), Some("ses_p"));
        // v13:子代理在子目录运行 → 继承根会话目录（否则子目录伪项目 0 会话 0 轮,被 Scratch 整个吞掉）
        assert_eq!(c.project_key, "e:/Work/Demo");
        let keys: Vec<String> = {
            let mut stmt = store.conn().prepare("SELECT DISTINCT project_key FROM daily_project").unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().flatten().collect()
        };
        assert_eq!(keys, vec!["e:/Work/Demo".to_string()], "子会话 token 不落子目录键");
        assert_eq!(store.test_task_sessions(META.id), vec!["ses_p".to_string()]);
        assert!(store.test_project_conservation().is_empty(), "{:?}", store.test_project_conservation());
        // S3 定案:子会话轮不计 request_count,token 照常计入
        let rows = store.month_rows("2026-09", "agent", "total", chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()).unwrap();
        assert_eq!(rows[0].message_counts.iter().sum::<i64>(), 1, "只计父会话 t1;子会话 c1 与取消轮 t2 不计");
        assert_eq!(rows[0].month_total, 110 + 220 + 55);
    }

    /// S4-R 待观察 C:活跃会话的一轮跨多次采集持续追加 model_usage 行——旧实现每行 +1 且 10 分钟节流
    /// 重算,当天一格会虚高（本机实证 1 轮记成 3〜8）。现应每次采集后都等于 distinct turn,并与
    /// daily_project.turns 逐格守恒;model_error 轮计错、不算中止。
    #[test]
    fn active_turn_across_collects_keeps_request_count_exact() {
        let dir = std::env::temp_dir().join(format!("tc_zcode_active_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("db.sqlite");
        let _ = std::fs::remove_file(&db);
        const T: i64 = 1_788_602_400_000;
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, slug TEXT, directory TEXT, title TEXT);
             CREATE TABLE model_usage (id TEXT, session_id TEXT, turn_id TEXT, model_id TEXT, status TEXT, started_at INTEGER, completed_at INTEGER,
                 duration_ms INTEGER, input_tokens INTEGER, output_tokens INTEGER, provider_total_tokens INTEGER, computed_total_tokens INTEGER);
             CREATE TABLE turn_usage (session_id TEXT, turn_id TEXT, status TEXT, started_at INTEGER, completed_at INTEGER, duration_ms INTEGER,
                 time_to_first_token_ms INTEGER, model_retry_count INTEGER, tool_call_count INTEGER, error_type TEXT, cancelled_by_user INTEGER);
             INSERT INTO session (id, parent_id, directory) VALUES ('s', NULL, 'D:\\OneDrive\\文档\\knowledge');",
        )
        .unwrap();
        // 活跃轮:turn_usage 已开行但未完成（completed_at NULL）
        c.execute("INSERT INTO turn_usage VALUES ('s', 't1', 'running', ?1, NULL, NULL, NULL, 0, 0, NULL, 0)", [T]).unwrap();
        let add_call = |i: i64| {
            c.execute(
                "INSERT INTO model_usage (session_id, turn_id, model_id, status, started_at, completed_at, duration_ms, input_tokens, output_tokens, provider_total_tokens, computed_total_tokens)
                 VALUES ('s', 't1', 'glm-5.3-flash', 'completed', ?1, ?2, 1000, 10, 1, 11, 11)",
                rusqlite::params![T + i * 2_000, T + i * 2_000 + 1_000],
            )
            .unwrap();
        };
        let adapter = ZcodeAdapter { db_path: db.clone(), recalc_pending: Mutex::new(BTreeSet::new()) };
        let mut store = crate::collector::store::Store::open_in_memory().unwrap();
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap();
        let counts = |store: &crate::collector::store::Store| -> i64 {
            store.month_rows("2026-09", "agent", "total", today).unwrap()[0].message_counts.iter().sum()
        };
        for round in 0..4 {
            for i in 0..3 {
                add_call(round * 3 + i);
            }
            assert!(adapter.collect(&mut store).is_ok());
            assert_eq!(counts(&store), 1, "第 {round} 次采集:同一活跃轮只计 1");
            assert!(store.test_project_conservation().is_empty(), "第 {round} 次采集:{:?}", store.test_project_conservation());
        }
        // 轮完成 + 第二轮报 model_error
        c.execute("UPDATE turn_usage SET status = 'completed', completed_at = ?1, duration_ms = 30000 WHERE turn_id = 't1'", [T + 30_000]).unwrap();
        c.execute("INSERT INTO turn_usage VALUES ('s', 't2', 'error', ?1, ?2, 3000, NULL, 0, 0, 'model_error', 0)", rusqlite::params![T + 60_000, T + 63_000]).unwrap();
        c.execute(
            "INSERT INTO model_usage (session_id, turn_id, model_id, status, started_at, completed_at, duration_ms, input_tokens, output_tokens, provider_total_tokens, computed_total_tokens)
             VALUES ('s', 't2', 'glm-5.3-flash', 'completed', ?1, ?2, 1000, 5, 1, 6, 6)",
            rusqlite::params![T + 60_500, T + 61_500],
        )
        .unwrap();
        assert!(adapter.collect(&mut store).is_ok());
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(counts(&store), 2);
        assert!(store.test_project_conservation().is_empty(), "{:?}", store.test_project_conservation());
        let turns = store.test_turns(META.id);
        assert_eq!(turns.len(), 2);
        assert_eq!((turns[0].model_calls, turns[0].error_count, turns[0].aborted), (12, 0, false));
        assert_eq!((turns[1].error_count, turns[1].aborted), (1, false), "model_error 计错,不算中止");
        assert_eq!(turns[0].project_key, "d:/OneDrive/文档/knowledge", "中文目录原样归一");
    }

    /// S3 定案:apply_recalc 排除 parent_id 非空的会话;session 表缺列时退回全计。
    #[test]
    fn turn_recalc_excludes_child_sessions() {
        let today = chrono::Local::now().date_naive();
        let day_start = chrono::Local.from_local_datetime(&today.and_hms_opt(0, 0, 0).unwrap()).single().unwrap().timestamp_millis();
        let ymd = today.format("%Y-%m-%d").to_string();
        let month = today.format("%Y-%m").to_string();
        let run = |with_session: bool| -> i64 {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(
                "CREATE TABLE model_usage (session_id TEXT, turn_id TEXT, model_id TEXT, status TEXT, started_at INTEGER);",
            )
            .unwrap();
            if with_session {
                conn.execute_batch(
                    "CREATE TABLE session (id TEXT, parent_id TEXT);
                     INSERT INTO session VALUES ('root', NULL), ('root2', ''), ('child', 'root');",
                )
                .unwrap();
            }
            for (sid, tid, offset) in [("root", "t1", 1_000), ("root", "t1", 2_000), ("root2", "t2", 3_000), ("child", "c1", 4_000), ("child", "c2", 5_000)] {
                conn.execute(
                    "INSERT INTO model_usage VALUES (?1, ?2, 'A', 'completed', ?3)",
                    rusqlite::params![sid, tid, day_start + offset],
                )
                .unwrap();
            }
            let mut store = crate::collector::store::Store::open_in_memory().unwrap();
            let mut b = Batch::default();
            b.add(&ymd, "zcode", "A", 1, 1, 2, 5);
            store.commit("zcode", &b).unwrap();
            ZcodeAdapter::apply_recalc(&conn, &mut store, day_start, day_start + 86_400_000);
            store.month_rows(&month, "agent", "total", today).unwrap()[0].message_counts[today.day() as usize - 1]
        };
        assert_eq!(run(true), 2, "root t1 + root2 t2;child 两轮不计（空串 parent 视为根）");
        assert_eq!(run(false), 4, "缺 session 表 → 退回全计");
    }

    #[test]
    fn turn_recalc_counts_distinct_turns_per_day_and_model() {
        // 本地日边界:取今天 0 点(本地),三行同日、两行昨日
        let today = chrono::Local::now().date_naive();
        let day_start = chrono::Local
            .from_local_datetime(&today.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis();
        let yesterday_start = day_start - 86_400_000;

        let conn = Connection::open_in_memory().unwrap();
        setup_table(&conn);
        // 今日 turn T1(model A,2 次调用)+ T2(model B,1 次)→ A:1, B:1
        insert(&conn, "A", day_start + 1_000, "completed", 1, 1, Some(2), 2);
        insert(&conn, "A", day_start + 2_000, "completed", 1, 1, Some(2), 2);
        insert(&conn, "B", day_start + 3_000, "completed", 1, 1, Some(1), 1);
        // 昨日 turn T3(model A,2 次调用)→ A:1
        insert(&conn, "A", yesterday_start + 1_000, "completed", 1, 1, Some(2), 2);
        insert(&conn, "A", yesterday_start + 2_000, "completed", 1, 1, Some(2), 2);
        // 非 completed 行不参与
        insert(&conn, "A", day_start + 4_000, "error", 9, 9, Some(9), 9);

        let mut store = crate::collector::store::Store::open_in_memory().unwrap();
        // 模拟 collect 的行级粗值:今日按「行数」计(2+1+1=4,含 1 行 error 不入库);
        // 粗值用真实日期落库,供重算覆盖
        let today_ymd = today.format("%Y-%m-%d").to_string();
        let yesterday_ymd = (today - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        {
            let mut b = Batch::default();
            b.add(&today_ymd, "zcode", "A", 1, 1, 3, 1);
            b.add(&today_ymd, "zcode", "A", 1, 1, 3, 1);
            b.add(&today_ymd, "zcode", "B", 1, 1, 3, 1);
            b.add(&yesterday_ymd, "zcode", "A", 1, 1, 3, 1);
            b.add(&yesterday_ymd, "zcode", "A", 1, 1, 3, 1);
            store.commit("zcode", &b).unwrap();
        }
        let before = store.month_rows(today.format("%Y-%m").to_string().as_str(), "agent", "total", today).unwrap();
        let z = before.iter().find(|r| r.key == "zcode").unwrap();
        assert_eq!(z.message_counts[today.day() as usize - 1], 3, "行级粗值 = 模型调用行数");

        ZcodeAdapter::apply_recalc(&conn, &mut store, yesterday_start, day_start + 86_400_000);

        // 重算后:今日 A=1 turn + B=1 turn(合计 2,与官方「消息数」口径一致);
        // 昨日 A=1 turn。error 行不计。
        let after = store.month_rows(today.format("%Y-%m").to_string().as_str(), "agent", "total", today).unwrap();
        let z = after.iter().find(|r| r.key == "zcode").unwrap();
        assert_eq!(z.message_counts[today.day() as usize - 1], 2, "今日 distinct turn 应为 2");
        assert_eq!(z.message_counts[today.pred_opt().unwrap().day() as usize - 1], 1, "昨日 distinct turn 应为 1");
    }
}
