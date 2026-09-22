//! 就地迁移（起）：口径变更**只升级库内已有行**,不清库、不依赖源日志仍在。
//!
//! 原则：collector.db 是用量历史唯一的持久副本——源日志会轮换 / 被删（Claude Code 默认清理旧转录、
//! 用户删目录）,「清库重扫」会把源里已经没有的历史一起丢掉。
//! 每次口径迭代: 迁移前 `VACUUM INTO` 一份备份（`Store:open`）; 结构变更走 `CREATE IF NOT EXISTS` /
//! `ALTER`; 口径变更用库内原始层（`turn_raw` / `turn_part` / `session` / `source_cursor`）就地重算;
//!  只有采集器**下次读到**的会话才按新口径由源覆盖,读不到的行原样保留。
//!
//! v13 = 项目归属：
//! - Claude Code:项目 = 会话文件所在的文件夹（源自己的分组,见 `project_dir`）。库内可用信息 =
//!   该会话（含子代理）各轮的 project_key 集合 + `source_cursor.scope` 里的文件路径。取编码后与文件夹一致的键;
//!   无游标（文件已不在源里）时取**最短**键——`cd` 只会漂进启动目录的子目录,最短者即启动目录。
//! - ZCode:子会话项目 = 根会话项目（沿 `session.parent_id`）。
//! - Codex:会话行项目 = 最后一轮的项目（近似 Codex `threads.cwd` = 当前工作区;采集器下次读到该线程时以库内值覆盖）。
//! - 然后 `daily_project` 全表重算（只读原始层）。
//!
//! v14 = 正 v13 解析留下的「文件夹名当项目键」（见 `project_dir:looks_like_path`）:
//! - 凡 Claude Code / WorkBuddy 会话的键不像路径（无盘符、无分隔符）,且库内存在编码后与之一致的真实路径键 →
//!   会话 / 轮 / 原始轮 / 文件游标提示 / `project_meta` 一并改到真实键;`folder:` 映射写成真实键。
//! - 顺带按文件游标为每个文件夹补种 `folder:` 映射,后续采集不必从零解析（从零解析正是产生坏键的入口）。
//! - 找不到真实键的（源里从未出现过 cwd 行）原样保留。
//!
//! v15 = Claude Code 的 total 口径:
//! Anthropic 的 `usage.input_tokens` 是 cache-exclusive 且 JSONL 没有 provider total,
//! `total = input + output` 退化成「约等于 output」,把 Claude 低估两个数量级。改为四项和。
//! - `daily_usage` / `hourly_usage` 本就存着 cache 两列 → **精确就地重算**,源日志在不在都无损。
//! - 原始层 `turn_raw` / `turn_part` 原本没有 cache 列 → 先 `ALTER` 补列（此后采集器写真值,
//!   口径再变可以只读原始层重算）,存量按**日格 cache 总量 ∝ 各 turn_part 的 output** 分摊回填:
//!   日粒度与 `daily_usage` 逐格精确（末位行吃取整余数）,轮粒度是近似。被采集器再次读到的会话
//!   会用源值整行覆盖近似值;源已消失的会话保留近似值,不丢行。
//! - `turn` / `daily_project` 不加列（本就由原始层物化重算）,迁移末尾走 `recompute_all`。

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection};

use super::project_dir::{folder_matches, looks_like_path};
use super::turns::UNKNOWN_PROJECT;

type Res<T> = Result<T, String>;

fn err(e: rusqlite::Error) -> String {
    e.to_string()
}

#[derive(Debug, Default, PartialEq)]
pub struct V13Report {
    pub claude_sessions: usize,
    pub zcode_children: usize,
    pub codex_sessions: usize,
}

/// 根会话 + 全部后代（沿 parent_id,防环）。
fn family(conn: &Connection, agent: &str, root: &str) -> Res<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "WITH RECURSIVE d(id) AS (
                 SELECT ?2
                 UNION SELECT s.session_id FROM session s JOIN d ON s.parent_id = d.id WHERE s.agent_key = ?1
             ) SELECT id FROM d",
        )
        .map_err(err)?;
    let rows = stmt.query_map(params![agent, root], |r| r.get::<_, String>(0)).map_err(err)?;
    Ok(rows.flatten().collect())
}

fn set_family_project(conn: &Connection, agent: &str, ids: &[String], key: &str) -> Res<usize> {
    let mut changed = 0;
    for sid in ids {
        for sql in [
            "UPDATE session SET project_key = ?3 WHERE agent_key = ?1 AND session_id = ?2 AND project_key <> ?3",
            "UPDATE turn_raw SET project_key = ?3 WHERE agent_key = ?1 AND session_id = ?2 AND project_key <> ?3",
            "UPDATE turn SET project_key = ?3 WHERE agent_key = ?1 AND session_id = ?2 AND project_key <> ?3",
        ] {
            changed += conn.execute(sql, params![agent, sid, key]).map_err(err)?;
        }
    }
    Ok(changed)
}

/// 会话文件所在文件夹名（游标 scope = 文件全路径;主文件与子代理文件都在同一文件夹下）。
fn cursor_folder(conn: &Connection, agent: &str, sid: &str) -> Res<Option<String>> {
    let mut stmt = conn
        .prepare("SELECT scope FROM source_cursor WHERE source_id = ?1 AND (scope LIKE '%\\' || ?2 || '.jsonl' OR scope LIKE '%/' || ?2 || '.jsonl')")
        .map_err(err)?;
    let scope: Option<String> = stmt.query_row(params![agent, sid], |r| r.get(0)).ok();
    Ok(scope.and_then(|s| {
        let parts: Vec<&str> = s.split(['\\', '/']).collect();
        // …/projects/<folder>/<sid>.jsonl → 倒数第二段
        parts.iter().rev().nth(1).map(|f| f.to_string())
    }))
}

