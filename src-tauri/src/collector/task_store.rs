//! 任务层持久化（由 `Store:commit` 在同一事务内调用,与聚合 / 游标同提交）。
//!
//! 三层：
//! 1. **原始层** `turn_raw` / `turn_part`：采集器写入的每个会话（含子会话）每轮自身值,
//!    整行覆盖（累加器快照 / ZCode 整会话重建）,幂等。
//! 2. **物化层** `turn`：只含根会话。子会话的轮按开始时间落进父会话对应轮
//!早于父会话首轮 → 挂最后一轮）,token / 调用 /
//!    错误 / 重试 / 模型与工具时间并入;wall / gap / ttft 只取父轮自身（子代理在父轮墙钟内
//!    运行,相加会重复计等待）。`subagent_count` / `subagent_calls` 在此物化。
//! 3. **项目维** `daily_project`：按 （agent, 日) 从原始层重算覆盖。token / model_calls /
//!    turns（= Σ turn_mark) 取 `turn_part`（事件自身的日与模型,与 daily_usage 逐格守恒）;
//!    时间与工具 / 错误取 `turn_raw`（轮的日与模型）,wall / idle / aborted_count 只算根会话。
//!
//! 中止与错误分列（S4-R）:`aborted`（0/1,用户主动中止）与 `error_count`（API / 工具错误）互不计入;
//! 物化时子轮的 error 并入父轮,aborted 只取父轮自身。
//!
//! 离开阈值：运行时值 = designPrefs `idleThresholdMin`（启动载入、`set_idle_threshold` 下发）。
//! 改阈值 = `recompute_all` 全表重算 daily_project（只读原始层）;表内套用的阈值记在
//! source_cursor（`__tasks`, `idle_threshold_ms`) 标记里,采集线程启动时与运行时值不一致即重算
//! （迁移清库会连带清标记,自然触发一次）。

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicI64, Ordering};

use rusqlite::{params, Connection, OptionalExtension};

use super::store::{Batch, SessionRow, TurnRow, IDLE_THRESHOLD_MS};

type Res<T> = Result<T, String>;

/// 离开阈值（分钟）的合法域与 prefs.json 键名（前端 designPrefs.sanitize 同域）。
pub const IDLE_THRESHOLD_MIN_MINUTES: u32 = 1;
pub const IDLE_THRESHOLD_MAX_MINUTES: u32 = 1440;
pub const IDLE_THRESHOLD_PREFS_KEY: &str = "idleThresholdMin";
const MARKER_SOURCE: &str = "__tasks";
const MARKER_SCOPE: &str = "idle_threshold_ms";

static IDLE_THRESHOLD: AtomicI64 = AtomicI64::new(IDLE_THRESHOLD_MS);

/// 当前离开阈值（毫秒）。
pub fn idle_threshold_ms() -> i64 {
    IDLE_THRESHOLD.load(Ordering::SeqCst)
}

/// 下发离开阈值（分钟,越界夹到合法域）。只改运行时值;表内数据由调用方 `recompute_projects`。
pub fn set_idle_threshold_minutes(minutes: u32) {
    let m = minutes.clamp(IDLE_THRESHOLD_MIN_MINUTES, IDLE_THRESHOLD_MAX_MINUTES) as i64;
    IDLE_THRESHOLD.store(m * 60_000, Ordering::SeqCst);
}

/// prefs.json 原文 → 阈值分钟（缺键 / 非整数 / 越界 → None,按默认处理）。
pub fn threshold_from_prefs(json: &str) -> Option<u32> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let m = v.get(IDLE_THRESHOLD_PREFS_KEY)?.as_u64()?;
    u32::try_from(m).ok().filter(|m| (IDLE_THRESHOLD_MIN_MINUTES..=IDLE_THRESHOLD_MAX_MINUTES).contains(m))
}

/// 把阈值合并进 prefs.json 原文（其余键原样保留;原文缺失 / 损坏 → 以空对象起步）。
pub fn prefs_with_threshold(json: Option<&str>, minutes: u32) -> String {
    let mut v = json
        .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    v[IDLE_THRESHOLD_PREFS_KEY] = serde_json::json!(minutes);
    v.to_string()
}

