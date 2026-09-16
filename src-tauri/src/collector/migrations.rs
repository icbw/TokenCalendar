//! 就地迁移（起）：口径变更**只升级库内已有行**,不清库、不依赖源日志仍在。
//!
//! 原则：collector.db 是用量历史唯一的持久副本——源日志会轮换 / 被删
//! （Claude Code 默认清理旧转录、用户删目录）,「清库重扫」会把源里已经没有的历史一起丢掉。
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
//! v14 = 正 v13 解析器漏洞留下的「文件夹名当项目键」（looks_like_path`）:
//! - 凡 Claude Code / WorkBuddy 会话的键不像路径（无盘符、无分隔符）,且库内存在编码后与之一致的真实路径键 →
//!   会话 / 轮 / 原始轮 / 文件游标提示 / `project_meta` 一并改到真实键;`folder:` 映射写成真实键。
//! - 顺带按文件游标为每个文件夹补种 `folder:` 映射,后续采集不再从零解析（从零解析正是踩坑的入口）。
//! - 找不到真实键的（源里从未出现过 cwd 行）原样保留。

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::store::{Store, RESET_SCHEMA};

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