fn claude_code(conn: &Connection) -> Res<usize> {
    let agent = "claude-code";
    let roots: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT session_id FROM session WHERE agent_key = ?1 AND parent_id IS NULL")
            .map_err(err)?;
        let rows = stmt.query_map([agent], |r| r.get::<_, String>(0)).map_err(err)?;
        rows.flatten().collect()
    };
    let mut fixed = 0;
    for root in &roots {
        let ids = family(conn, agent, root)?;
        let mut keys: BTreeSet<String> = BTreeSet::new();
        for sid in &ids {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT project_key FROM turn_raw WHERE agent_key = ?1 AND session_id = ?2
                     UNION SELECT project_key FROM session WHERE agent_key = ?1 AND session_id = ?2",
                )
                .map_err(err)?;
            let rows = stmt.query_map(params![agent, sid], |r| r.get::<_, String>(0)).map_err(err)?;
            keys.extend(rows.flatten().filter(|k| k != UNKNOWN_PROJECT));
        }
        if keys.len() < 2 {
            continue;
        }
        let folder = cursor_folder(conn, agent, root)?;
        let chosen = folder
            .as_deref()
            .and_then(|f| keys.iter().find(|k| folder_matches(k, f)).cloned())
            .or_else(|| keys.iter().min_by_key(|k| (k.len(), (*k).clone())).cloned());
        if let Some(key) = chosen {
            if set_family_project(conn, agent, &ids, &key)? > 0 {
                fixed += 1;
            }
        }
    }
    Ok(fixed)
}

fn zcode(conn: &Connection) -> Res<usize> {
    let agent = "zcode";
    let parents: BTreeMap<String, Option<String>> = {
        let mut stmt = conn.prepare("SELECT session_id, parent_id FROM session WHERE agent_key = ?1").map_err(err)?;
        let rows = stmt.query_map([agent], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))).map_err(err)?;
        rows.flatten().collect()
    };
    let mut fixed = 0;
    for (sid, parent) in &parents {
        let Some(mut cur) = parent.clone().filter(|p| !p.is_empty()) else { continue };
        let mut seen = BTreeSet::new();
        while let Some(next) = parents.get(&cur).cloned().flatten().filter(|p| !p.is_empty() && *p != cur) {
            if !seen.insert(cur.clone()) {
                break;
            }
            cur = next;
        }
        let root_key: Option<String> = conn
            .query_row("SELECT project_key FROM session WHERE agent_key = ?1 AND session_id = ?2", params![agent, cur], |r| r.get(0))
            .ok();
        if let Some(key) = root_key.filter(|k| k != UNKNOWN_PROJECT) {
            if set_family_project(conn, agent, std::slice::from_ref(sid), &key)? > 0 {
                fixed += 1;
            }
        }
    }
    Ok(fixed)
}

fn codex(conn: &Connection) -> Res<usize> {
    conn.execute(
        "UPDATE session SET project_key = (
             SELECT r.project_key FROM turn_raw r WHERE r.agent_key = session.agent_key AND r.session_id = session.session_id
             ORDER BY r.started_at DESC, r.turn_seq DESC LIMIT 1)
         WHERE agent_key = 'codex' AND parent_id IS NULL
           AND EXISTS (SELECT 1 FROM turn_raw r WHERE r.agent_key = session.agent_key AND r.session_id = session.session_id
                       AND r.project_key <> session.project_key AND r.project_key <> ?1)
           AND (SELECT r.project_key FROM turn_raw r WHERE r.agent_key = session.agent_key AND r.session_id = session.session_id
                ORDER BY r.started_at DESC, r.turn_seq DESC LIMIT 1) <> session.project_key",
        [UNKNOWN_PROJECT],
    )
    .map_err(err)
}

/// v12 → v13 就地升级（调用方包在同一事务里）。
pub fn upgrade_v13(conn: &Connection) -> Res<V13Report> {
    let report = V13Report { claude_sessions: claude_code(conn)?, zcode_children: zcode(conn)?, codex_sessions: codex(conn)? };
    // 项目维按新键全表重算;阈值标记随后由采集线程按运行时阈值校验（不一致再算一次）
    super::task_store::recompute_all(conn, super::task_store::idle_threshold_ms())?;
    Ok(report)
}

#[derive(Debug, Default, PartialEq)]
pub struct V14Report {
    /// 改到真实键的会话数
    pub sessions_rekeyed: usize,
    /// 正的 `folder:` 映射数
    pub mappings_fixed: usize,
    /// 补种的 `folder:` 映射数
    pub mappings_seeded: usize,
}

const FOLDER_AGENTS: [&str; 2] = ["claude-code", "workbuddy"];
const FOLDER_SCOPE: &str = "folder:";

/// 该代理下所有会话的项目键（去重）。
fn session_keys(conn: &Connection, agent: &str) -> Res<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT project_key FROM session WHERE agent_key = ?1").map_err(err)?;
    let rows = stmt.query_map([agent], |r| r.get::<_, String>(0)).map_err(err)?;
    Ok(rows.flatten().collect())
}

/// 在真实路径键里找编码后与文件夹一致者;多个（不应发生）取最短。
fn real_key_for<'a>(keys: &'a [String], folder: &str) -> Option<&'a String> {
    keys.iter().filter(|k| looks_like_path(k) && folder_matches(k, folder)).min_by_key(|k| (k.len(), (*k).clone()))
}