fn err(e: rusqlite::Error) -> String {
    e.to_string()
}

/// 本批的任务层写入 + 物化 + 项目维重算。批次无任务数据时零开销返回。
pub fn apply(conn: &Connection, batch: &Batch) -> Res<()> {
    if batch.turns.is_empty() && batch.sessions.is_empty() && batch.replaced_sessions.is_empty() {
        return Ok(());
    }
    let mut days: BTreeSet<(String, String)> = BTreeSet::new();
    let mut touched: BTreeSet<(String, String)> = BTreeSet::new();

    for (agent, sid) in &batch.replaced_sessions {
        collect_days(conn, agent, sid, None, &mut days)?;
        conn.execute("DELETE FROM turn_part WHERE agent_key = ?1 AND session_id = ?2", params![agent, sid]).map_err(err)?;
        conn.execute("DELETE FROM turn_raw WHERE agent_key = ?1 AND session_id = ?2", params![agent, sid]).map_err(err)?;
        touched.insert((agent.clone(), sid.clone()));
    }
    for ((agent, sid), row) in &batch.sessions {
        upsert_session(conn, agent, row)?;
        touched.insert((agent.clone(), sid.clone()));
    }
    for ((agent, sid, seq), row) in &batch.turns {
        collect_days(conn, agent, sid, Some(*seq), &mut days)?;
        write_turn_raw(conn, agent, row)?;
        days.insert((agent.clone(), row.day.clone()));
        for p in &row.parts {
            days.insert((agent.clone(), p.day.clone()));
        }
        touched.insert((agent.clone(), sid.clone()));
    }

    let mut roots: BTreeSet<(String, String)> = BTreeSet::new();
    for (agent, sid) in &touched {
        refresh_session_span(conn, agent, sid)?;
        roots.insert((agent.clone(), root_of(conn, agent, sid)?));
    }
    for (agent, root) in &roots {
        materialize_root(conn, agent, root)?;
    }
    let threshold = idle_threshold_ms();
    for (agent, day) in &days {
        recompute_day(conn, agent, day, threshold)?;
    }
    Ok(())
}

/// 全表重算 daily_project（原始层有数据的 （agent, 日) ∪ 表内现存的 （agent, 日)）+ 写阈值标记。
pub fn recompute_all(conn: &Connection, idle_threshold_ms: i64) -> Res<usize> {
    let keys: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT agent_key, day FROM turn_raw UNION SELECT agent_key, day FROM turn_part
                 UNION SELECT agent_key, day FROM daily_project",
            )
            .map_err(err)?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(err)?;
        rows.flatten().collect()
    };
    for (agent, day) in &keys {
        recompute_day(conn, agent, day, idle_threshold_ms)?;
    }
    conn.execute(
        "INSERT INTO source_cursor (source_id, scope, cursor_json, updated_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(source_id, scope) DO UPDATE SET cursor_json = excluded.cursor_json, updated_at = excluded.updated_at",
        params![MARKER_SOURCE, MARKER_SCOPE, idle_threshold_ms.to_string(), super::store::now_millis()],
    )
    .map_err(err)?;
    Ok(keys.len())
}

pub fn threshold_marker(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT cursor_json FROM source_cursor WHERE source_id = ?1 AND scope = ?2",
        params![MARKER_SOURCE, MARKER_SCOPE],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| s.parse().ok())
}

fn collect_days(conn: &Connection, agent: &str, sid: &str, seq: Option<i64>, out: &mut BTreeSet<(String, String)>) -> Res<()> {
    let (filter, seq_v) = match seq {
        Some(s) => ("AND turn_seq = ?3", s),
        None => ("AND ?3 = ?3", 0),
    };
    let sql = format!(
        "SELECT day FROM turn_raw WHERE agent_key = ?1 AND session_id = ?2 {filter}
         UNION SELECT day FROM turn_part WHERE agent_key = ?1 AND session_id = ?2 {filter}"
    );
    let mut stmt = conn.prepare_cached(&sql).map_err(err)?;
    let rows = stmt.query_map(params![agent, sid, seq_v], |r| r.get::<_, String>(0)).map_err(err)?;
    for d in rows.flatten() {
        out.insert((agent.to_string(), d));
    }
    Ok(())
}

