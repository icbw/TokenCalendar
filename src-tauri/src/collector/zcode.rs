//! ZCode 适配器：`~/.zcode/cli/db/db.sqlite` 的 `model_usage` 表（rowid 增量）。
//!
//! 口径（跟随旧项目）：total = `provider_total_tokens`，为 0/NULL 时兜底
//! `computed_total_tokens`；input/output 取原始列（cache 分项独立列,不并入）；
//! 仅 `status='completed'` 计入；时间优先 `started_at`（Unix 毫秒）→ 本地日。
//! 数据库被 ZCode 进程实时写入（WAL）：只读打开 + busy_timeout,BUSY 视为可重试降级。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use rusqlite::{OpenFlags, Connection};
use serde_json::json;

use super::store::{Batch, Store};
use super::{
    Adapter, AdapterError, AdapterMeta, CollectOutcome, CollectResult, ProbeOutcome, clamp0,
    millis_to_local_day,
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
    /// 每日 turn 重算节流：同日期 10 分钟内不重复（重算为全表范围查询）。
    turn_recalc: Mutex<HashMap<String, Instant>>,
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
        ZcodeAdapter { db_path, turn_recalc: Mutex::new(HashMap::new()) }
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
        Self::check_schema(&conn)?; // 采集前再校验一次 schema（可能漂移）

        let scope = "db";
        let mut last_rowid: i64 = store
            .get_cursor(META.id, scope)
            .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
            .and_then(|v| v.get("rowid").and_then(|x| x.as_i64()))
            .unwrap_or(0);

        let mut batch = Batch::default();
        let mut months = std::collections::BTreeSet::new();
        let mut total_events = 0u64;
        let sql = format!(
            "SELECT rowid, model_id, started_at, completed_at, status,
                    input_tokens, output_tokens, provider_total_tokens, computed_total_tokens
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
                    ))
                })
                .map_err(|e| AdapterError::new("error", e.to_string()))?;

            let mut fetched = 0usize;
            let mut last_in_batch = last_rowid;
            for row in rows {
                let (rowid, model, started_at, completed_at, status, input, output, provider_total, computed_total) =
                    row.map_err(|e| AdapterError::new("error", e.to_string()))?;
                fetched += 1;
                last_in_batch = last_in_batch.max(rowid);

                // 行级过滤（与旧项目一致）：非 completed 不计,但游标照常推进
                if status.as_deref() != Some("completed") {
                    continue;
                }
                let millis = started_at.or(completed_at);
                let Some(millis) = millis else { continue };
                let Some(day) = millis_to_local_day(millis) else { continue };
                let model = model.filter(|m| !m.is_empty()).unwrap_or_else(|| "unknown".into());

                let input = clamp0(input.unwrap_or(0));
                let output = clamp0(output.unwrap_or(0));
                // total 口径：provider 优先,0/NULL 兜底 computed（防真实用量误报 0）
                let provider = provider_total.unwrap_or(0);
                let total = if provider > 0 { provider } else { computed_total.unwrap_or(0).max(0) };

                batch.add(&day, META.id, &model, input, output, total, 1);
                months.insert(day[..7].to_string());
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

        // request_count 权威化：行级增量写的粗值（=模型调用行数,一个对话轮十几行）
        // 由 distinct turn 重算覆盖。
        self.recalc_turn_counts(store, &months);

        Ok(CollectOutcome { events: total_events, months })
    }
}

impl ZcodeAdapter {
    /// 对涉及日期（一次范围查询)按 （本地日, model) 重算 distinct turn,
    /// 覆盖聚合表计数。同日期 10 分钟节流;范围查询无新数据的日 GROUP BY 自然为空,
    /// apply_turn_counts 对其清零无害。
    fn recalc_turn_counts(&self, store: &mut Store, months: &std::collections::BTreeSet<String>) {
        if months.is_empty() {
            return;
        }
        let now = Instant::now();
        let due: Vec<String> = {
            let mut map = self.turn_recalc.lock().unwrap();
            let mut due = Vec::new();
            // months 由 batch.add 的 day[..7] 收集（月粒度):月内任一新数据日触发
            // 整月范围的一次合并重算,同月 10 分钟节流。
            for m in months {
                let should = match map.get(m) {
                    None => true,
                    Some(t) => now.duration_since(*t) >= Duration::from_secs(600),
                };
                if should {
                    map.insert(m.clone(), now);
                    due.push(m.clone());
                }
            }
            due
        };
        if due.is_empty() {
            return;
        }
        // 月份范围 → 本地毫秒边界
        let (min_ym, max_ym) = (due.iter().min().unwrap(), due.iter().max().unwrap());
        let month_start = |ym: &str| -> Option<i64> {
            let (y, m) = ym_parts(ym)?;
            let first = chrono::NaiveDate::from_ymd_opt(y, m, 1)?;
            Local
                .from_local_datetime(&first.and_hms_opt(0, 0, 0)?)
                .single()
                .map(|dt| dt.timestamp_millis())
        };
        let (Some(start_ms), Some(mut end_ms)) = (month_start(min_ym), month_start(max_ym)) else {
            return;
        };
        // end = 下月 1 号
        let (y, m) = ym_parts(max_ym).unwrap_or_default();
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        if let Some(first_next) = chrono::NaiveDate::from_ymd_opt(ny, nm, 1) {
            if let Some(midnight) = first_next.and_hms_opt(0, 0, 0) {
                if let Some(dt) = Local.from_local_datetime(&midnight).single() {
                    end_ms = dt.timestamp_millis();
                }
            }
        }
        match self.open_ro() {
            Ok(c) => Self::apply_recalc(&c, store, start_ms, end_ms),
            Err(_) => {} // 下轮重试,不影响 tokens 数据
        }
    }

    /// 范围内按 （本地日, model) 重算 distinct turn 并覆盖聚合表。
    fn apply_recalc(conn: &Connection, store: &mut Store, start_ms: i64, end_ms: i64) {
        let Ok(mut stmt) = conn.prepare(
            "SELECT date(started_at/1000, 'unixepoch', 'localtime') AS d, model_id,
                    COUNT(DISTINCT turn_id) AS turns
             FROM model_usage
             WHERE status = 'completed' AND started_at IS NOT NULL
               AND started_at >= ?1 AND started_at < ?2
             GROUP BY d, model_id",
        ) else {
            return;
        };
        let Ok(rows) = stmt.query_map(rusqlite::params![start_ms, end_ms], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?))
        }) else {
            return;
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
    }
}

fn ym_parts(ym: &str) -> Option<(i32, u32)> {
    let (y, m) = ym.split_once('-')?;
    Some((y.parse().ok()?, m.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use crate::collector::store::days_in_month;

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