fn upsert_cursor(conn: &Connection, agent: &str, scope: &str, value: &str, now: i64) -> Res<()> {
    conn.execute(
        "INSERT INTO source_cursor (source_id, scope, cursor_json, updated_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(source_id, scope) DO UPDATE SET cursor_json = excluded.cursor_json, updated_at = excluded.updated_at",
        params![agent, scope, value, now],
    )
    .map_err(err)?;
    Ok(())
}

/// 把文件游标 JSON 里的 `turn.project_key`（下次采集的提示值）从 bad 改成 good。
fn fix_cursor_hints(conn: &Connection, agent: &str, bad: &str, good: &str) -> Res<()> {
    let rows: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT scope, cursor_json FROM source_cursor WHERE source_id = ?1 AND scope NOT LIKE 'folder:%' AND cursor_json LIKE '%' || ?2 || '%'")
            .map_err(err)?;
        let it = stmt.query_map(params![agent, bad], |r| Ok((r.get(0)?, r.get(1)?))).map_err(err)?;
        it.flatten().collect()
    };
    for (scope, json) in rows {
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&json) else { continue };
        let Some(pk) = v.pointer_mut("/turn/project_key") else { continue };
        if pk.as_str() != Some(bad) {
            continue;
        }
        *pk = serde_json::Value::String(good.to_string());
        conn.execute("UPDATE source_cursor SET cursor_json = ?3 WHERE source_id = ?1 AND scope = ?2", params![agent, scope, v.to_string()])
            .map_err(err)?;
    }
    Ok(())
}

fn rekey_project(conn: &Connection, agent: &str, bad: &str, good: &str) -> Res<usize> {
    let sessions = conn
        .execute("UPDATE session SET project_key = ?3 WHERE agent_key = ?1 AND project_key = ?2", params![agent, bad, good])
        .map_err(err)?;
    for t in ["turn_raw", "turn"] {
        conn.execute(&format!("UPDATE {t} SET project_key = ?3 WHERE agent_key = ?1 AND project_key = ?2"), params![agent, bad, good])
            .map_err(err)?;
    }
    fix_cursor_hints(conn, agent, bad, good)?;
    // 用户在坏键上做过的别名 / 隐藏:真实键无记录则整行改键,否则丢弃坏键那份（真实键上的设置优先）
    let good_has: bool = conn
        .query_row("SELECT COUNT(*) FROM project_meta WHERE project_key = ?1", [good], |r| r.get::<_, i64>(0))
        .map(|n| n > 0)
        .unwrap_or(false);
    if good_has {
        conn.execute("DELETE FROM project_meta WHERE project_key = ?1", [bad]).map_err(err)?;
    } else {
        conn.execute("UPDATE project_meta SET project_key = ?2 WHERE project_key = ?1", [bad, good]).map_err(err)?;
    }
    Ok(sessions)
}

/// v13 → v14 就地升级（调用方包在同一事务里）。幂等。
pub fn upgrade_v14(conn: &Connection) -> Res<V14Report> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let mut report = V14Report::default();
    let mut touched = false;
    for agent in FOLDER_AGENTS {
        let keys = session_keys(conn, agent)?;
        //  坏键 → 真实键
        for bad in keys.iter().filter(|k| !looks_like_path(k) && k.as_str() != UNKNOWN_PROJECT) {
            let Some(good) = real_key_for(&keys, bad).cloned() else { continue };
            let n = rekey_project(conn, agent, bad, &good)?;
            report.sessions_rekeyed += n;
            touched |= n > 0;
            let scope = format!("{FOLDER_SCOPE}{bad}");
            let cur: Option<String> = conn
                .query_row("SELECT cursor_json FROM source_cursor WHERE source_id = ?1 AND scope = ?2", params![agent, scope], |r| r.get(0))
                .ok();
            if cur.as_deref() != Some(good.as_str()) {
                upsert_cursor(conn, agent, &scope, &good, now)?;
                report.mappings_fixed += 1;
            }
        }
        //  映射值本身不像路径（坏键会话已被源覆盖走、只剩映射的情况）
        let bad_maps: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT scope, cursor_json FROM source_cursor WHERE source_id = ?1 AND scope LIKE 'folder:%'")
                .map_err(err)?;
            let it = stmt.query_map([agent], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(err)?;
            it.flatten().filter(|(_, v)| !looks_like_path(v)).map(|(s, _)| s).collect()
        };
        let keys = session_keys(conn, agent)?;
        for scope in bad_maps {
            let folder = &scope[FOLDER_SCOPE.len()..];
            if let Some(good) = real_key_for(&keys, folder) {
                upsert_cursor(conn, agent, &scope, good, now)?;
                report.mappings_fixed += 1;
            }
        }
        //  补种:按文件游标 `…/<folder>/<sid>.jsonl` + 该会话的真实键
        let files: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT scope FROM source_cursor WHERE source_id = ?1 AND scope NOT LIKE 'folder:%' AND scope LIKE '%.jsonl'")
                .map_err(err)?;
            let it = stmt.query_map([agent], |r| r.get::<_, String>(0)).map_err(err)?;
            it.flatten().collect()
        };
        let mut seeded: BTreeSet<String> = BTreeSet::new();
        for scope in files {
            let parts: Vec<&str> = scope.split(['\\', '/']).collect();
            let (Some(file), Some(folder)) = (parts.iter().rev().next(), parts.iter().rev().nth(1)) else { continue };
            let sid = file.trim_end_matches(".jsonl");
            let map_scope = format!("{FOLDER_SCOPE}{folder}");
            if seeded.contains(&map_scope) {
                continue;
            }
            let existing: Option<String> = conn
                .query_row("SELECT cursor_json FROM source_cursor WHERE source_id = ?1 AND scope = ?2", params![agent, map_scope], |r| r.get(0))
                .ok();
            if existing.as_deref().is_some_and(looks_like_path) {
                seeded.insert(map_scope);
                continue;
            }
            let key: Option<String> = conn
                .query_row("SELECT project_key FROM session WHERE agent_key = ?1 AND session_id = ?2", params![agent, sid], |r| r.get(0))
                .ok();
            if let Some(k) = key.filter(|k| looks_like_path(k) && folder_matches(k, folder)) {
                upsert_cursor(conn, agent, &map_scope, &k, now)?;
                report.mappings_seeded += 1;
                seeded.insert(map_scope);
            }
        }
    }
    if touched {
        super::task_store::recompute_all(conn, super::task_store::idle_threshold_ms())?;
    }
    Ok(report)
}