fn upsert_session(conn: &Connection, agent: &str, row: &SessionRow) -> Res<()> {
    let project = row.project_key.clone().unwrap_or_else(|| super::turns::UNKNOWN_PROJECT.to_string());
    conn.prepare_cached(
        "INSERT INTO session (agent_key, session_id, project_key, parent_id, title, started_at, ended_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(agent_key, session_id) DO UPDATE SET
            project_key = CASE WHEN (?8 = 1 AND excluded.project_key <> 'unknown') OR session.project_key = 'unknown'
                               THEN excluded.project_key ELSE session.project_key END,
            parent_id   = COALESCE(session.parent_id, excluded.parent_id),
            title       = COALESCE(excluded.title, session.title),
            started_at  = MIN(session.started_at, excluded.started_at),
            ended_at    = MAX(COALESCE(session.ended_at, 0), COALESCE(excluded.ended_at, 0))",
    )
    .map_err(err)?
    .execute(params![
        agent,
        row.session_id,
        project,
        row.parent_id,
        row.title,
        row.started_at.unwrap_or(i64::MAX),
        row.ended_at,
        row.project_authoritative as i64
    ])
    .map_err(err)?;
    Ok(())
}

fn write_turn_raw(conn: &Connection, agent: &str, row: &TurnRow) -> Res<()> {
    let (input, output, total) = row.parts.iter().fold((0, 0, 0), |a, p| (a.0 + p.input, a.1 + p.output, a.2 + p.total));
    // 会话行兜底（适配器正常会先 upsert_session;缺失时以轮信息建占位行）
    conn.prepare_cached(
        "INSERT OR IGNORE INTO session (agent_key, session_id, project_key, started_at, ended_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .map_err(err)?
    .execute(params![agent, row.session_id, row.project_key, row.started_at, row.ended_at])
    .map_err(err)?;
    conn.prepare_cached(
        "INSERT OR REPLACE INTO turn_raw (agent_key, session_id, turn_seq, day, project_key, model_key, started_at, ended_at,
             wall_ms, model_ms, tool_ms, ttft_ms, gap_ms, model_calls, tool_calls, error_count, retry_count,
             input_tokens, output_tokens, total_tokens, aborted)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
    )
    .map_err(err)?
    .execute(params![
        agent,
        row.session_id,
        row.turn_seq,
        row.day,
        row.project_key,
        row.model_key,
        row.started_at,
        row.ended_at,
        row.wall_ms,
        row.model_ms,
        row.tool_ms,
        row.ttft_ms,
        row.gap_ms,
        row.model_calls,
        row.tool_calls,
        row.error_count,
        row.retry_count,
        input,
        output,
        total,
        row.aborted as i64
    ])
    .map_err(err)?;
    conn.prepare_cached("DELETE FROM turn_part WHERE agent_key = ?1 AND session_id = ?2 AND turn_seq = ?3")
        .map_err(err)?
        .execute(params![agent, row.session_id, row.turn_seq])
        .map_err(err)?;
    let mut ins = conn
        .prepare_cached(
            "INSERT INTO turn_part (agent_key, session_id, turn_seq, day, model_key, input_tokens, output_tokens, total_tokens, model_calls, turn_mark)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(agent_key, session_id, turn_seq, day, model_key) DO UPDATE SET
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                total_tokens = total_tokens + excluded.total_tokens,
                model_calls = model_calls + excluded.model_calls,
                turn_mark = turn_mark + excluded.turn_mark",
        )
        .map_err(err)?;
    for p in &row.parts {
        ins.execute(params![agent, row.session_id, row.turn_seq, p.day, p.model, p.input, p.output, p.total, p.model_calls, p.turn_mark])
            .map_err(err)?;
    }
    Ok(())
}

