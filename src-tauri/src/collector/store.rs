//! 采集器本地存储（app_data_dir/collector.db，rusqlite bundled）。
//!
//! 与旧项目（事件表 + 去重键 + retention）的最大差异：事件不落明细库——
//! 契约所需的最低粒度即 `day × agent × model`（矩阵/钻取/导出全是这个粒度），
//! 因此只存聚合表。幂等性由「游标与聚合同事务提交、游标严格不重复消费」保证。
//!
//! 三张聚合表 + 一张对账表：
//! - `daily_usage` 聚合表（PRIMARY KEY 去重，upsert += 累加）
//! - `source_cursor` 每源每 scope 的增量游标（JSON）
//! - `source_state` 每源健康状态（list_sources 的数据源）
//! - `request_model` 官网导出对账映射（CodeBuddy 请求 ID → 模型，见 collector/imports.rs）

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use chrono::NaiveDate;
use rusqlite::Connection;

/// 一次采集批次的提交物：聚合增量 + 游标推进，同事务落库。
#[derive(Default)]
pub struct Batch {
    /// （day, agent_key, model_key) → [input, output, total, requests] 累加量。
    /// requests = 请求/回合数（每 add 一次计 1：zcode model_usage 行、claude
    /// assistant 行、codex token_count 回合、workbuddy function_call、codebuddy request）。
    pub entries: BTreeMap<(String, String, String), [i64; 4]>,
    /// （day, hour, agent_key, model_key) → [input, output, total] 累加量
    /// （小时粒度,与日聚合同事务提交;hour = 本地时 0-23）。
    pub hourly: BTreeMap<(String, u8, String, String), [i64; 3]>,
    /// （scope, cursor_json) 游标推进（同事务提交，防崩溃重计）。
    pub cursors: Vec<(String, String)>,
    /// 实际采集到的事件数（用于 events_collected 与 usage:changed 判定）。
    pub events: u64,
}

impl Batch {
    /// `turns`：该事件是否开启一个新对话轮（0/1）。turn 由「真实用户输入后的
    /// 第一条带 usage 的行」判定（pending 标志,游标持久化）,无该概念的源传 1
    /// （zcode 走按日重算覆盖,粗值不影响权威值;codebuddy 的 request 行天然=轮）。
    /// `hour`：本地小时（0-23）;None = 该源未提供时间（只入日表）。
    pub fn add(&mut self, day: &str, agent: &str, model: &str, input: i64, output: i64, total: i64, turns: i64) {
        self.add_hour(day, None, agent, model, input, output, total, turns);
    }

    /// 带 hour 的完整入口。日表照常累加（日视图口径永不变）,
    /// 小时表仅在有 hour 时累加。
    pub fn add_hour(
        &mut self,
        day: &str,
        hour: Option<u8>,
        agent: &str,
        model: &str,
        input: i64,
        output: i64,
        total: i64,
        turns: i64,
    ) {
        if total == 0 && input == 0 && output == 0 {
            return;
        }
        let e = self
            .entries
            .entry((day.to_string(), agent.to_string(), model.to_string()))
            .or_insert([0, 0, 0, 0]);
        e[0] += input;
        e[1] += output;
        e[2] += total;
        e[3] += turns;
        if let Some(h) = hour {
            let he = self
                .hourly
                .entry((day.to_string(), h, agent.to_string(), model.to_string()))
                .or_insert([0, 0, 0]);
            he[0] += input;
            he[1] += output;
            he[2] += total;
        }
        self.events += 1;
    }
}

pub struct StoreRow {
    pub key: String,
    pub label: String,
    /// 下标 0 = 1 号；None=未来日期，Some（0)=真实零。
    pub values: Vec<Option<i64>>,
    /// 与 values 平行的请求/对话数（未来日为 0；hover「N messages」用）。
    pub message_counts: Vec<i64>,
    pub month_total: i64,
}

pub struct StoreSlice {
    pub key: String,
    pub label: String,
    pub tokens: i64,
}

pub struct StoreBreakdownDay {
    pub day: String,
    pub slices: Option<Vec<StoreSlice>>,
}

pub struct SourceState {
    pub probe_status: String,
    pub schema_fingerprint: Option<String>,
    pub last_success_at: Option<i64>,
    pub last_attempt_at: Option<i64>,
    pub last_error_code: Option<String>,
    pub last_error_message: Option<String>,
    pub events_collected: i64,
}

// 数据洞察:credit 月报的查询结果（经 commands.rs 映射为 serde 契约)。
pub struct CreditModelRow {
    pub key: String,
    pub label: String,
    pub credit: f64,
    pub requests: i64,
}

pub struct CreditDayRow {
    pub day: String,
    pub credit: f64,
}

/// credit 按模型×日序列（双组图）:一个模型的逐日 credit。
pub struct CreditModelDayRow {
    pub key: String,
    pub label: String,
    /// 连续日轴（月内 1 号 → 月末/今天）上该模型的逐日 credit,缺失日补 0。
    pub by_day: Vec<CreditDayRow>,
}

pub struct CreditSummary {
    pub has_data: bool,
    pub total_credit: f64,
    pub total_requests: i64,
    pub by_model: Vec<CreditModelRow>,
    pub by_day: Vec<CreditDayRow>,
    /// 模型维逐日 credit（非 WB 行口径,与 by_model 同一过滤;双组图按模型曲线用）。
    pub by_model_day: Vec<CreditModelDayRow>,
}

/// 时间范围序列:后端按 bucket 聚合的连续序列 + 系列（维度分组）。
pub struct RangeSeriesPoint {
    /// bucket key:day 粒度 = "YYYY-MM-DD";hour 粒度 = "YYYY-MM-DD HH"（HH 本地时）。
    pub bucket: String,
    /// 系列值,与 series_keys 一一对应;缺失补 0。
    pub values: Vec<i64>,
}

pub struct RangeSeries {
    /// 系列标识（= 维度 key,如 agent_key / model_key),与 point.values 对齐。
    pub series_keys: Vec<String>,
    pub series_labels: Vec<String>,
    pub points: Vec<RangeSeriesPoint>,
}

pub struct Store {
    conn: Connection,
}

pub fn days_in_month(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let first_next = NaiveDate::from_ymd_opt(ny, nm, 1).expect("valid next month");
    (first_next - NaiveDate::from_ymd_opt(y, m, 1).expect("valid month")).num_days() as u32
}