#[derive(Debug, Default, PartialEq)]
pub struct V15Report {
    /// 改写 total 的 daily_usage 行数。
    pub daily_rows: usize,
    /// 改写 total 的 hourly_usage 行数。
    pub hourly_rows: usize,
    /// 回填 cache 并改写 total 的 turn_part 行数。
    pub part_rows: usize,
    /// 同步改写的「跨迁移仍开着的轮」游标数。
    pub cursors_synced: usize,
}

/// 缺列才补（`ALTER TABLE ... ADD COLUMN` 无 IF NOT EXISTS）。
fn add_column(conn: &Connection, table: &str, column: &str) -> Res<()> {
    let has: bool = {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).map_err(err)?;
        let it = stmt.query_map([], |r| r.get::<_, String>(1)).map_err(err)?;
        let found = it.flatten().any(|c| c == column);
        found
    };
    if !has {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"), []).map_err(err)?;
    }
    Ok(())
}

/// 把迁移后的 turn_part 值同步回游标里那份轮累加器（只动开着的轮;返回改写的游标数）。
fn sync_open_turn_cursors(conn: &Connection, agent: &str) -> Res<usize> {
    let rows: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT scope, cursor_json FROM source_cursor WHERE source_id = ?1 AND scope NOT LIKE 'folder:%'")
            .map_err(err)?;
        let it = stmt.query_map([agent], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(err)?;
        let v: Vec<(String, String)> = it.flatten().collect();
        v
    };
    let mut synced = 0usize;
    for (scope, json) in rows {
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&json) else { continue };
        let Some(sid) = v.pointer("/turn/session_id").and_then(|x| x.as_str()).map(str::to_string) else { continue };
        let Some(seq) = v.pointer("/turn/open/seq").and_then(|x| x.as_i64()) else { continue };
        let Some(parts) = v.pointer_mut("/turn/open/parts").and_then(|x| x.as_array_mut()) else { continue };
        let mut changed = false;
        for p in parts.iter_mut() {
            let (Some(day), Some(model)) = (
                p.get("day").and_then(|x| x.as_str()).map(str::to_string),
                p.get("model").and_then(|x| x.as_str()).map(str::to_string),
            ) else {
                continue;
            };
            let got: Option<(i64, i64, i64)> = conn
                .query_row(
                    "SELECT cache_read_tokens, cache_write_tokens, total_tokens FROM turn_part
                     WHERE agent_key = ?1 AND session_id = ?2 AND turn_seq = ?3 AND day = ?4 AND model_key = ?5",
                    params![agent, sid, seq, day, model],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .ok();
            let Some((cr, cw, total)) = got else { continue };
            let same = p.get("cache_read").and_then(|x| x.as_i64()) == Some(cr)
                && p.get("cache_write").and_then(|x| x.as_i64()) == Some(cw)
                && p.get("total").and_then(|x| x.as_i64()) == Some(total);
            if same {
                continue;
            }
            p["cache_read"] = cr.into();
            p["cache_write"] = cw.into();
            p["total"] = total.into();
            changed = true;
        }
        if changed {
            conn.execute(
                "UPDATE source_cursor SET cursor_json = ?3 WHERE source_id = ?1 AND scope = ?2",
                params![agent, scope, v.to_string()],
            )
            .map_err(err)?;
            synced += 1;
        }
    }
    Ok(synced)
}