/// 会话起止以原始轮为准（标题先于轮到达时的占位 started_at 在此校正）。
fn refresh_session_span(conn: &Connection, agent: &str, sid: &str) -> Res<()> {
    conn.prepare_cached(
        "UPDATE session SET
            started_at = (SELECT MIN(started_at) FROM turn_raw r WHERE r.agent_key = ?1 AND r.session_id = ?2),
            ended_at   = (SELECT MAX(ended_at) FROM turn_raw r WHERE r.agent_key = ?1 AND r.session_id = ?2)
         WHERE agent_key = ?1 AND session_id = ?2
           AND EXISTS (SELECT 1 FROM turn_raw r WHERE r.agent_key = ?1 AND r.session_id = ?2)",
    )
    .map_err(err)?
    .execute(params![agent, sid])
    .map_err(err)?;
    Ok(())
}

/// 沿 parent_id 找根会话（父行缺失时以最后一个已知父 id 为根;深度上限防环）。
fn root_of(conn: &Connection, agent: &str, sid: &str) -> Res<String> {
    let mut cur = sid.to_string();
    let mut seen = BTreeSet::new();
    for _ in 0..16 {
        if !seen.insert(cur.clone()) {
            break;
        }
        let parent: Option<Option<String>> = conn
            .prepare_cached("SELECT parent_id FROM session WHERE agent_key = ?1 AND session_id = ?2")
            .map_err(err)?
            .query_row(params![agent, cur], |r| r.get(0))
            .optional()
            .map_err(err)?;
        match parent.flatten() {
            Some(p) if !p.is_empty() && p != cur => cur = p,
            _ => break,
        }
    }
    Ok(cur)
}

#[derive(Clone, Debug)]
struct RawTurn {
    session_id: String,
    day: String,
    project_key: String,
    model_key: String,
    started_at: i64,
    ended_at: i64,
    wall_ms: Option<i64>,
    model_ms: Option<i64>,
    tool_ms: Option<i64>,
    ttft_ms: Option<i64>,
    gap_ms: Option<i64>,
    model_calls: i64,
    tool_calls: i64,
    error_count: i64,
    retry_count: i64,
    aborted: bool,
    input: i64,
    output: i64,
    total: i64,
}

fn load_raw(conn: &Connection, agent: &str, sid: &str) -> Res<Vec<RawTurn>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT session_id, day, project_key, model_key, started_at, ended_at, wall_ms, model_ms, tool_ms,
                    ttft_ms, gap_ms, model_calls, tool_calls, error_count, retry_count, input_tokens, output_tokens, total_tokens, aborted
             FROM turn_raw WHERE agent_key = ?1 AND session_id = ?2 ORDER BY started_at, turn_seq",
        )
        .map_err(err)?;
    let rows = stmt
        .query_map(params![agent, sid], |r| {
            Ok(RawTurn {
                session_id: r.get(0)?,
                day: r.get(1)?,
                project_key: r.get(2)?,
                model_key: r.get(3)?,
                started_at: r.get(4)?,
                ended_at: r.get(5)?,
                wall_ms: r.get(6)?,
                model_ms: r.get(7)?,
                tool_ms: r.get(8)?,
                ttft_ms: r.get(9)?,
                gap_ms: r.get(10)?,
                model_calls: r.get(11)?,
                tool_calls: r.get(12)?,
                error_count: r.get(13)?,
                retry_count: r.get(14)?,
                input: r.get(15)?,
                output: r.get(16)?,
                total: r.get(17)?,
                aborted: r.get::<_, i64>(18)? != 0,
            })
        })
        .map_err(err)?;
    Ok(rows.flatten().collect())
}

fn add_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0) + y.unwrap_or(0)),
    }
}

/// 子轮归属：父轮区间 [开始, 下一轮开始) 包含子轮开始时间者;早于父会话首轮 → 最后一轮。
fn assign_parent_turn(parent_starts: &[i64], child_start: i64) -> usize {
    match parent_starts.iter().rposition(|s| *s <= child_start) {
        Some(i) => i,
        None => parent_starts.len() - 1,
    }
}