fn parse_month(month: &str) -> Option<(i32, u32)> {
    let (y, m) = month.split_once('-')?;
    let y: i32 = y.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if !(1..=12).contains(&m) {
        return None;
    }
    Some((y, m))
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        Self::init(conn)
    }

    /// 测试用内存库。
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, String> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 4000;
             CREATE TABLE IF NOT EXISTS daily_usage (
                 day           TEXT    NOT NULL,
                 agent_key     TEXT    NOT NULL,
                 model_key     TEXT    NOT NULL,
                 input_tokens  INTEGER NOT NULL DEFAULT 0,
                 output_tokens INTEGER NOT NULL DEFAULT 0,
                 total_tokens  INTEGER NOT NULL DEFAULT 0,
                 request_count INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (day, agent_key, model_key)
             );
             CREATE TABLE IF NOT EXISTS source_cursor (
                 source_id   TEXT NOT NULL,
                 scope       TEXT NOT NULL,
                 cursor_json TEXT NOT NULL,
                 updated_at  INTEGER NOT NULL,
                 PRIMARY KEY (source_id, scope)
             );
             CREATE TABLE IF NOT EXISTS source_state (
                 source_id           TEXT PRIMARY KEY,
                 probe_status        TEXT NOT NULL DEFAULT 'no_source',
                 schema_fingerprint  TEXT,
                 last_success_at     INTEGER,
                 last_attempt_at     INTEGER,
                 last_error_code     TEXT,
                 last_error_message  TEXT,
                 events_collected    INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS request_model (
                 source_id   TEXT NOT NULL,
                 request_id  TEXT NOT NULL,
                 model_key   TEXT NOT NULL,
                 client      TEXT,
                 day         TEXT,
                 credit      REAL,
                 PRIMARY KEY (source_id, request_id)
             );",
        )
        .map_err(|e| e.to_string())?;
        // 版本迁移：v1 无 request_count 列;v2 = 模型调用行数（差一个数量级);
        // v3 = zcode distinct turn（其余源仍行级);v4 = 全源对话轮次（claude/wb
        // pending 流式);v5 = codex 也接入 pending（其 token_count 是 API 回合级,
        // event_msg/user_message 才是用户输入)→ 清库重扫。
        // v6 = 小时粒度:新增 hourly_usage（day,hour,agent,model),
        // 清 daily_usage+source_cursor 重扫（游标已推进,历史行补不上小时维)。
        // 口径语义：`request_count` = 用户发起的对话轮次（迁移起全源统一）。
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        if version < 6 {
            conn.execute_batch(
                "DROP TABLE IF EXISTS daily_usage;
                 DROP TABLE IF EXISTS source_cursor;
                 CREATE TABLE IF NOT EXISTS daily_usage (
                     day           TEXT    NOT NULL,
                     agent_key     TEXT    NOT NULL,
                     model_key     TEXT    NOT NULL,
                     input_tokens  INTEGER NOT NULL DEFAULT 0,
                     output_tokens INTEGER NOT NULL DEFAULT 0,
                     total_tokens  INTEGER NOT NULL DEFAULT 0,
                     request_count INTEGER NOT NULL DEFAULT 0,
                     PRIMARY KEY (day, agent_key, model_key)
                 );
                 CREATE TABLE IF NOT EXISTS hourly_usage (
                     day           TEXT    NOT NULL,
                     hour          INTEGER NOT NULL,
                     agent_key     TEXT    NOT NULL,
                     model_key     TEXT    NOT NULL,
                     input_tokens  INTEGER NOT NULL DEFAULT 0,
                     output_tokens INTEGER NOT NULL DEFAULT 0,
                     total_tokens  INTEGER NOT NULL DEFAULT 0,
                     PRIMARY KEY (day, hour, agent_key, model_key)
                 );
                 CREATE TABLE IF NOT EXISTS source_cursor (
                     source_id   TEXT NOT NULL,
                     scope       TEXT NOT NULL,
                     cursor_json TEXT NOT NULL,
                     updated_at  INTEGER NOT NULL,
                     PRIMARY KEY (source_id, scope)
                 );
                 PRAGMA user_version = 6;",
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(Store { conn })
    }

    pub fn get_cursor(&self, source_id: &str, scope: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT cursor_json FROM source_cursor WHERE source_id = ?1 AND scope = ?2",
                [source_id, scope],
                |r| r.get(0),
            )
            .ok()
    }

    /// 聚合增量与游标推进同事务提交：崩溃时二者要么都在要么都不在。
    pub fn commit(&mut self, source_id: &str, batch: &Batch) -> Result<(), String> {
        let now = now_millis();
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO daily_usage (day, agent_key, model_key, input_tokens, output_tokens, total_tokens, request_count)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(day, agent_key, model_key) DO UPDATE SET
                        input_tokens  = input_tokens  + excluded.input_tokens,
                        output_tokens = output_tokens + excluded.output_tokens,
                        total_tokens  = total_tokens  + excluded.total_tokens,
                        request_count = request_count + excluded.request_count",
                )
                .map_err(|e| e.to_string())?;
            for ((day, agent, model), [input, output, total, requests]) in &batch.entries {
                stmt.execute(rusqlite::params![day, agent, model, input, output, total, requests])
                    .map_err(|e| e.to_string())?;
            }
        }
        // 小时粒度:与日聚合同事务,守恒关系 hourly（日合计) == daily。
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO hourly_usage (day, hour, agent_key, model_key, input_tokens, output_tokens, total_tokens)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(day, hour, agent_key, model_key) DO UPDATE SET
                        input_tokens  = input_tokens  + excluded.input_tokens,
                        output_tokens = output_tokens + excluded.output_tokens,
                        total_tokens  = total_tokens  + excluded.total_tokens",
                )
                .map_err(|e| e.to_string())?;
            for ((day, hour, agent, model), [input, output, total]) in &batch.hourly {
                stmt.execute(rusqlite::params![day, *hour as i64, agent, model, input, output, total])
                    .map_err(|e| e.to_string())?;
            }
        }
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO source_cursor (source_id, scope, cursor_json, updated_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(source_id, scope) DO UPDATE SET
                        cursor_json = excluded.cursor_json, updated_at = excluded.updated_at",
                )
                .map_err(|e| e.to_string())?;
            for (scope, json) in &batch.cursors {
                stmt.execute(rusqlite::params![source_id, scope, json, now])
                    .map_err(|e| e.to_string())?;
            }
        }
        if batch.events > 0 {
            tx.execute(
                "UPDATE source_state SET events_collected = events_collected + ?1 WHERE source_id = ?2",
                rusqlite::params![batch.events as i64, source_id],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// 采集成功后更新源状态（probe_status=ready，清除错误）。
    pub fn record_success(&mut self, source_id: &str, fingerprint: Option<&str>) {
        let _ = self.conn.execute(
            "INSERT INTO source_state (source_id, probe_status, schema_fingerprint, last_success_at, last_attempt_at)
             VALUES (?1, 'ready', ?2, ?3, ?3)
             ON CONFLICT(source_id) DO UPDATE SET
                probe_status = 'ready',
                schema_fingerprint = COALESCE(?2, schema_fingerprint),
                last_success_at = ?3, last_attempt_at = ?3,
                last_error_code = NULL, last_error_message = NULL",
            rusqlite::params![source_id, fingerprint, now_millis()],
        );
    }

    /// 采集失败：错误码直接作为 probe_status（busy/error/no_source/unsupported_schema…）。
    pub fn record_failure(&mut self, source_id: &str, code: &str, message: &str) {
        let _ = self.conn.execute(
            "INSERT INTO source_state (source_id, probe_status, last_attempt_at, last_error_code, last_error_message)
             VALUES (?1, ?2, ?3, ?2, ?4)
             ON CONFLICT(source_id) DO UPDATE SET
                probe_status = ?2, last_attempt_at = ?3,
                last_error_code = ?2, last_error_message = ?4",
            rusqlite::params![source_id, code, now_millis(), message],
        );
    }

    /// 用源真值**覆盖**某日某 agent 的 request_count（distinct turn 不可加,行级
    /// 增量只写粗值,权威值由采集端按日重算后经此覆盖）。先清该日该 agent 全部
    /// 计数再逐 model 写入——turn 归属的 model 集合可能缩小。
    pub fn apply_turn_counts(&mut self, agent: &str, day: &str, counts: &[(String, i64)]) {
        if self
            .conn
            .execute(
                "UPDATE daily_usage SET request_count = 0 WHERE day = ?1 AND agent_key = ?2",
                rusqlite::params![day, agent],
            )
            .is_err()
        {
            return;
        }
        for (model, n) in counts {
            let _ = self.conn.execute(
                "UPDATE daily_usage SET request_count = ?1
                 WHERE day = ?2 AND agent_key = ?3 AND model_key = ?4",
                rusqlite::params![n, day, agent, model],
            );
        }
    }

    // ---------- 官网导出对账（request_model,见 collector/imports.rs） ----------

    /// 批量 upsert 官网导出的 RequestID→模型映射。
    /// 返回「覆盖性指标」：新 ID 数 + 模型被正的既有 ID 数——用于判断
    /// 是否需要失效关联源重扫（纯积分刷新不影响矩阵,不算变更）。
    pub fn import_request_models(&mut self, source_id: &str, rows: &[(String, String, Option<String>, String, Option<f64>)]) -> Result<(usize, usize), String> {
        let mut added = 0usize;
        let mut corrected = 0usize;
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO request_model (source_id, request_id, model_key, client, day, credit)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(source_id, request_id) DO UPDATE SET
                        model_key = excluded.model_key, client = excluded.client,
                        day = excluded.day, credit = excluded.credit",
                )
                .map_err(|e| e.to_string())?;
            for (request_id, model_key, client, day, credit) in rows {
                let prev: Option<String> = tx
                    .query_row(
                        "SELECT model_key FROM request_model WHERE source_id = ?1 AND request_id = ?2",
                        rusqlite::params![source_id, request_id],
                        |r| r.get(0),
                    )
                    .ok();
                match &prev {
                    None => added += 1,
                    Some(old) if old != model_key => corrected += 1,
                    _ => {}
                }
                stmt.execute(rusqlite::params![source_id, request_id, model_key, client, day, credit])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok((added, corrected))
    }

    /// 按 ID 批量查模型归属。只返回映射存在的 ID。
    /// **只认 CodeBuddy 客户端行**：导出账本混入 WorkBuddy 行（共享积分）,
    /// 其 RequestID 与 CodeBuddy 本地 ID 空间不相交,但防御性排除,
    /// 避免异常 ID 撞车时把 workbuddy 的模型错配给 codebuddy 请求。
    pub fn request_models(&self, source_id: &str, request_ids: &[String]) -> HashMap<String, String> {
        let mut out = HashMap::new();
        if request_ids.is_empty() {
            return out;
        }
        // 单条 prepared 查询循环即可（每文件 ≤数百条;IN 列表拼接反而引入上限问题）。
        let mut stmt = match self.conn.prepare_cached(
            "SELECT model_key FROM request_model
             WHERE source_id = ?1 AND request_id = ?2
               AND (client IS NULL OR client NOT LIKE 'WorkBuddy%')",
        ) {
            Ok(s) => s,
            Err(_) => return out,
        };
        for id in request_ids {
            if let Ok(model) = stmt.query_row(rusqlite::params![source_id, id], |r| r.get::<_, String>(0)) {
                out.insert(id.clone(), model);
            }
        }
        out
    }

    /// 映射表行数（list_sources 诊断展示）。
    pub fn request_model_count(&self, source_id: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM request_model WHERE source_id = ?1",
                [source_id],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// 失效某源全部聚合（导入带来新映射/正后调用）：清该源 daily_usage、
    /// hourly_usage 与全部游标,下轮 collect 从头重读。request_model 表本身
    /// 保留（就是对账依据）。
    pub fn invalidate_source(&mut self, source_id: &str) -> Result<u64, String> {
        let n = self
            .conn
            .execute("DELETE FROM daily_usage WHERE agent_key = ?1", [source_id])
            .map_err(|e| e.to_string())?;
        self.conn
            .execute("DELETE FROM hourly_usage WHERE agent_key = ?1", [source_id])
            .map_err(|e| e.to_string())?;
        self.conn
            .execute("DELETE FROM source_cursor WHERE source_id = ?1", [source_id])
            .map_err(|e| e.to_string())?;
        Ok(n as u64)
    }

    pub fn source_state(&self, source_id: &str) -> SourceState {
        let row = self
            .conn
            .query_row(
                "SELECT probe_status, schema_fingerprint, last_success_at, last_attempt_at,
                        last_error_code, last_error_message, events_collected
                 FROM source_state WHERE source_id = ?1",
                [source_id],
                |r| {
                    Ok(SourceState {
                        probe_status: r.get(0)?,
                        schema_fingerprint: r.get(1)?,
                        last_success_at: r.get(2)?,
                        last_attempt_at: r.get(3)?,
                        last_error_code: r.get(4)?,
                        last_error_message: r.get(5)?,
                        events_collected: r.get(6)?,
                    })
                },
            )
            .ok();
        row.unwrap_or(SourceState {
            probe_status: "no_source".to_string(),
            schema_fingerprint: None,
            last_success_at: None,
            last_attempt_at: None,
            last_error_code: None,
            last_error_message: None,
            events_collected: 0,
        })
    }

    /// 月度矩阵（group_by: "agent" | "model"；metric: "total" | "input" | "output"）。
    /// 键集 = 月内有记录的 key；无记录日按 null ≠ 0 语义补位；行按月总量降序。
    pub fn month_rows(&self, month: &str, group_by: &str, metric: &str, today: NaiveDate) -> Option<Vec<StoreRow>> {
        let (y, m) = parse_month(month)?;
        let first = NaiveDate::from_ymd_opt(y, m, 1)?;
        let dim = days_in_month(y, m) as usize;
        let metric_col = match metric {
            "input" => "input_tokens",
            "output" => "output_tokens",
            _ => "total_tokens",
        };
        let (key_col, label_of) = match group_by {
            "model" => ("model_key", Box::new(model_label) as Box<dyn Fn(&str) -> String + Send>),
            _ => ("agent_key", Box::new(agent_label) as Box<dyn Fn(&str) -> String + Send>),
        };
        let prefix = format!("{}-%", month);

        // 月内各 key 的每日合计与月合计一次查完（tokens 与请求数两列）
        let sql = format!(
            "SELECT {key_col} AS k, substr(day, 9) AS d, SUM({metric_col}) AS v, SUM(request_count) AS rc
             FROM daily_usage WHERE day LIKE ?1 GROUP BY k, d"
        );
        let mut daily: BTreeMap<String, Vec<(usize, i64)>> = BTreeMap::new();
        let mut reqs: BTreeMap<String, Vec<(usize, i64)>> = BTreeMap::new();
        let mut stmt = self.conn.prepare(&sql).ok()?;
        let rows = stmt
            .query_map([&prefix], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .ok()?;
        for (k, d, v, rc) in rows.flatten() {
            // d 是 "DD" 文本（substr（day, 9)），转 1-based 下标
            if let Ok(day_num) = d.parse::<usize>() {
                if day_num >= 1 && day_num <= dim {
                    daily.entry(k.clone()).or_default().push((day_num - 1, v));
                    reqs.entry(k).or_default().push((day_num - 1, rc));
                }
            }
        }
        drop(stmt);

        let mut out: Vec<StoreRow> = daily
            .into_iter()
            .map(|(k, cells)| {
                // 契约语义先铺底：过去无记录日 = Some（0)（灰格），未来 = None（透明格）
                let mut values: Vec<Option<i64>> = (0..dim)
                    .map(|i| {
                        let date = first + chrono::Duration::days(i as i64);
                        if date > today { None } else { Some(0) }
                    })
                    .collect();
                let mut message_counts = vec![0i64; dim];
                let mut month_total = 0i64;
                for (idx, v) in cells {
                    values[idx] = Some(v);
                    month_total += v;
                }
                for (idx, rc) in reqs.remove(&k).unwrap_or_default() {
                    message_counts[idx] = rc;
                }
                StoreRow { label: label_of(&k), values, message_counts, month_total, key: k }
            })
            .collect();
        out.sort_by(|a, b| b.month_total.cmp(&a.month_total).then(a.key.cmp(&b.key)));
        Some(out)
    }

    /// 行钻取：kind="agent" → 每日模型构成；kind="model" → 每日 Agent 构成。
    /// 未来日期不产出 breakdown day（旧契约）。
    pub fn breakdown(&self, kind: &str, key: &str, month: &str, today: NaiveDate) -> Option<Vec<StoreBreakdownDay>> {
        let (y, m) = parse_month(month)?;
        let first = NaiveDate::from_ymd_opt(y, m, 1)?;
        let dim = days_in_month(y, m);
        let prefix = format!("{}-%", month);
        let (filter_col, slice_col) = match kind {
            "model" => ("model_key", "agent_key"),
            _ => ("agent_key", "model_key"),
        };
        let sql = format!(
            "SELECT day, {slice_col} AS k, SUM(total_tokens) AS v
             FROM daily_usage WHERE {filter_col} = ?1 AND day LIKE ?2
             GROUP BY day, k ORDER BY day"
        );
        let mut per_day: BTreeMap<String, Vec<(String, i64)>> = BTreeMap::new();
        let mut stmt = self.conn.prepare(&sql).ok()?;
        let rows = stmt
            .query_map(rusqlite::params![key, prefix], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .ok()?;
        for (day, k, v) in rows.flatten() {
            per_day.entry(day).or_default().push((k, v));
        }
        drop(stmt);

        let mut out = Vec::with_capacity(dim as usize);
        for d in 0..dim {
            let date = first + chrono::Duration::days(d as i64);
            if date > today {
                break;
            }
            let day_key = date.format("%Y-%m-%d").to_string();
            let mut slices: Vec<StoreSlice> = per_day
                .remove(&day_key)
                .map(|cells| {
                    cells
                        .into_iter()
                        .filter(|(_, v)| *v > 0)
                        .map(|(k, v)| StoreSlice { label: match kind { "model" => agent_label(&k), _ => model_label(&k) }, key: k, tokens: v })
                        .collect()
                })
                .unwrap_or_default();
            slices.sort_by(|a, b| b.tokens.cmp(&a.tokens));
            out.push(StoreBreakdownDay {
                day: day_key,
                slices: if slices.is_empty() { None } else { Some(slices) },
            });
        }
        Some(out)
    }

    /// 备份快照：SQLite `VACUUM INTO`（运行中安全的一致性导出,含 WAL 内容合并）。
    pub fn vacuum_into(&self, dest: &Path) -> Result<(), String> {
        if dest.exists() {
            return Err(format!("backup target exists: {}", dest.display()));
        }
        let sql = format!("VACUUM INTO '{}'", dest.display().to_string().replace('\'', "''"));
        self.conn.execute(&sql, []).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 导出明细（聚合粒度即契约明细粒度）：day, agent, model, total>0。
    pub fn export_rows(&self, month: &str) -> Result<Vec<(String, String, String, i64)>, String> {
        let prefix = format!("{}-%", month);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT day, agent_key, model_key, total_tokens FROM daily_usage
                 WHERE day LIKE ?1 AND total_tokens > 0 ORDER BY day, agent_key, model_key",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([&prefix], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.flatten().collect())
    }

    /// 时间范围序列（后端聚合）。
    ///
    /// - bucket: "day"（daily_usage 表）| "hour"（hourly_usage 表,bucket = day+HH）
    /// - dimension: "agent" | "model" | "total"（单系列全合计）
    /// - metric: "total" | "input" | "output"
    /// - filter: 可选维度筛选（Some（（dim,key)) 只保留该维度指定 key 的行;
    ///   dim 必须与聚合维度同族——agent 维可用 agent 筛,model 维可用 model 筛;
    ///   跨族筛选在 SQL 里同样成立（如 model 维度只看 zcode 的系列构成）。
    /// - range: [start_day, end_day] 闭区间（"YYYY-MM-DD"）
    ///
    /// 每系列在范围内连续补零（含全零中间日）,前端不重拼;系列按范围内总量降序。
    pub fn range_series(
        &self,
        start_day: &str,
        end_day: &str,
        bucket: &str,
        dimension: &str,
        metric: &str,
        filter: Option<(&str, &str)>,
    ) -> Option<RangeSeries> {
        let (table, bucket_expr) = match bucket {
            "hour" => ("hourly_usage", "day || ' ' || printf('%02d', hour)"),
            _ => ("daily_usage", "day"),
        };
        let metric_col = match metric {
            "input" => "input_tokens",
            "output" => "output_tokens",
            _ => "total_tokens",
        };
        // 聚合维度与筛选谓词相互独立:任一维都可作为系列轴,另一维用 filter 收窄。
        let (key_col, label_of): (&str, Box<dyn Fn(&str) -> String + Send>) = match dimension {
            "model" => ("model_key", Box::new(model_label)),
            "total" => ("'__total__'", Box::new(|_| "全部".to_string())),
            "agent" => ("agent_key", Box::new(agent_label)),
            _ => return None,
        };
        let mut where_clauses: Vec<String> = vec!["day >= ?1".into(), "day <= ?2".into()];
        let mut params: Vec<String> = vec![start_day.to_string(), end_day.to_string()];
        if let Some((fdim, fkey)) = filter {
            let col = match fdim {
                "model" => "model_key",
                "agent" => "agent_key",
                _ => return None,
            };
            where_clauses.push(format!("{col} = ?{}", params.len() + 1));
            params.push(fkey.to_string());
        }
        let sql = format!(
            "SELECT {key_col} AS k, {bucket_expr} AS b, SUM({metric_col}) AS v
             FROM {table}
             WHERE {} AND {metric_col} > 0
             GROUP BY k, b ORDER BY b",
            where_clauses.join(" AND ")
        );

        let mut per_series: BTreeMap<String, Vec<(String, i64)>> = BTreeMap::new();
        let mut stmt = self.conn.prepare(&sql).ok()?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .ok()?;
        for (k, b, v) in rows.flatten() {
            per_series.entry(k).or_default().push((b, v));
        }
        drop(stmt);

        // 连续 bucket 轴:day 按日历推进;hour 按小时推进（范围过长时由调用方限定了粒度）。
        let axis: Vec<String> = match bucket {
            "hour" => {
                let start = NaiveDate::parse_from_str(start_day, "%Y-%m-%d").ok()?;
                let end = NaiveDate::parse_from_str(end_day, "%Y-%m-%d").ok()?;
                // 逐日 × 24 小时展开（hour 恒 0-23,不走 datetime 解析避免格式歧义）
                let mut out = Vec::new();
                let mut cur = start;
                while cur <= end {
                    for h in 0..24u32 {
                        out.push(format!("{} {h:02}", cur.format("%Y-%m-%d")));
                    }
                    cur += chrono::Duration::days(1);
                }
                out
            }
            _ => {
                let start = NaiveDate::parse_from_str(start_day, "%Y-%m-%d").ok()?;
                let end = NaiveDate::parse_from_str(end_day, "%Y-%m-%d").ok()?;
                let mut cur = start;
                let mut out = Vec::new();
                while cur <= end {
                    out.push(cur.format("%Y-%m-%d").to_string());
                    cur += chrono::Duration::days(1);
                }
                out
            }
        };

        // 系列按范围总量降序;total 维恒单系列（即使无数据也补基线）。
        let mut series: Vec<(String, i64)> = per_series
            .iter()
            .map(|(k, cells)| (k.clone(), cells.iter().map(|(_, v)| v).sum()))
            .collect();
        series.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        if dimension == "total" {
            series = vec![("__total__".to_string(), series.first().map(|(_, v)| *v).unwrap_or(0))];
        }

        let series_keys: Vec<String> = series.iter().map(|(k, _)| k.clone()).collect();
        let series_labels = series_keys.iter().map(|k| label_of(k)).collect();

        let points = axis
            .iter()
            .map(|b| {
                let values = series_keys
                    .iter()
                    .map(|k| {
                        per_series
                            .get(k)
                            .and_then(|cells| cells.iter().find(|(cb, _)| cb == b))
                            .map(|(_, v)| *v)
                            .unwrap_or(0)
                    })
                    .collect();
                RangeSeriesPoint { bucket: b.clone(), values }
            })
            .collect();

        Some(RangeSeries { series_keys, series_labels, points })
    }

    /// credit 月报（数据洞察）：request_model 表按月聚合。
    ///
    /// 口径（与对账链路同一事实源）：
    /// - 月度总量 = **全表**（含 WorkBuddy 行——两家共享积分池,总量就是池消耗）;
    /// - 按模型分布 = **只认非 WorkBuddy 客户端行**（与 request_models 归属查询
    ///   同一过滤;WB 的模型归属由其本地适配器提供,官方账本的 WB 模型列不采信）;
    /// - by_day 按 day 列（官方北京时间,parse_export_day 已归一为 YYYY-MM-DD）;
    /// - has_data = 该月表内是否有任何行（无导入月份 = 无数据,UI 走引导,不渲染 0）。
    pub fn credit_summary(&self, month: &str) -> Option<CreditSummary> {
        parse_month(month)?;
        let prefix = format!("{}-%", month);

        let mut total_credit = 0f64;
        let mut total_requests = 0i64;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT COUNT(*), COALESCE(SUM(credit), 0) FROM request_model
                 WHERE day LIKE ?1",
            )
            .ok()?;
        // COUNT 恒产出一行;查询失败按 （0, 0) 处理 = has_data=false
        if let Ok((n, c)) = stmt.query_row([&prefix], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
        }) {
            total_requests = n;
            total_credit = c;
        }
        drop(stmt);
        let has_data = total_requests > 0;

        // 按模型分布:非 WorkBuddy 客户端行;unknown 归组展示不丢弃。
        let mut by_model: Vec<CreditModelRow> = Vec::new();
        if has_data {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT model_key, COUNT(*), COALESCE(SUM(credit), 0) FROM request_model
                     WHERE day LIKE ?1 AND (client IS NULL OR client NOT LIKE 'WorkBuddy%')
                     GROUP BY model_key ORDER BY 3 DESC",
                )
                .ok()?;
            let rows = stmt
                .query_map([&prefix], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, f64>(2)?,
                    ))
                })
                .ok()?;
            for (k, n, c) in rows.flatten() {
                by_model.push(CreditModelRow { label: model_label(&k), key: k, credit: c, requests: n });
            }
        }

        // 按日走势:全表（与月度总量同口径）。
        let mut by_day: Vec<CreditDayRow> = Vec::new();
        if has_data {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT day, COALESCE(SUM(credit), 0) FROM request_model
                     WHERE day LIKE ?1 GROUP BY day ORDER BY day",
                )
                .ok()?;
            let rows = stmt
                .query_map([&prefix], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
                })
                .ok()?;
            for (d, c) in rows.flatten() {
                by_day.push(CreditDayRow { day: d, credit: c });
            }
        }

        // 按模型×日（双组图）:非 WB 行口径（与 by_model 同一过滤）。
        // 请求级数据,只有有行的日;连续日轴由调用方（命令层）补零。
        let mut by_model_day: Vec<CreditModelDayRow> = Vec::new();
        if has_data {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT model_key, day, COALESCE(SUM(credit), 0) FROM request_model
                     WHERE day LIKE ?1 AND (client IS NULL OR client NOT LIKE 'WorkBuddy%')
                     GROUP BY model_key, day ORDER BY model_key, day",
                )
                .ok()?;
            let rows = stmt
                .query_map([&prefix], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, f64>(2)?,
                    ))
                })
                .ok()?;
            let mut per_model: BTreeMap<String, Vec<CreditDayRow>> = BTreeMap::new();
            for (k, d, c) in rows.flatten() {
                per_model.entry(k).or_default().push(CreditDayRow { day: d, credit: c });
            }
            for (k, cells) in per_model {
                let label = model_label(&k);
                by_model_day.push(CreditModelDayRow { key: k, label, by_day: cells });
            }
        }

        Some(CreditSummary {
            has_data,
            total_credit,
            total_requests,
            by_model,
            by_day,
            by_model_day,
        })
    }
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// agent_key → 展示名（源固定映射，未知 key 原样返回）。
pub fn agent_label(key: &str) -> String {
    match key {
        "zcode" => "ZCode".to_string(),
        "workbuddy" => "WorkBuddy".to_string(),
        "claude-code" => "Claude Code".to_string(),
        "codex" => "Codex".to_string(),
        "codebuddy" => "CodeBuddy".to_string(),
        "dsh" => "DeepSeek Harness".to_string(),
        other => other.to_string(),
    }
}