/// v14 → v15 就地升级（调用方包在同一事务里）。幂等：第二次跑时 total 已等于四项和,无行可改。
pub fn upgrade_v15(conn: &Connection) -> Res<V15Report> {
    const AGENT: &str = "claude-code";
    for t in ["turn_raw", "turn_part"] {
        for c in ["cache_read_tokens", "cache_write_tokens"] {
            add_column(conn, t, c)?;
        }
    }
    let mut report = V15Report::default();

    //  有 cache 列的两张表:精确重算。
    for t in ["daily_usage", "hourly_usage"] {
        let n = conn
            .execute(
                &format!(
                    "UPDATE {t} SET total_tokens = input_tokens + output_tokens + cache_read_tokens + cache_write_tokens
                     WHERE agent_key = ?1
                       AND total_tokens <> input_tokens + output_tokens + cache_read_tokens + cache_write_tokens"
                ),
                [AGENT],
            )
            .map_err(err)?;
        if t == "daily_usage" {
            report.daily_rows = n;
        } else {
            report.hourly_rows = n;
        }
    }

    //  原始层:把日格的 cache 总量按 output 占比分摊进 turn_part（末位行吃余数,逐格 Σ 精确）。
    let cells: Vec<(String, String, i64, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT day, model_key, cache_read_tokens, cache_write_tokens FROM daily_usage
                 WHERE agent_key = ?1 AND cache_read_tokens + cache_write_tokens > 0",
            )
            .map_err(err)?;
        let it = stmt
            .query_map([AGENT], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))
            .map_err(err)?;
        it.flatten().collect()
    };
    for (day, model, cache_read, cache_write) in cells {
        let rows: Vec<(String, i64, i64)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT session_id, turn_seq, output_tokens FROM turn_part
                     WHERE agent_key = ?1 AND day = ?2 AND model_key = ?3
                       AND cache_read_tokens = 0 AND cache_write_tokens = 0
                     ORDER BY session_id, turn_seq",
                )
                .map_err(err)?;
            let it = stmt
                .query_map(params![AGENT, day, model], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
                })
                .map_err(err)?;
            it.flatten().collect()
        };
        if rows.is_empty() {
            continue;
        }
        // 权重取 output;整格 output 为 0（理论上不会,兜底）时按行均分。
        let weights: Vec<i64> = if rows.iter().all(|r| r.2 == 0) { vec![1; rows.len()] } else { rows.iter().map(|r| r.2).collect() };
        let sum: i128 = weights.iter().map(|w| *w as i128).sum();
        let (mut used_r, mut used_w) = (0i64, 0i64);
        for (i, ((sid, seq, _), w)) in rows.iter().zip(weights.iter()).enumerate() {
            let last = i + 1 == rows.len();
            let cr = if last { cache_read - used_r } else { ((cache_read as i128 * *w as i128) / sum) as i64 };
            let cw = if last { cache_write - used_w } else { ((cache_write as i128 * *w as i128) / sum) as i64 };
            used_r += cr;
            used_w += cw;
            conn.execute(
                "UPDATE turn_part SET cache_read_tokens = ?4, cache_write_tokens = ?5,
                     total_tokens = input_tokens + output_tokens + ?4 + ?5
                 WHERE agent_key = ?1 AND session_id = ?2 AND turn_seq = ?3 AND day = ?6 AND model_key = ?7",
                params![AGENT, sid, seq, cr, cw, day, model],
            )
            .map_err(err)?;
            report.part_rows += 1;
        }
    }

    // b 跨迁移仍开着的轮:累加器整份存在游标里,下一批 flush 会用它**整行覆盖** turn_part。
    // 若不同步,那一轮迁移前的部分会带着旧口径的 total（且 cache 两项按 serde default 为 0）被写回去,
    // daily_usage（只累加）与 turn_part / daily_project 就此失衡。
    report.cursors_synced = sync_open_turn_cursors(conn, AGENT)?;

    //  turn_raw 的 token 三列 = 其 turn_part 行之和（原始层内部自洽）。
    conn.execute(
        "UPDATE turn_raw SET
             cache_read_tokens = COALESCE((SELECT SUM(p.cache_read_tokens) FROM turn_part p
                 WHERE p.agent_key = turn_raw.agent_key AND p.session_id = turn_raw.session_id AND p.turn_seq = turn_raw.turn_seq), 0),
             cache_write_tokens = COALESCE((SELECT SUM(p.cache_write_tokens) FROM turn_part p
                 WHERE p.agent_key = turn_raw.agent_key AND p.session_id = turn_raw.session_id AND p.turn_seq = turn_raw.turn_seq), 0),
             total_tokens = COALESCE((SELECT SUM(p.total_tokens) FROM turn_part p
                 WHERE p.agent_key = turn_raw.agent_key AND p.session_id = turn_raw.session_id AND p.turn_seq = turn_raw.turn_seq), 0)
         WHERE agent_key = ?1",
        [AGENT],
    )
    .map_err(err)?;

    //  turn / daily_project 由原始层物化重算。
    super::task_store::recompute_all(conn, super::task_store::idle_threshold_ms())?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::store::{Store, RESET_SCHEMA};

    /// v14 形态的库（原始层无 cache 列、Claude total = input + output）→ v15:
    /// 补列、日 / 小时表精确重算、turn_part 按 output 占比分摊回填、turn_raw 与项目维守恒。
    #[test]
    fn v15_rebuilds_claude_total_from_cache_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(RESET_SCHEMA).unwrap();
        // 退回 v14 形态:原始层两表没有 cache 两列
        conn.execute_batch(
            "DROP TABLE turn_raw; DROP TABLE turn_part;
             CREATE TABLE turn_raw (agent_key TEXT NOT NULL, session_id TEXT NOT NULL, turn_seq INTEGER NOT NULL,
                 day TEXT NOT NULL, project_key TEXT NOT NULL, model_key TEXT NOT NULL, started_at INTEGER NOT NULL,
                 ended_at INTEGER NOT NULL, wall_ms INTEGER, model_ms INTEGER, tool_ms INTEGER, ttft_ms INTEGER, gap_ms INTEGER,
                 model_calls INTEGER NOT NULL DEFAULT 0, tool_calls INTEGER NOT NULL DEFAULT 0,
                 error_count INTEGER NOT NULL DEFAULT 0, retry_count INTEGER NOT NULL DEFAULT 0,
                 aborted INTEGER NOT NULL DEFAULT 0, input_tokens INTEGER NOT NULL DEFAULT 0,
                 output_tokens INTEGER NOT NULL DEFAULT 0, total_tokens INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (agent_key, session_id, turn_seq));
             CREATE TABLE turn_part (agent_key TEXT NOT NULL, session_id TEXT NOT NULL, turn_seq INTEGER NOT NULL,
                 day TEXT NOT NULL, model_key TEXT NOT NULL, input_tokens INTEGER NOT NULL DEFAULT 0,
                 output_tokens INTEGER NOT NULL DEFAULT 0, total_tokens INTEGER NOT NULL DEFAULT 0,
                 model_calls INTEGER NOT NULL DEFAULT 0, turn_mark INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (agent_key, session_id, turn_seq, day, model_key));",
        )
        .unwrap();
        // 旧口径:total = input + output;cache 只在日 / 小时表里
        conn.execute_batch(
            "INSERT INTO daily_usage (day, agent_key, model_key, input_tokens, output_tokens, total_tokens, request_count,
                                      cache_read_tokens, cache_write_tokens) VALUES
                ('2026-09-16','claude-code','opus',2,300,302,2,90000,1000),
                ('2026-09-16','codex','gpt',10,20,900,1,870,0);
             INSERT INTO hourly_usage (day, hour, agent_key, model_key, input_tokens, output_tokens, total_tokens,
                                       cache_read_tokens, cache_write_tokens) VALUES
                ('2026-09-16',9,'claude-code','opus',2,300,302,90000,1000);
             INSERT INTO session (agent_key, session_id, project_key, parent_id, started_at) VALUES
                ('claude-code','S','e:/W/Demo',NULL,1);
             INSERT INTO turn_raw (agent_key, session_id, turn_seq, day, project_key, model_key, started_at, ended_at,
                                   input_tokens, output_tokens, total_tokens, model_calls) VALUES
                ('claude-code','S',1,'2026-09-16','e:/W/Demo','opus',1,2,1,100,101,1),
                ('claude-code','S',2,'2026-09-16','e:/W/Demo','opus',3,4,1,200,201,1);
             INSERT INTO turn_part (agent_key, session_id, turn_seq, day, model_key, input_tokens, output_tokens,
                                    total_tokens, model_calls, turn_mark) VALUES
                ('claude-code','S',1,'2026-09-16','opus',1,100,101,1,1),
                ('claude-code','S',2,'2026-09-16','opus',1,200,201,1,1);",
        )
        .unwrap();

        // 跨迁移仍开着的轮:游标里那份累加器带旧口径 total、无 cache 两项
        conn.execute(
            "INSERT INTO source_cursor VALUES ('claude-code','C:/u/S.jsonl',?1,1)",
            [r#"{"offset":9,"size":9,"mtime":1,"turn":{"session_id":"S","open":{"seq":2,"started_at":3,
                "parts":[{"day":"2026-09-16","model":"opus","input":1,"output":200,"total":201,
                "model_calls":1,"turn_mark":1}]}}}"#],
        )
        .unwrap();

        let r = upgrade_v15(&conn).unwrap();
        assert_eq!((r.daily_rows, r.hourly_rows, r.part_rows, r.cursors_synced), (1, 1, 2, 1));

        // 游标里那份被同步成迁移后的值 → 下一批 flush 整行覆盖不会把 cache 抹掉
        let cur: String = conn
            .query_row("SELECT cursor_json FROM source_cursor WHERE scope LIKE '%S.jsonl'", [], |r| r.get(0))
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&cur).unwrap();
        let part = &v["turn"]["open"]["parts"][0];
        assert_eq!(
            (part["cache_read"].as_i64(), part["cache_write"].as_i64(), part["total"].as_i64()),
            (Some(60000), Some(667), Some(60868)),
            "游标与 turn_part 同值"
        );

        let claude: i64 = conn
            .query_row("SELECT total_tokens FROM daily_usage WHERE agent_key='claude-code'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(claude, 2 + 300 + 90000 + 1000, "日表精确重算");
        let hourly: i64 = conn.query_row("SELECT total_tokens FROM hourly_usage", [], |r| r.get(0)).unwrap();
        assert_eq!(hourly, 91302, "小时表与日表同口径");
        let codex: i64 =
            conn.query_row("SELECT total_tokens FROM daily_usage WHERE agent_key='codex'", [], |r| r.get(0)).unwrap();
        assert_eq!(codex, 900, "其余源的 provider total 不动");

        // 分摊按 output 占比 100:200,末位吃余数;逐格 Σ 与日表精确相等
        let parts: Vec<(i64, i64, i64)> = {
            let mut stmt = conn
                .prepare("SELECT cache_read_tokens, cache_write_tokens, total_tokens FROM turn_part ORDER BY turn_seq")
                .unwrap();
            let it = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
            it.flatten().collect()
        };
        assert_eq!(parts, vec![(30000, 333, 30434), (60000, 667, 60868)]);
        let part_sum: i64 = parts.iter().map(|p| p.2).sum();
        assert_eq!(part_sum, claude, "原始层逐格 Σ == daily_usage");
        let raw: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT total_tokens FROM turn_raw ORDER BY turn_seq").unwrap();
            let it = stmt.query_map([], |r| r.get(0)).unwrap();
            it.flatten().collect()
        };
        assert_eq!(raw, vec![30434, 60868], "turn_raw = 其 turn_part 之和");
        let dp: i64 = conn
            .query_row("SELECT COALESCE(SUM(total_tokens),0) FROM daily_project WHERE agent_key='claude-code'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(dp, claude, "项目维守恒");

        assert_eq!(upgrade_v15(&conn).unwrap(), V15Report::default(), "幂等");
    }

    fn seed(conn: &Connection) {
        conn.execute_batch(&format!(
            "{RESET_SCHEMA}
             INSERT INTO daily_usage (day, agent_key, model_key, total_tokens, request_count) VALUES ('2026-09-16','claude-code','m',10,2);
             -- Claude:根 S(轮 1 漂进子目录、轮 2 回根)+ 子代理 A(在子目录)+ 游标指明文件夹
             INSERT INTO session (agent_key, session_id, project_key, parent_id, started_at) VALUES
                ('claude-code','S','e:/W/Demo/src',NULL,1), ('claude-code','A','e:/W/Demo/src','S',2);
             INSERT INTO turn_raw (agent_key, session_id, turn_seq, day, project_key, model_key, started_at, ended_at, total_tokens) VALUES
                ('claude-code','S',1,'2026-09-16','e:/W/Demo/src','m',1,2,5), ('claude-code','S',2,'2026-09-16','e:/W/Demo','m',10,11,5),
                ('claude-code','A',1,'2026-09-16','e:/W/Demo/src','m',2,3,0);
             INSERT INTO turn_part (agent_key, session_id, turn_seq, day, model_key, total_tokens, turn_mark) VALUES
                ('claude-code','S',1,'2026-09-16','m',5,1), ('claude-code','S',2,'2026-09-16','m',5,1);
             INSERT INTO source_cursor VALUES ('claude-code','C:\\u\\.claude\\projects\\E--W-Demo\\S.jsonl','{{}}',1);
             -- Claude:无游标的旧会话 T,全在子目录 → 最短键
             INSERT INTO session (agent_key, session_id, project_key, parent_id, started_at) VALUES ('claude-code','T','e:/A/b/c',NULL,1);
             INSERT INTO turn_raw (agent_key, session_id, turn_seq, day, project_key, model_key, started_at, ended_at) VALUES
                ('claude-code','T',1,'2026-09-16','e:/A/b/c','m',1,2), ('claude-code','T',2,'2026-09-16','e:/A/b','m',2,3);
             -- ZCode:子会话 C 在子目录
             INSERT INTO session (agent_key, session_id, project_key, parent_id, started_at) VALUES
                ('zcode','R','d:/legado',NULL,1), ('zcode','C','d:/legado/tools','R',2);
             INSERT INTO turn_raw (agent_key, session_id, turn_seq, day, project_key, model_key, started_at, ended_at) VALUES
                ('zcode','C',1,'2026-09-16','d:/legado/tools','m',2,3);
             -- Codex:会话行记首轮目录,最后一轮换了工作区
             INSERT INTO session (agent_key, session_id, project_key, parent_id, started_at) VALUES ('codex','X','c:/bang',NULL,1);
             INSERT INTO turn_raw (agent_key, session_id, turn_seq, day, project_key, model_key, started_at, ended_at) VALUES
                ('codex','X',1,'2026-09-16','c:/bang','m',1,2), ('codex','X',2,'2026-09-16','e:/spring','m',5,6);
             CREATE TABLE IF NOT EXISTS project_meta (project_key TEXT PRIMARY KEY, alias TEXT, hidden INTEGER NOT NULL DEFAULT 0,
                 merged_into TEXT, note TEXT, updated_at INTEGER NOT NULL);
             INSERT INTO project_meta (project_key, alias, updated_at) VALUES ('e:/W/Demo','Alias',1);
             PRAGMA user_version = 12;"
        ))
        .unwrap();
    }

    fn keys(conn: &Connection, table: &str, agent: &str, sid: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("SELECT DISTINCT project_key FROM {table} WHERE agent_key = ?1 AND session_id = ?2 ORDER BY 1"))
            .unwrap();
        stmt.query_map([agent, sid], |r| r.get(0)).unwrap().flatten().collect()
    }

    #[test]
    fn v13_upgrades_rows_in_place() {
        let conn = Connection::open_in_memory().unwrap();
        seed(&conn);
        let r = upgrade_v13(&conn).unwrap();
        assert_eq!(r, V13Report { claude_sessions: 2, zcode_children: 1, codex_sessions: 1 });
        for (t, sid) in [("session", "S"), ("turn_raw", "S"), ("session", "A"), ("turn_raw", "A")] {
            assert_eq!(keys(&conn, t, "claude-code", sid), vec!["e:/W/Demo".to_string()], "{t}/{sid} 按文件夹归根");
        }
        assert_eq!(keys(&conn, "session", "claude-code", "T"), vec!["e:/A/b".to_string()], "无游标取最短键");
        assert_eq!(keys(&conn, "session", "zcode", "C"), vec!["d:/legado".to_string()]);
        assert_eq!(keys(&conn, "turn_raw", "zcode", "C"), vec!["d:/legado".to_string()]);
        assert_eq!(keys(&conn, "session", "codex", "X"), vec!["e:/spring".to_string()], "Codex 会话行 = 最后一轮");
        assert_eq!(keys(&conn, "turn_raw", "codex", "X"), vec!["c:/bang".to_string(), "e:/spring".to_string()], "Codex 轮不动");
        let dp: Vec<(String, i64)> = {
            let mut s = conn.prepare("SELECT project_key, SUM(total_tokens) FROM daily_project WHERE agent_key='claude-code' AND total_tokens > 0 GROUP BY 1").unwrap();
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().flatten().collect()
        };
        assert_eq!(dp, vec![("e:/W/Demo".to_string(), 10)], "daily_project 按新键重算");
        assert!(upgrade_v13(&conn).unwrap() == V13Report::default(), "幂等");
    }

    #[test]
    fn v14_rekeys_folder_name_projects_and_seeds_mappings() {
        let conn = Connection::open_in_memory().unwrap();
        seed(&conn);
        upgrade_v13(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO session (agent_key, session_id, project_key, parent_id, started_at) VALUES ('claude-code','B','E--W-Demo',NULL,20);
             INSERT INTO turn_raw (agent_key, session_id, turn_seq, day, project_key, model_key, started_at, ended_at, total_tokens) VALUES
                ('claude-code','B',1,'2026-09-17','E--W-Demo','m',20,21,7);
             INSERT INTO turn (agent_key, session_id, turn_seq, day, project_key, model_key, started_at, ended_at, total_tokens) VALUES
                ('claude-code','B',1,'2026-09-17','E--W-Demo','m',20,21,7);
             INSERT INTO source_cursor VALUES ('claude-code','folder:E--W-Demo','E--W-Demo',1);
             INSERT INTO source_cursor VALUES ('claude-code','C:\\\\u\\\\.claude\\\\projects\\\\E--W-Demo\\\\B.jsonl','{\"offset\":1,\"turn\":{\"session_id\":\"B\",\"project_key\":\"E--W-Demo\"}}',1);
             INSERT INTO project_meta (project_key, alias, updated_at) VALUES ('E--W-Demo','Bad',1);
             -- 无真实键可对应的坏键:原样保留
             INSERT INTO session (agent_key, session_id, project_key, parent_id, started_at) VALUES ('claude-code','N','D--Nowhere',NULL,30);",
        )
        .unwrap();
        let r = upgrade_v14(&conn).unwrap();
        assert_eq!(r, V14Report { sessions_rekeyed: 1, mappings_fixed: 1, mappings_seeded: 0 }, "映射已由 ① 修正,③ 无需补种");
        for t in ["session", "turn_raw", "turn"] {
            assert_eq!(keys(&conn, t, "claude-code", "B"), vec!["e:/W/Demo".to_string()], "{t}");
        }
        assert_eq!(keys(&conn, "session", "claude-code", "N"), vec!["D--Nowhere".to_string()]);
        let map: String = conn.query_row("SELECT cursor_json FROM source_cursor WHERE scope = 'folder:E--W-Demo'", [], |r| r.get(0)).unwrap();
        assert_eq!(map, "e:/W/Demo");
        let hint: String = conn.query_row("SELECT cursor_json FROM source_cursor WHERE scope LIKE '%B.jsonl'", [], |r| r.get(0)).unwrap();
        assert!(hint.contains("\"project_key\":\"e:/W/Demo\"") && hint.contains("\"offset\":1"), "{hint}");
        let metas: Vec<(String, String)> = {
            let mut s = conn.prepare("SELECT project_key, alias FROM project_meta ORDER BY 1").unwrap();
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().flatten().collect()
        };
        assert_eq!(metas, vec![("e:/W/Demo".to_string(), "Alias".to_string())], "真实键已有设置 → 坏键那份丢弃");
        let dp: i64 = conn.query_row("SELECT COALESCE(SUM(total_tokens),0) FROM daily_project WHERE project_key = 'E--W-Demo'", [], |r| r.get(0)).unwrap();
        assert_eq!(dp, 0, "daily_project 重算后无坏键");
        assert_eq!(upgrade_v14(&conn).unwrap(), V14Report::default(), "幂等");
    }

    #[test]
    fn v14_seeds_mapping_from_file_cursor_when_missing() {
        let conn = Connection::open_in_memory().unwrap();
        seed(&conn);
        upgrade_v13(&conn).unwrap();
        let r = upgrade_v14(&conn).unwrap();
        assert_eq!(r, V14Report { sessions_rekeyed: 0, mappings_fixed: 0, mappings_seeded: 1 });
        let map: String = conn.query_row("SELECT cursor_json FROM source_cursor WHERE scope = 'folder:E--W-Demo'", [], |r| r.get(0)).unwrap();
        assert_eq!(map, "e:/W/Demo");
    }

    /// 手动:对一份真实库副本跑迁移（`TC_MIGRATE_DB=<路径>`,只动该副本;`cargo test migrate_real_db_copy -- --ignored --nocapture`）。
    #[test]
    #[ignore]
    fn migrate_real_db_copy() {
        let Some(p) = std::env::var_os("TC_MIGRATE_DB") else { return };
        let path = std::path::PathBuf::from(p);
        let before: i64 = Connection::open(&path).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        let t = std::time::Instant::now();
        let store = Store::open(&path).unwrap();
        let after: i64 = store.conn().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        println!("migrated {} v{before} -> v{after} in {:?}", path.display(), t.elapsed());
    }

    /// 文件库:打开即备份 + 就地升级,历史行与常驻表都在。
    #[test]
    fn open_backs_up_then_upgrades_without_dropping() {
        let dir = std::env::temp_dir().join(format!("tc_v13_inplace_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("collector.db");
        {
            let conn = Connection::open(&db).unwrap();
            seed(&conn);
        }
        {
            let store = Store::open(&db).unwrap();
            let v: i64 = store.conn().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(v, crate::collector::store::SCHEMA_VERSION);
            let n: i64 = store.conn().query_row("SELECT COUNT(*) FROM daily_usage", [], |r| r.get(0)).unwrap();
            assert_eq!(n, 1, "历史聚合行保留");
            assert!(store.get_cursor("claude-code", r"C:\u\.claude\projects\E--W-Demo\S.jsonl").is_some(), "游标保留,不重扫");
            assert_eq!(keys(store.conn(), "session", "claude-code", "S"), vec!["e:/W/Demo".to_string()]);
            let alias: String = store.conn().query_row("SELECT alias FROM project_meta WHERE project_key = 'e:/W/Demo'", [], |r| r.get(0)).unwrap();
            assert_eq!(alias, "Alias");
        }
        let backups: Vec<_> = std::fs::read_dir(dir.join("backups")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert!(backups[0].starts_with("collector-v12-") && backups[0].ends_with(".db"));
        let b = Connection::open(dir.join("backups").join(&backups[0])).unwrap();
        let v: i64 = b.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 12, "备份是迁移前的库");
        assert_eq!(keys(&b, "session", "claude-code", "S"), vec!["e:/W/Demo/src".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