fn materialize_root(conn: &Connection, agent: &str, root: &str) -> Res<()> {
    conn.execute("DELETE FROM turn WHERE agent_key = ?1 AND session_id = ?2", params![agent, root]).map_err(err)?;
    let mut merged = load_raw(conn, agent, root)?;
    let descendants: Vec<String> = {
        let mut stmt = conn
            .prepare_cached(
                "WITH RECURSIVE d(id) AS (
                     SELECT session_id FROM session WHERE agent_key = ?1 AND parent_id = ?2
                     UNION SELECT s.session_id FROM session s JOIN d ON s.parent_id = d.id WHERE s.agent_key = ?1
                 ) SELECT id FROM d WHERE id <> ?2",
            )
            .map_err(err)?;
        let rows = stmt.query_map(params![agent, root], |r| r.get::<_, String>(0)).map_err(err)?;
        rows.flatten().collect()
    };
    if merged.is_empty() {
        conn.execute(
            "UPDATE session SET subagent_count = 0, subagent_calls = 0 WHERE agent_key = ?1 AND session_id = ?2",
            params![agent, root],
        )
        .map_err(err)?;
        return Ok(());
    }
    let starts: Vec<i64> = merged.iter().map(|t| t.started_at).collect();
    let mut sub_sessions: Vec<BTreeSet<String>> = vec![BTreeSet::new(); merged.len()];
    let mut sub_calls = vec![0i64; merged.len()];
    for child in &descendants {
        for c in load_raw(conn, agent, child)? {
            let i = assign_parent_turn(&starts, c.started_at);
            let m = &mut merged[i];
            m.model_calls += c.model_calls;
            m.tool_calls += c.tool_calls;
            m.error_count += c.error_count;
            m.retry_count += c.retry_count;
            // aborted 只取父轮自身（子代理随父轮一起被中止,并入会重复标记;与 wall 同理）
            m.input += c.input;
            m.output += c.output;
            m.total += c.total;
            m.model_ms = add_opt(m.model_ms, c.model_ms);
            m.tool_ms = add_opt(m.tool_ms, c.tool_ms);
            sub_calls[i] += c.model_calls;
            sub_sessions[i].insert(c.session_id.clone());
        }
    }
    let mut ins = conn
        .prepare_cached(
            "INSERT INTO turn (agent_key, session_id, turn_seq, day, project_key, model_key, started_at, ended_at,
                 wall_ms, model_ms, tool_ms, ttft_ms, gap_ms, model_calls, tool_calls, subagent_count, subagent_calls,
                 error_count, retry_count, input_tokens, output_tokens, total_tokens, aborted)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
        )
        .map_err(err)?;
    let mut all_subs: BTreeSet<String> = BTreeSet::new();
    // 物化轮号 = 按开始时间的序号 1..n（原始层 turn_seq 带文件命名空间,不直接展示）
    for (i, t) in merged.iter().enumerate() {
        all_subs.extend(sub_sessions[i].iter().cloned());
        ins.execute(params![
            agent,
            root,
            i as i64 + 1,
            t.day,
            t.project_key,
            t.model_key,
            t.started_at,
            t.ended_at,
            t.wall_ms,
            t.model_ms,
            t.tool_ms,
            t.ttft_ms,
            t.gap_ms,
            t.model_calls,
            t.tool_calls,
            sub_sessions[i].len() as i64,
            sub_calls[i],
            t.error_count,
            t.retry_count,
            t.input,
            t.output,
            t.total,
            t.aborted as i64
        ])
        .map_err(err)?;
    }
    conn.execute(
        "UPDATE session SET subagent_count = ?3, subagent_calls = ?4 WHERE agent_key = ?1 AND session_id = ?2",
        params![agent, root, all_subs.len() as i64, sub_calls.iter().sum::<i64>()],
    )
    .map_err(err)?;
    Ok(())
}