/// model_key → 展示名（未知模型键原样返回;unknown 是对账未命中的兜底行）。
pub fn model_label(key: &str) -> String {
    match key {
        "unknown" => "Unknown".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(conn: &mut Store, day: &str, agent: &str, model: &str, i: i64, o: i64, t: i64) {
        let mut batch = Batch::default();
        batch.add(day, agent, model, i, o, t, 1);
        conn.commit("test", &batch).unwrap();
    }

    const TODAY: NaiveDate = NaiveDate::from_ymd_opt(2026, 9, 6).unwrap();

    #[test]
    fn upsert_accumulates() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "glm-5.3", 10, 5, 15);
        insert(&mut s, "2026-09-01", "zcode", "glm-5.3", 1, 2, 3);
        let rows = s.month_rows("2026-09", "agent", "total", TODAY).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "zcode");
        assert_eq!(rows[0].values[0], Some(18));
        assert_eq!(rows[0].month_total, 18);
        // 每次 add 计一次请求；两次 add 同一格 → request_count=2
        assert_eq!(rows[0].message_counts[0], 2);
        assert_eq!(rows[0].message_counts.len(), rows[0].values.len());
    }

    #[test]
    fn matrix_null_semantics() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-05", "codex", "gpt-5", 7, 3, 10);
        let rows = s.month_rows("2026-09", "agent", "total", TODAY).unwrap();
        let r = &rows[0];
        assert_eq!(r.values.len(), 30);
        // 过去无记录日 = Some(0)，今天 = Some(0)，未来 = None
        assert_eq!(r.values[0], Some(0));
        assert_eq!(r.values[4], Some(10));
        assert_eq!(r.values[5], Some(0)); // 9-6 = today
        assert!(r.values[6].is_none());
    }

    #[test]
    fn metric_columns() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "m", 30, 12, 42);
        for (metric, expect) in [("total", 42), ("input", 30), ("output", 12)] {
            let rows = s.month_rows("2026-09", "agent", metric, TODAY).unwrap();
            assert_eq!(rows[0].values[0], Some(expect), "metric={}", metric);
        }
    }

    #[test]
    fn agent_model_rows_conserve() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "a", 10, 5, 15);
        insert(&mut s, "2026-09-01", "codex", "a", 4, 1, 5);
        insert(&mut s, "2026-09-01", "zcode", "b", 2, 3, 5);
        let agents = s.month_rows("2026-09", "agent", "total", TODAY).unwrap();
        let models = s.month_rows("2026-09", "model", "total", TODAY).unwrap();
        let sa: i64 = agents.iter().map(|r| r.month_total).sum();
        let sm: i64 = models.iter().map(|r| r.month_total).sum();
        assert_eq!(sa, sm);
        assert_eq!(sa, 25);
        // 降序排列
        assert!(agents.windows(2).all(|w| w[0].month_total >= w[1].month_total));
    }

    #[test]
    fn breakdown_conserves_cell() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "glm", 10, 5, 15);
        insert(&mut s, "2026-09-01", "zcode", "gpt", 0, 0, 7);
        insert(&mut s, "2026-09-02", "zcode", "glm", 1, 1, 2);
        let agents = s.month_rows("2026-09", "agent", "total", TODAY).unwrap();
        let z = agents.iter().find(|r| r.key == "zcode").unwrap();
        assert_eq!(z.values[0], Some(22));
        let bd = s.breakdown("agent", "zcode", "2026-09", TODAY).unwrap();
        // 每天都产出（≤today 共 6 天），无数据日 slices=None
        assert_eq!(bd.len(), 6);
        let d1 = &bd[0];
        let slices = d1.slices.as_ref().unwrap();
        assert_eq!(slices.len(), 2); // glm=15 + gpt=7，零贡献行不存在
        let sum: i64 = slices.iter().map(|x| x.tokens).sum();
        assert_eq!(sum, 22);
        assert_eq!(bd[1].slices.as_ref().unwrap()[0].tokens, 2);
    }

    #[test]
    fn breakdown_model_kind_labels_agents() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "glm-5.3", 10, 5, 15);
        let bd = s.breakdown("model", "glm-5.3", "2026-09", TODAY).unwrap();
        let slice = &bd[0].slices.as_ref().unwrap()[0];
        assert_eq!(slice.key, "zcode");
        assert_eq!(slice.label, "ZCode");
    }

    #[test]
    fn export_rows_filters_zero() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "m", 0, 0, 5);
        insert(&mut s, "2026-09-02", "codex", "m", 0, 0, 0); // 全零不入导出
        let rows = s.export_rows("2026-09").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], ("2026-09-01".into(), "zcode".into(), "m".into(), 5));
    }

    #[test]
    fn cursor_roundtrip() {
        let mut s = Store::open_in_memory().unwrap();
        assert!(s.get_cursor("zcode", "db").is_none());
        let mut b = Batch::default();
        b.cursors.push(("db".into(), "{\"rowid\":42}".into()));
        s.commit("zcode", &b).unwrap();
        assert_eq!(s.get_cursor("zcode", "db").unwrap(), "{\"rowid\":42}");
    }

    #[test]
    fn invalid_month_returns_none() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.month_rows("2026-13", "agent", "total", TODAY).is_none());
        assert!(s.month_rows("bad", "agent", "total", TODAY).is_none());
    }

    #[test]
    fn days_in_month_matches_fixture_semantics() {
        assert_eq!(days_in_month(2026, 9), 30);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
    }

    #[test]
    fn request_model_import_and_lookup() {
        let mut s = Store::open_in_memory().unwrap();
        let rows = vec![
            ("r1".to_string(), "glm-5.3-flash".to_string(), Some("CodeBuddyIDE".to_string()), "2026-09-01".to_string(), Some(1.5)),
            ("r2".to_string(), "deepseek-v4-pro".to_string(), Some("CodeBuddyIDE".to_string()), "2026-09-02".to_string(), None),
            // WorkBuddy 行也入表（共享积分账本）,但 lookup 不区分——过滤责任在调用方
            ("r3".to_string(), "hy4-preview".to_string(), Some("WorkBuddy".to_string()), "2026-09-02".to_string(), Some(0.9)),
        ];
        let (added, corrected) = s.import_request_models("codebuddy", &rows).unwrap();
        assert_eq!((added, corrected), (3, 0));
        // 重导同一批：零新增零修正（幂等,不触发失效）
        let (added, corrected) = s.import_request_models("codebuddy", &rows).unwrap();
        assert_eq!((added, corrected), (0, 0));
        // 模型被修正 → corrected 计数
        let fix = vec![("r2".to_string(), "deepseek-v4-flash".to_string(), None, "2026-09-02".to_string(), None)];
        let (added, corrected) = s.import_request_models("codebuddy", &fix).unwrap();
        assert_eq!((added, corrected), (0, 1));

        let got = s.request_models("codebuddy", &["r1".into(), "r2".into(), "r3".into(), "rX".into()]);
        assert_eq!(got.get("r1").map(|s| s.as_str()), Some("glm-5.3-flash"));
        assert_eq!(got.get("r2").map(|s| s.as_str()), Some("deepseek-v4-flash"));
        assert!(!got.contains_key("r3"), "WorkBuddy 行不参与 codebuddy 归属");
        assert!(!got.contains_key("rX"));
        assert_eq!(s.request_model_count("codebuddy"), 3);
        // 其他 source_id 隔离
        assert_eq!(s.request_model_count("codex"), 0);
    }

    #[test]
    fn invalidate_source_clears_usage_and_cursors_only() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "codebuddy", "unknown", 10, 5, 15);
        insert(&mut s, "2026-09-01", "zcode", "glm", 1, 1, 2);
        let mut b = Batch::default();
        b.cursors.push(("f1".into(), "{\"count\":3}".into()));
        s.commit("codebuddy", &b).unwrap();
        // 对账行不受失效影响
        let rows = vec![("r1".to_string(), "glm-5.3-flash".to_string(), None, "2026-09-01".to_string(), None)];
        s.import_request_models("codebuddy", &rows).unwrap();

        let cleared = s.invalidate_source("codebuddy").unwrap();
        assert_eq!(cleared, 1, "只清 codebuddy 的 daily_usage 行");
        assert!(s.get_cursor("codebuddy", "f1").is_none());
        assert_eq!(s.request_model_count("codebuddy"), 1, "对账表保留");
        // 其他源毫发无损
        let agents = s.month_rows("2026-09", "agent", "total", TODAY).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].key, "zcode");
    }

    fn import_credit_row(s: &mut Store, id: &str, model: &str, client: Option<&str>, day: &str, credit: Option<f64>) {
        let rows = vec![(id.to_string(), model.to_string(), client.map(|c| c.to_string()), day.to_string(), credit)];
        s.import_request_models("codebuddy", &rows).unwrap();
    }

    #[test]
    fn credit_summary_empty_month_is_no_data() {
        let s = Store::open_in_memory().unwrap();
        let sum = s.credit_summary("2026-08").unwrap();
        assert!(!sum.has_data);
        assert_eq!(sum.total_credit, 0.0);
        assert_eq!(sum.total_requests, 0);
        assert!(sum.by_model.is_empty());
        assert!(sum.by_day.is_empty());
        // 非法月份
        assert!(s.credit_summary("bad").is_none());
        assert!(s.credit_summary("2026-13").is_none());
    }

    #[test]
    fn credit_summary_totals_include_workbuddy_model_rows_exclude() {
        let mut s = Store::open_in_memory().unwrap();
        import_credit_row(&mut s, "r1", "glm-5.3-flash", Some("CodeBuddyIDE"), "2026-09-01", Some(1.5));
        import_credit_row(&mut s, "r2", "deepseek-v4-pro", Some("CodeBuddyIDE"), "2026-09-01", Some(2.0));
        // WorkBuddy 行:共享积分池 → 计入总量;模型维度排除
        import_credit_row(&mut s, "r3", "hy4-preview", Some("WorkBuddy"), "2026-09-02", Some(0.9));
        // client 为 NULL 的行:模型维度保留(防御过滤只排 WorkBuddy 前缀)
        import_credit_row(&mut s, "r4", "glm-5.3-flash", None, "2026-09-03", Some(0.6));
        // unknown 模型:归「Unknown」行,不丢弃
        import_credit_row(&mut s, "r5", "unknown", Some("CodeBuddyIDE"), "2026-09-03", Some(1.0));

        let sum = s.credit_summary("2026-09").unwrap();
        assert!(sum.has_data);
        assert_eq!(sum.total_requests, 5);
        assert!((sum.total_credit - 6.0).abs() < 1e-9, "全表求和含 WB 行: 1.5+2+0.9+0.6+1");

        let labels: Vec<&str> = sum.by_model.iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"Unknown"), "unknown 行归组展示不丢弃");
        assert!(!labels.contains(&"hy4-preview"), "WB 行不进模型分布");
        // by_model credit 总和 = 总量 − WB 行(0.9)
        let model_sum: f64 = sum.by_model.iter().map(|r| r.credit).sum();
        assert!((model_sum - 5.1).abs() < 1e-9);
        // 排序:credit 降序
        assert!(sum.by_model.windows(2).all(|w| w[0].credit >= w[1].credit));

        // by_day:全表口径,按日排序
        assert_eq!(sum.by_day.len(), 3);
        assert_eq!(sum.by_day[0].day, "2026-09-01");
        assert!((sum.by_day[0].credit - 3.5).abs() < 1e-9);
        assert!((sum.by_day[2].credit - 1.6).abs() < 1e-9);

        // 隔离:别的月份不受影响
        let aug = s.credit_summary("2026-08").unwrap();
        assert!(!aug.has_data);

        // by_model_day:v3 双组图——非 WB 行口径,模型×日聚合;WB 行(hy4-preview)排除
        assert_eq!(sum.by_model_day.len(), 3, "glm/deepseek/unknown 三系列");
        let glm = sum.by_model_day.iter().find(|m| m.key == "glm-5.3-flash").unwrap();
        assert_eq!(glm.by_day.len(), 2);
        assert!((glm.by_day[0].credit - 1.5).abs() < 1e-9);
        assert!((glm.by_day[1].credit - 0.6).abs() < 1e-9);
        assert!(!sum.by_model_day.iter().any(|m| m.key == "hy4-preview"), "WB 行不进模型×日");
        // 同一天聚合:deepseek 单行单日
        let ds = sum.by_model_day.iter().find(|m| m.key == "deepseek-v4-pro").unwrap();
        assert_eq!(ds.by_day.len(), 1);
        assert!((ds.by_day[0].credit - 2.0).abs() < 1e-9);
        // 空月:by_model_day 同步为空
        assert!(aug.by_model_day.is_empty());
    }

    #[test]
    fn hourly_and_daily_conserve() {
        let mut s = Store::open_in_memory().unwrap();
        let mut b = Batch::default();
        b.add_hour("2026-09-01", Some(9), "zcode", "glm", 10, 5, 15, 1);
        b.add_hour("2026-09-01", Some(9), "zcode", "glm", 1, 1, 2, 0);
        b.add_hour("2026-09-01", Some(14), "zcode", "glm", 3, 0, 3, 1);
        // 无 hour 的事件:只入日表
        b.add("2026-09-01", "codex", "gpt", 4, 1, 5, 1);
        s.commit("test", &b).unwrap();

        // 日表含全部事件;小时表按日合计 == 日表(仅对有 hour 的行)
        let daily = s.month_rows("2026-09", "agent", "total", TODAY).unwrap();
        let z = daily.iter().find(|r| r.key == "zcode").unwrap();
        assert_eq!(z.month_total, 20); // 15+2+3
        let c = daily.iter().find(|r| r.key == "codex").unwrap();
        assert_eq!(c.month_total, 5);

        let (y, m) = (2026i32, 9u32);
        let _ = (y, m);
        // 小时序列守恒:zcode 当日 total 合计 == 日表 zcode 总量
        let series = s.range_series("2026-09-01", "2026-09-01", "hour", "agent", "total", Some(("agent", "zcode"))).unwrap();
        let hour_sum: i64 = series.points.iter().flat_map(|p| p.values.iter()).sum();
        assert_eq!(hour_sum, 20);
        assert_eq!(series.points.len(), 24, "单日 hour 轴恒 24 点补零");
        // bucket key 形如 "2026-09-01 09"
        assert!(series.points.iter().any(|p| p.bucket == "2026-09-01 09"));
        assert!(series.points.iter().any(|p| p.bucket == "2026-09-01 14"));
    }

    #[test]
    fn range_series_day_axis_and_filters() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "glm", 10, 5, 15);
        insert(&mut s, "2026-09-03", "codex", "gpt", 4, 1, 5);

        // 全维单系列:中间日(9-2)补零
        let total = s.range_series("2026-09-01", "2026-09-03", "day", "total", "total", None).unwrap();
        assert_eq!(total.series_keys, vec!["__total__"]);
        assert_eq!(total.points.len(), 3);
        assert_eq!(total.points[0].values, vec![15]);
        assert_eq!(total.points[1].values, vec![0]);
        assert_eq!(total.points[2].values, vec![5]);

        // agent 维两系列按总量降序,逐点对齐
        let ag = s.range_series("2026-09-01", "2026-09-03", "day", "agent", "total", None).unwrap();
        assert_eq!(ag.series_keys, vec!["zcode", "codex"]);
        assert_eq!(ag.series_labels, vec!["ZCode", "Codex"]);
        assert_eq!(ag.points[0].values, vec![15, 0]);
        assert_eq!(ag.points[2].values, vec![0, 5]);

        // model 筛选收窄:只看 zcode
        let filtered = s.range_series("2026-09-01", "2026-09-03", "day", "model", "total", Some(("agent", "zcode"))).unwrap();
        assert_eq!(filtered.series_keys, vec!["glm"]);
        assert_eq!(filtered.points[2].values, vec![0]);

        // metric 列切换
        let inp = s.range_series("2026-09-01", "2026-09-01", "day", "total", "input", None).unwrap();
        assert_eq!(inp.points[0].values, vec![10]);

        // 非法参数
        assert!(s.range_series("bad", "2026-09-03", "day", "total", "total", None).is_none());
        assert!(s.range_series("2026-09-01", "2026-09-03", "day", "bad_dim", "total", None).is_none());
    }

    #[test]
    fn migration_from_v5_creates_hourly_and_resets() {
        // 模拟 v5 旧库（有 daily_usage 旧 schema,无 hourly_usage）重开:
        // init 应 DROP 重建 + 推版本到 6。经临时文件走真实 Store::open 路径。
        let dir = std::env::temp_dir().join(format!("tc_v6_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("migrate.db");
        let _ = std::fs::remove_file(&db);
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE daily_usage (day TEXT, agent_key TEXT, model_key TEXT,
                     input_tokens INTEGER, output_tokens INTEGER, total_tokens INTEGER, request_count INTEGER,
                     PRIMARY KEY (day, agent_key, model_key));
                 INSERT INTO daily_usage VALUES ('2026-09-01','zcode','glm',1,1,2,1);
                 PRAGMA user_version = 5;",
            )
            .unwrap();
        }
        {
            let store = Store::open(&db).unwrap();
            let v: i64 = store.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(v, 6, "v5 库重开应迁移到 v6");
            // 旧数据已清（清库重扫语义）,hourly_usage 可查
            assert_eq!(store.range_series("2026-09-01", "2026-09-01", "hour", "total", "total", None).unwrap().points.len(), 24);
        }
        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_dir(&dir);
    }
}