/// 按 （agent, 日) 重算覆盖 daily_project。`idle_threshold_ms`:gap ≤ 阈值才计入 idle_ms。
pub fn recompute_day(conn: &Connection, agent: &str, day: &str, idle_threshold_ms: i64) -> Res<()> {
    conn.prepare_cached("DELETE FROM daily_project WHERE agent_key = ?1 AND day = ?2")
        .map_err(err)?
        .execute(params![agent, day])
        .map_err(err)?;
    conn.prepare_cached(
        "INSERT INTO daily_project (day, agent_key, model_key, project_key, turns, model_calls, subagent_calls,
                                    input_tokens, output_tokens, total_tokens)
         SELECT p.day, p.agent_key, p.model_key, r.project_key, SUM(p.turn_mark), SUM(p.model_calls),
                SUM(CASE WHEN s.parent_id IS NOT NULL THEN p.model_calls ELSE 0 END),
                SUM(p.input_tokens), SUM(p.output_tokens), SUM(p.total_tokens)
         FROM turn_part p
         JOIN turn_raw r ON r.agent_key = p.agent_key AND r.session_id = p.session_id AND r.turn_seq = p.turn_seq
         LEFT JOIN session s ON s.agent_key = p.agent_key AND s.session_id = p.session_id
         WHERE p.agent_key = ?1 AND p.day = ?2
         GROUP BY p.day, p.agent_key, p.model_key, r.project_key",
    )
    .map_err(err)?
    .execute(params![agent, day])
    .map_err(err)?;
    conn.prepare_cached(
        "INSERT INTO daily_project (day, agent_key, model_key, project_key, tool_calls, wall_ms, model_ms, tool_ms, idle_ms, error_count, aborted_count)
         SELECT r.day, r.agent_key, r.model_key, r.project_key, SUM(r.tool_calls),
                SUM(CASE WHEN s.parent_id IS NULL THEN COALESCE(r.wall_ms, 0) ELSE 0 END),
                SUM(COALESCE(r.model_ms, 0)), SUM(COALESCE(r.tool_ms, 0)),
                SUM(CASE WHEN s.parent_id IS NULL AND r.gap_ms IS NOT NULL AND r.gap_ms <= ?3 THEN r.gap_ms ELSE 0 END),
                SUM(r.error_count),
                SUM(CASE WHEN s.parent_id IS NULL THEN r.aborted ELSE 0 END)
         FROM turn_raw r
         LEFT JOIN session s ON s.agent_key = r.agent_key AND s.session_id = r.session_id
         WHERE r.agent_key = ?1 AND r.day = ?2
         GROUP BY r.day, r.agent_key, r.model_key, r.project_key
         ON CONFLICT(day, agent_key, model_key, project_key) DO UPDATE SET
            tool_calls = excluded.tool_calls, wall_ms = excluded.wall_ms, model_ms = excluded.model_ms,
            tool_ms = excluded.tool_ms, idle_ms = excluded.idle_ms, error_count = excluded.error_count,
            aborted_count = excluded.aborted_count",
    )
    .map_err(err)?
    .execute(params![agent, day, idle_threshold_ms])
    .map_err(err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_threshold_roundtrip_keeps_other_keys() {
        assert_eq!(threshold_from_prefs(r#"{"idleThresholdMin":45,"locked":true}"#), Some(45));
        assert_eq!(threshold_from_prefs(r#"{"idleThresholdMin":0}"#), None, "越界按默认");
        assert_eq!(threshold_from_prefs(r#"{"idleThresholdMin":"30"}"#), None);
        assert_eq!(threshold_from_prefs("not json"), None);
        let merged = prefs_with_threshold(Some(r#"{"locked":true,"sizePreset":"large"}"#), 12);
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!((v["locked"].as_bool(), v["sizePreset"].as_str(), v["idleThresholdMin"].as_u64()), (Some(true), Some("large"), Some(12)));
        assert_eq!(threshold_from_prefs(&prefs_with_threshold(None, 90)), Some(90));
        assert_eq!(threshold_from_prefs(&prefs_with_threshold(Some("[1]"), 5)), Some(5), "非对象原文以空对象起步");
    }

    #[test]
    fn parent_turn_assignment_rule() {
        let starts = [100, 200, 300];
        assert_eq!(assign_parent_turn(&starts, 150), 0);
        assert_eq!(assign_parent_turn(&starts, 200), 1);
        assert_eq!(assign_parent_turn(&starts, 999), 2);
        assert_eq!(assign_parent_turn(&starts, 50), 2, "早于首轮 → 挂最后一轮");
    }
}
