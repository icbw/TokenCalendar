//! 项目管理:把「采集到的工作目录」与「用户认可的可分析项目」分开。
//!
//! 数据:`project_meta`（常驻表,建在 `Store:init` 的常驻批里,**不在 RESET_TABLES**,
//! 跨清库保留）。`session` / `turn` / `daily_project` 的原始 project_key 一律不改;改映射不重扫。
//!
//! 解析层（所有项目维查询统一经 `resolve_cte`,产出 `pmap（raw_key, eff_key)` = 可见原始键 → 有效键）:
//! 1. **自身归属** `se`:有 meta 行 → 键本身（显式管理过的键不受自动规则影响）;否则 `unknown` 且
//!    「Unknown 视为 Scratch」开 → `__scratch`;否则规则开且根会话数 < min_sessions 且总轮数 < min_turns
//!    → `__scratch`;否则键本身。统计口径 = 物化层 `turn`（只含根会话）:会话数 = distinct 会话,轮数 = 行数。
//! 2. **合并**:`merged_into` 非空 → 有效键 = 目标的自身归属（目标只允许一层,见 `merge_projects` 校验）。
//! 3. **隐藏**:自身 `hidden = 1`,或有效键（合并目标 / `__scratch` 伪项目）的 meta 行 `hidden = 1` → 不进 pmap。
//! 4. **显示名**:`coalesce（alias, 末段)`;`__scratch` → alias 或 "Scratch";`unknown` → "Unknown project"。
//!
//! 规则参数（开关 / 两个阈值 / Unknown 开关）是 Rust 校验过的整数,以字面量拼进 CTE（无字符串拼接面）。
//! 运行时值 = designPrefs `scratch*` 四键（启动载入、`set_scratch_rule` 下发）,同离开阈值模式;
//! 规则只作用于查询时的解析,改规则不重算任何表。
//!
//! 行存在性语义（显式管理）:
//! - `set_project_meta` 总是保留行（Keep / Rename / Hide / Unhide 都让该键脱离自动规则）;`reset` 删行回到自动态
//!   （仍是他人合并目标时只清 alias / note / hidden,保留行以免成员被连带卷进 Scratch）。
//! - `merge_projects` 为目标补建空行（目标即显式项目）;`unmerge_projects` 清 merged_into 后删掉变空的行。
//!
//! 守恒:隐藏与合并只作用于项目维（group_by / 切片 / 筛选 = project）;agent / model / total 维的项目维查询
//! 与 daily_usage 的一切读入口不经解析层,数字不变。

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use super::store::{agent_label, now_millis, Store};
use super::turns::UNKNOWN_PROJECT;

/// 内置伪项目键（自动折叠目标;可隐藏 / 改名,不可作合并源或目标）。
pub const SCRATCH_KEY: &str = "__scratch";
pub const SCRATCH_LABEL: &str = "Scratch";
pub const UNKNOWN_LABEL: &str = "Unknown project";
/// agent / model 行钻取按项目切片时,隐藏项目的合计归入此切片（切片和仍等于格值）。
pub const HIDDEN_SLICE_KEY: &str = "__hidden";
pub const HIDDEN_SLICE_LABEL: &str = "Hidden projects";

/// alias / note 长度上限（字符数）。
pub const META_TEXT_MAX: usize = 120;
/// project_key 长度上限（字符数,防御性）。
const KEY_MAX: usize = 1024;
/// 单次合并 / 取消合并的键数上限。
pub const BATCH_KEYS_MAX: usize = 500;

pub const SCRATCH_MIN_SESSIONS_BOUNDS: (u32, u32) = (1, 50);
pub const SCRATCH_MIN_TURNS_BOUNDS: (u32, u32) = (1, 500);

/// prefs.json 键名（前端 designPrefs.sanitize 同域）。
pub const PREFS_ENABLED: &str = "scratchRuleEnabled";
pub const PREFS_MIN_SESSIONS: &str = "scratchMinSessions";
pub const PREFS_MIN_TURNS: &str = "scratchMinTurns";
pub const PREFS_UNKNOWN: &str = "scratchUnknown";

/// 自动折叠规则。命令契约 snake_case。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScratchRule {
    pub enabled: bool,
    /// 根会话数 < 此值…
    pub min_sessions: u32,
    /// …且总轮数 < 此值 → 折叠进 Scratch。
    pub min_turns: u32,
    /// `unknown`（无目录源）归 Scratch。
    pub unknown_as_scratch: bool,
}

impl ScratchRule {
    pub const DEFAULT: ScratchRule = ScratchRule { enabled: true, min_sessions: 2, min_turns: 5, unknown_as_scratch: true };
    /// 规则全关（解析层只剩 alias / hidden / merge;测试与「规则关」等价）。
    pub const OFF: ScratchRule = ScratchRule { enabled: false, min_sessions: 2, min_turns: 5, unknown_as_scratch: false };

    /// 阈值越界 → Err（命令层入口校验;prefs 载入走 `from_prefs` 的逐键回落）。
    pub fn validate(&self) -> Result<(), String> {
        let (s0, s1) = SCRATCH_MIN_SESSIONS_BOUNDS;
        let (t0, t1) = SCRATCH_MIN_TURNS_BOUNDS;
        if !(s0..=s1).contains(&self.min_sessions) {
            return Err(format!("min_sessions out of range ({s0}..={s1})"));
        }
        if !(t0..=t1).contains(&self.min_turns) {
            return Err(format!("min_turns out of range ({t0}..={t1})"));
        }
        Ok(())
    }

    /// prefs.json 原文 → 规则（逐键:缺失 / 类型错 / 越界 → 该键默认值;原文损坏 → 全默认）。
    pub fn from_prefs(json: &str) -> ScratchRule {
        let d = ScratchRule::DEFAULT;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return d };
        let flag = |k: &str, def: bool| v.get(k).and_then(|x| x.as_bool()).unwrap_or(def);
        let num = |k: &str, def: u32, (lo, hi): (u32, u32)| {
            v.get(k).and_then(|x| x.as_u64()).and_then(|n| u32::try_from(n).ok()).filter(|n| (lo..=hi).contains(n)).unwrap_or(def)
        };
        ScratchRule {
            enabled: flag(PREFS_ENABLED, d.enabled),
            min_sessions: num(PREFS_MIN_SESSIONS, d.min_sessions, SCRATCH_MIN_SESSIONS_BOUNDS),
            min_turns: num(PREFS_MIN_TURNS, d.min_turns, SCRATCH_MIN_TURNS_BOUNDS),
            unknown_as_scratch: flag(PREFS_UNKNOWN, d.unknown_as_scratch),
        }
    }

    /// 把规则合并进 prefs.json 原文（其余键原样;原文缺失 / 损坏 → 以空对象起步）。
    pub fn merge_into_prefs(&self, json: Option<&str>) -> String {
        let mut v = json
            .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
            .filter(|v| v.is_object())
            .unwrap_or_else(|| serde_json::json!({}));
        v[PREFS_ENABLED] = serde_json::json!(self.enabled);
        v[PREFS_MIN_SESSIONS] = serde_json::json!(self.min_sessions);
        v[PREFS_MIN_TURNS] = serde_json::json!(self.min_turns);
        v[PREFS_UNKNOWN] = serde_json::json!(self.unknown_as_scratch);
        v.to_string()
    }
}

static RULE: Mutex<ScratchRule> = Mutex::new(ScratchRule::DEFAULT);

/// 当前运行时规则。
pub fn scratch_rule() -> ScratchRule {
    RULE.lock().map(|r| *r).unwrap_or(ScratchRule::DEFAULT)
}

/// 下发运行时规则（调用方先 `validate`）。
pub fn set_scratch_rule(rule: ScratchRule) {
    if let Ok(mut r) = RULE.lock() {
        *r = rule;
    }
}

/// 解析层公共 CTE 片段（不含 `WITH`;调用方 `format!（"WITH {cte} SELECT …")`,可再接自己的 CTE）。
/// 产出:`pm_stat（k, s, t)` 物化层统计、`pm_self（k, mi, h, s, t, se)` 自身归属、
/// `pm_res（raw_key, eff_key, own_hidden)` 含合并的有效键、`pmap（raw_key, eff_key)` 可见映射。
pub fn resolve_cte(rule: &ScratchRule) -> String {
    let on = rule.enabled as i32;
    let unknown = rule.unknown_as_scratch as i32;
    let (ms, mt) = (rule.min_sessions, rule.min_turns);
    format!(
        "pm_stat AS (
            SELECT project_key AS k, COUNT(DISTINCT agent_key || char(31) || session_id) AS s, COUNT(*) AS t
            FROM turn GROUP BY project_key),
         pm_key AS (
            SELECT project_key AS k FROM daily_project UNION SELECT project_key FROM turn
            UNION SELECT project_key FROM session UNION SELECT project_key FROM project_meta),
         pm_self AS (
            SELECT pk.k AS k, m.merged_into AS mi, COALESCE(m.hidden, 0) AS h, COALESCE(st.s, 0) AS s, COALESCE(st.t, 0) AS t,
                   CASE WHEN m.project_key IS NOT NULL OR pk.k = '{SCRATCH_KEY}' THEN pk.k
                        WHEN pk.k = '{UNKNOWN_PROJECT}' THEN CASE WHEN {unknown} = 1 THEN '{SCRATCH_KEY}' ELSE pk.k END
                        WHEN {on} = 1 AND COALESCE(st.s, 0) < {ms} AND COALESCE(st.t, 0) < {mt} THEN '{SCRATCH_KEY}'
                        ELSE pk.k END AS se
            FROM pm_key pk LEFT JOIN project_meta m ON m.project_key = pk.k LEFT JOIN pm_stat st ON st.k = pk.k),
         pm_res AS (
            SELECT a.k AS raw_key, CASE WHEN a.mi IS NULL THEN a.se ELSE COALESCE(b.se, a.mi) END AS eff_key, a.h AS own_hidden
            FROM pm_self a LEFT JOIN pm_self b ON b.k = a.mi),
         pmap AS (
            SELECT r.raw_key AS raw_key, r.eff_key AS eff_key
            FROM pm_res r LEFT JOIN project_meta e ON e.project_key = r.eff_key
            WHERE r.own_hidden = 0 AND COALESCE(e.hidden, 0) = 0)"
    )
}

/// 无别名时的展示名:路径末段;`unknown` / `__scratch` 用固定英文名。
pub fn project_short_label(key: &str) -> String {
    match key {
        UNKNOWN_PROJECT => UNKNOWN_LABEL.to_string(),
        SCRATCH_KEY => SCRATCH_LABEL.to_string(),
        HIDDEN_SLICE_KEY => HIDDEN_SLICE_LABEL.to_string(),
        _ => key.rsplit('/').find(|s| !s.is_empty()).unwrap_or(key).to_string(),
    }
}

/// 一组项目键的展示名:alias 优先;无 alias 的键末段在本组内重名时退回完整路径。
pub fn project_labels(keys: &[String], aliases: &HashMap<String, String>) -> Vec<String> {
    let short: Vec<Option<String>> = keys.iter().map(|k| if aliases.contains_key(k) { None } else { Some(project_short_label(k)) }).collect();
    keys.iter()
        .zip(&short)
        .map(|(k, s)| match s {
            None => aliases[k].clone(),
            Some(s) if short.iter().filter(|x| x.as_ref() == Some(s)).count() > 1 && k.contains('/') => k.clone(),
            Some(s) => s.clone(),
        })
        .collect()
}

// ---------- 命令契约 ----------

#[derive(Debug, Clone, Serialize)]
pub struct ProjectMetaRow {
    pub key: String,
    /// 展示名（alias 或末段;全表内重名退回完整路径）。
    pub label: String,
    pub alias: Option<String>,
    pub hidden: bool,
    pub merged_into: Option<String>,
    /// 合并目标的展示名（未合并 = null）。
    pub merged_label: Option<String>,
    pub note: Option<String>,
    /// active | hidden | merged | scratch（优先级:自身隐藏 > 合并 > 规则折叠 > 活跃）。
    pub status: String,
    /// 是否有 meta 行（显式管理过;false = 自动态,规则可作用）。
    pub managed: bool,
    /// 分析视图里的有效键（不可见 = null:自身隐藏,或合并目标 / Scratch 被隐藏）。
    pub effective_key: Option<String>,
    /// Agent 展示名（去重,按 key 排序）。
    pub agents: Vec<String>,
    /// 根会话数 / 物化轮数（规则判据同源）。
    pub sessions: i64,
    pub turns: i64,
    pub tokens: i64,
    pub first_day: Option<String>,
    pub last_day: Option<String>,
    pub updated_at: Option<i64>,
    /// 目录在本机存在（命令层填充,存储层恒 false）。
    pub folder_exists: bool,
}

#[derive(Debug, Serialize)]
pub struct ProjectMetaList {
    pub rows: Vec<ProjectMetaRow>,
    /// Scratch 伪项目本身被隐藏。
    pub scratch_hidden: bool,
    pub scratch_alias: Option<String>,
}

/// set_project_meta 入参（单条 upsert;merged_into 只经 merge / unmerge 改）。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ProjectMetaInput {
    pub project_key: String,
    /// 空串 / 全空白 = 清除。
    pub alias: Option<String>,
    pub hidden: bool,
    pub note: Option<String>,
    /// true = 删行回到自动态（忽略其余字段）。
    pub reset: bool,
}

fn clean_text(field: &str, v: Option<&str>) -> Result<Option<String>, String> {
    let Some(t) = v.map(str::trim).filter(|t| !t.is_empty()) else { return Ok(None) };
    if t.chars().count() > META_TEXT_MAX {
        return Err(format!("{field} is longer than {META_TEXT_MAX} characters"));
    }
    if t.chars().any(char::is_control) {
        return Err(format!("{field} contains control characters"));
    }
    Ok(Some(t.to_string()))
}

fn clean_key(key: &str) -> Result<String, String> {
    let k = key.trim();
    if k.is_empty() {
        return Err("project key is empty".into());
    }
    if k.chars().count() > KEY_MAX || k.chars().any(char::is_control) {
        return Err("invalid project key".into());
    }
    Ok(k.to_string())
}

fn clean_keys(keys: &[String]) -> Result<Vec<String>, String> {
    if keys.is_empty() {
        return Err("no projects selected".into());
    }
    if keys.len() > BATCH_KEYS_MAX {
        return Err(format!("too many projects (max {BATCH_KEYS_MAX})"));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for k in keys {
        let k = clean_key(k)?;
        if seen.insert(k.clone()) {
            out.push(k);
        }
    }
    Ok(out)
}

fn err(e: rusqlite::Error) -> String {
    e.to_string()
}

impl Store {
    /// project_meta 中非空 alias 的键 → 别名。
    pub fn project_aliases(&self) -> HashMap<String, String> {
        let Ok(mut stmt) = self.conn().prepare("SELECT project_key, alias FROM project_meta WHERE alias IS NOT NULL AND alias <> ''") else {
            return HashMap::new();
        };
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// 管理面板列表:每个目录键一行（数据中出现过的键 ∪ meta 行,不含 Scratch 伪项目）,按最近活动降序。
    pub fn list_project_meta(&self, rule: &ScratchRule) -> Result<ProjectMetaList, String> {
        let cte = resolve_cte(rule);
        let sql = format!(
            "WITH {cte},
             dp AS (SELECT project_key AS k, SUM(total_tokens) AS tok, MIN(day) AS d0, MAX(day) AS d1 FROM daily_project GROUP BY project_key),
             ag AS (SELECT k, group_concat(a, ',') AS agents FROM (
                        SELECT project_key AS k, agent_key AS a FROM daily_project UNION SELECT project_key, agent_key FROM turn
                        ORDER BY 1, 2) GROUP BY k)
             SELECT s.k, m.alias, s.h, s.mi, m.note, m.updated_at, s.se, m.project_key IS NOT NULL,
                    (SELECT p.eff_key FROM pmap p WHERE p.raw_key = s.k), s.s, s.t, COALESCE(dp.tok, 0), dp.d0, dp.d1, ag.agents
             FROM pm_self s LEFT JOIN project_meta m ON m.project_key = s.k
                  LEFT JOIN dp ON dp.k = s.k LEFT JOIN ag ON ag.k = s.k
             WHERE s.k <> '{SCRATCH_KEY}'
             ORDER BY dp.d1 IS NULL, dp.d1 DESC, s.s DESC, s.k"
        );
        let mut stmt = self.conn().prepare(&sql).map_err(err)?;
        let rows = stmt
            .query_map([], |r| {
                let hidden = r.get::<_, i64>(2)? != 0;
                let merged_into: Option<String> = r.get(3)?;
                let se: String = r.get(6)?;
                let status = if hidden {
                    "hidden"
                } else if merged_into.as_deref() == Some(SCRATCH_KEY) {
                    // 手动归入 Scratch（合并目标 = Scratch 伪项目）与规则折叠同一状态
                    "scratch"
                } else if merged_into.is_some() {
                    "merged"
                } else if se == SCRATCH_KEY {
                    "scratch"
                } else {
                    "active"
                };
                Ok(ProjectMetaRow {
                    key: r.get(0)?,
                    label: String::new(),
                    alias: r.get(1)?,
                    hidden,
                    merged_into,
                    merged_label: None,
                    note: r.get(4)?,
                    status: status.to_string(),
                    managed: r.get(7)?,
                    effective_key: r.get(8)?,
                    agents: r.get::<_, Option<String>>(14)?.map(|a| a.split(',').map(agent_label).collect()).unwrap_or_default(),
                    sessions: r.get(9)?,
                    turns: r.get(10)?,
                    tokens: r.get(11)?,
                    first_day: r.get(12)?,
                    last_day: r.get(13)?,
                    updated_at: r.get(5)?,
                    folder_exists: false,
                })
            })
            .map_err(err)?;
        let mut rows: Vec<ProjectMetaRow> = rows.collect::<Result<_, _>>().map_err(err)?;
        drop(stmt);
        let aliases = self.project_aliases();
        let keys: Vec<String> = rows.iter().map(|r| r.key.clone()).collect();
        let labels: HashMap<String, String> = keys.iter().cloned().zip(project_labels(&keys, &aliases)).collect();
        for r in &mut rows {
            r.label = labels[&r.key].clone();
            r.merged_label = r.merged_into.as_ref().map(|t| {
                labels.get(t).cloned().or_else(|| aliases.get(t).cloned()).unwrap_or_else(|| project_short_label(t))
            });
        }
        let scratch: Option<(i64, Option<String>)> = self
            .conn()
            .query_row("SELECT hidden, alias FROM project_meta WHERE project_key = ?1", [SCRATCH_KEY], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(err)?;
        Ok(ProjectMetaList {
            rows,
            scratch_hidden: scratch.as_ref().is_some_and(|(h, _)| *h != 0),
            scratch_alias: scratch.and_then(|(_, a)| a),
        })
    }

    /// 键是否出现在数据或 meta 中（Open folder 等只对已知键开放）。
    pub fn project_key_known(&self, key: &str) -> bool {
        self.conn()
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM daily_project WHERE project_key = ?1)
                     OR EXISTS (SELECT 1 FROM turn WHERE project_key = ?1)
                     OR EXISTS (SELECT 1 FROM session WHERE project_key = ?1)
                     OR EXISTS (SELECT 1 FROM project_meta WHERE project_key = ?1)",
                [key],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false)
    }

    /// 单条 upsert（alias / hidden / note;merged_into 不动）或 reset。
    pub fn set_project_meta(&mut self, input: &ProjectMetaInput) -> Result<(), String> {
        let key = clean_key(&input.project_key)?;
        let now = now_millis();
        let tx = self.conn_mut().transaction_with_behavior(TransactionBehavior::Immediate).map_err(err)?;
        if input.reset {
            let is_target: bool = tx
                .query_row("SELECT EXISTS (SELECT 1 FROM project_meta WHERE merged_into = ?1)", [&key], |r| r.get(0))
                .map_err(err)?;
            if is_target {
                tx.execute(
                    "UPDATE project_meta SET alias = NULL, note = NULL, hidden = 0, updated_at = ?2 WHERE project_key = ?1",
                    params![key, now],
                )
                .map_err(err)?;
            } else {
                tx.execute("DELETE FROM project_meta WHERE project_key = ?1", [&key]).map_err(err)?;
            }
        } else {
            let alias = clean_text("alias", input.alias.as_deref())?;
            let note = clean_text("note", input.note.as_deref())?;
            tx.execute(
                "INSERT INTO project_meta (project_key, alias, hidden, merged_into, note, updated_at)
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5)
                 ON CONFLICT (project_key) DO UPDATE SET alias = excluded.alias, hidden = excluded.hidden,
                     note = excluded.note, updated_at = excluded.updated_at",
                params![key, alias, input.hidden as i64, note, now],
            )
            .map_err(err)?;
        }
        tx.commit().map_err(err)
    }

    /// 把 `keys` 合并进 `into`（一层）。拒绝:并入自身（环）、目标本身已合并、源是他人的合并目标、Scratch 伪项目作源。
    /// 目标补建空 meta 行（显式项目,不受自动规则影响）。返回合并的键数。
    /// **手动归入 Scratch**（管理列表点状态徽章在 Active / Scratch 间切换）= 以 `SCRATCH_KEY` 为目标
    /// 合并;解析层不需改动（pm_res 取 Scratch 自身归属 = `__scratch`,Scratch 隐藏时连带不可）,列表状态报 `scratch`。
    /// 只新增一种 merged_into 取值,无 schema 变化、不动既有行。
    pub fn merge_projects(&mut self, keys: &[String], into: &str) -> Result<usize, String> {
        let into = clean_key(into)?;
        let keys = clean_keys(keys)?;
        if keys.iter().any(|k| k == SCRATCH_KEY) {
            return Err("Scratch cannot be merged".into());
        }
        if keys.contains(&into) {
            return Err("cannot merge a project into itself".into());
        }
        let now = now_millis();
        let tx = self.conn_mut().transaction_with_behavior(TransactionBehavior::Immediate).map_err(err)?;
        let target_merged: Option<String> = tx
            .query_row("SELECT merged_into FROM project_meta WHERE project_key = ?1", [&into], |r| r.get(0))
            .optional()
            .map_err(err)?
            .flatten();
        if let Some(t) = target_merged {
            return Err(format!("{into} is already merged into {t}; merge into {t} instead"));
        }
        for k in &keys {
            let is_target: bool = tx
                .query_row("SELECT EXISTS (SELECT 1 FROM project_meta WHERE merged_into = ?1)", [k], |r| r.get(0))
                .map_err(err)?;
            if is_target {
                return Err(format!("{k} is a merge target; unmerge its projects first"));
            }
        }
        tx.execute(
            "INSERT INTO project_meta (project_key, hidden, updated_at) VALUES (?1, 0, ?2) ON CONFLICT (project_key) DO NOTHING",
            params![into, now],
        )
        .map_err(err)?;
        for k in &keys {
            tx.execute(
                "INSERT INTO project_meta (project_key, hidden, merged_into, updated_at) VALUES (?1, 0, ?2, ?3)
                 ON CONFLICT (project_key) DO UPDATE SET merged_into = excluded.merged_into, updated_at = excluded.updated_at",
                params![k, into, now],
            )
            .map_err(err)?;
        }
        tx.commit().map_err(err)?;
        Ok(keys.len())
    }

    /// 取消合并:清 merged_into;清完后无 alias / note / hidden 的行删除（回到自动态）。返回实际取消的键数。
    pub fn unmerge_projects(&mut self, keys: &[String]) -> Result<usize, String> {
        let keys = clean_keys(keys)?;
        let now = now_millis();
        let tx = self.conn_mut().transaction_with_behavior(TransactionBehavior::Immediate).map_err(err)?;
        let mut n = 0;
        for k in &keys {
            n += tx
                .execute(
                    "UPDATE project_meta SET merged_into = NULL, updated_at = ?2 WHERE project_key = ?1 AND merged_into IS NOT NULL",
                    params![k, now],
                )
                .map_err(err)?;
            tx.execute(
                "DELETE FROM project_meta WHERE project_key = ?1 AND merged_into IS NULL AND alias IS NULL AND note IS NULL AND hidden = 0",
                [k],
            )
            .map_err(err)?;
        }
        tx.commit().map_err(err)?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::store::{Batch, SessionRow, Tokens, TurnPart, TurnRow};
    use crate::collector::task_query::{TaskFilters, TaskPageReq, TaskSort};
    use chrono::NaiveDate;

    const T: i64 = 1_788_602_400_000; // 2026-09-05 本地日内

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
    }

    fn day_of(ms: i64) -> String {
        crate::collector::millis_to_local_day(ms).unwrap()
    }

    /// 一个根会话 `sid` 在 `project` 下 `turns` 轮（每轮 10 token,同源双写 daily_usage）。
    fn session(b: &mut Batch, agent: &str, sid: &str, project: &str, turns: i64, start: i64) {
        b.upsert_session(agent, SessionRow { session_id: sid.into(), project_key: Some(project.into()), ..SessionRow::default() });
        for seq in 1..=turns {
            let at = start + seq * 60_000;
            let day = day_of(at);
            b.add_usage(&day, Some(9), agent, "m", Tokens { input: 10, output: 0, total: 10, cache_read: 0, cache_write: 0 }, 1);
            b.add_turn(
                agent,
                TurnRow {
                    session_id: sid.into(),
                    turn_seq: seq,
                    day: day.clone(),
                    project_key: project.into(),
                    model_key: "m".into(),
                    started_at: at,
                    ended_at: at + 1_000,
                    wall_ms: Some(1_000),
                    model_ms: Some(500),
                    tool_ms: Some(0),
                    ttft_ms: None,
                    gap_ms: None,
                    model_calls: 1,
                    tool_calls: 0,
                    error_count: 0,
                    retry_count: 0,
                    aborted: false,
                    parts: vec![TurnPart { day, model: "m".into(), input: 10, output: 0, total: 10, model_calls: 1, turn_mark: 1 }],
                },
            );
        }
    }

    /// big（2 会话 6 轮）/ small（1 会话 2 轮）/ edge（1 会话 4 轮）/ wide（2 会话 2 轮）/ unknown（1 会话 1 轮）/
    /// app 与 other/app（各 2 会话 5 轮,末段同名）。
    fn fixture() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        let mut b = Batch::default();
        session(&mut b, "claude-code", "b1", "e:/work/big", 3, T);
        session(&mut b, "codex", "b2", "e:/work/big", 3, T + 3_600_000);
        session(&mut b, "claude-code", "s1", "e:/tmp/small", 2, T);
        session(&mut b, "claude-code", "e1", "e:/tmp/edge", 4, T);
        session(&mut b, "claude-code", "w1", "e:/tmp/wide", 1, T);
        session(&mut b, "claude-code", "w2", "e:/tmp/wide", 1, T + 86_400_000);
        session(&mut b, "codebuddy", "u1", UNKNOWN_PROJECT, 1, T);
        session(&mut b, "claude-code", "a1", "e:/work/app", 3, T);
        session(&mut b, "claude-code", "a2", "e:/work/app", 2, T + 7_200_000);
        session(&mut b, "codex", "o1", "d:/other/app", 3, T);
        session(&mut b, "codex", "o2", "d:/other/app", 2, T + 7_200_000);
        s.commit("claude-code", &b).unwrap();
        s
    }

    fn pmap(s: &Store, rule: &ScratchRule) -> HashMap<String, String> {
        let sql = format!("WITH {} SELECT raw_key, eff_key FROM pmap", resolve_cte(rule));
        let mut stmt = s.conn().prepare(&sql).unwrap();
        let out = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).unwrap().flatten().collect();
        out
    }

    fn project_totals(s: &Store, rule: &ScratchRule) -> Vec<(String, String, i64)> {
        s.project_month_rows("2026-09", "project", "total", today(), rule).unwrap().into_iter().map(|r| (r.key, r.label, r.month_total)).collect()
    }

    fn input(key: &str, alias: Option<&str>, hidden: bool) -> ProjectMetaInput {
        ProjectMetaInput { project_key: key.into(), alias: alias.map(str::to_string), hidden, note: None, reset: false }
    }

    fn keys(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scratch_rule_boundaries() {
        let s = fixture();
        let m = pmap(&s, &ScratchRule::DEFAULT);
        assert_eq!(m["e:/work/big"], "e:/work/big", "2 会话:不折叠");
        assert_eq!(m["e:/tmp/small"], SCRATCH_KEY, "1 会话 2 轮:折叠");
        assert_eq!(m["e:/tmp/edge"], SCRATCH_KEY, "1 会话 4 轮（< 5）:折叠");
        assert_eq!(m["e:/tmp/wide"], "e:/tmp/wide", "2 会话即使只有 2 轮也不折叠（两个条件同时满足才折叠）");
        assert_eq!(m[UNKNOWN_PROJECT], SCRATCH_KEY, "unknown 默认归 Scratch");
        // 阈值边界:min_turns = 4 → 4 轮不再 < 4
        let m4 = pmap(&s, &ScratchRule { min_turns: 4, ..ScratchRule::DEFAULT });
        assert_eq!((m4["e:/tmp/edge"].as_str(), m4["e:/tmp/small"].as_str()), ("e:/tmp/edge", SCRATCH_KEY));
        // min_sessions = 3 → 2 会话 2 轮的 wide 也折叠;big 6 轮不折叠
        let m3 = pmap(&s, &ScratchRule { min_sessions: 3, ..ScratchRule::DEFAULT });
        assert_eq!((m3["e:/tmp/wide"].as_str(), m3["e:/work/big"].as_str()), (SCRATCH_KEY, "e:/work/big"));
        // 规则关 + unknown 开关独立
        let off = pmap(&s, &ScratchRule { enabled: false, ..ScratchRule::DEFAULT });
        assert_eq!((off["e:/tmp/small"].as_str(), off[UNKNOWN_PROJECT].as_str()), ("e:/tmp/small", SCRATCH_KEY));
        let unk_off = pmap(&s, &ScratchRule { unknown_as_scratch: false, ..ScratchRule::DEFAULT });
        assert_eq!(unk_off[UNKNOWN_PROJECT], UNKNOWN_PROJECT, "unknown 开关关:unknown 独立成项,不受阈值规则影响");
        let rows = project_totals(&s, &ScratchRule::DEFAULT);
        let scratch = rows.iter().find(|r| r.0 == SCRATCH_KEY).unwrap();
        assert_eq!((scratch.1.as_str(), scratch.2), (SCRATCH_LABEL, (2 + 4 + 1) * 10));
    }

    #[test]
    fn explicit_meta_exempts_from_rule() {
        let mut s = fixture();
        s.set_project_meta(&input("e:/tmp/small", Some("Small one"), false)).unwrap();
        s.set_project_meta(&input("e:/tmp/edge", None, false)).unwrap(); // Keep / Unhide:行存在即显式
        s.set_project_meta(&input(UNKNOWN_PROJECT, None, false)).unwrap();
        let m = pmap(&s, &ScratchRule::DEFAULT);
        assert_eq!((m["e:/tmp/small"].as_str(), m["e:/tmp/edge"].as_str(), m[UNKNOWN_PROJECT].as_str()), ("e:/tmp/small", "e:/tmp/edge", UNKNOWN_PROJECT));
        let rows = project_totals(&s, &ScratchRule::DEFAULT);
        assert!(rows.iter().any(|r| r.0 == "e:/tmp/small" && r.1 == "Small one"), "alias 显示名");
        assert!(rows.iter().all(|r| r.0 != SCRATCH_KEY), "三个成员都脱离规则后 Scratch 为空");
        // reset → 回到自动态
        s.set_project_meta(&ProjectMetaInput { project_key: "e:/tmp/edge".into(), reset: true, ..Default::default() }).unwrap();
        assert_eq!(pmap(&s, &ScratchRule::DEFAULT)["e:/tmp/edge"], SCRATCH_KEY);
    }

    #[test]
    fn alias_labels_and_validation() {
        let mut s = fixture();
        let off = ScratchRule::OFF;
        let rows = project_totals(&s, &off);
        let app: Vec<&str> = rows.iter().filter(|r| r.0.ends_with("/app")).map(|r| r.1.as_str()).collect();
        assert_eq!(app.len(), 2);
        assert!(app.iter().all(|l| l.contains(":/")), "同名末段退回完整路径:{app:?}");
        s.set_project_meta(&input("e:/work/app", Some("  Work App  "), false)).unwrap();
        let rows = project_totals(&s, &off);
        assert_eq!(rows.iter().find(|r| r.0 == "e:/work/app").unwrap().1, "Work App", "alias trim");
        assert_eq!(rows.iter().find(|r| r.0 == "d:/other/app").unwrap().1, "app", "另一方有 alias 后不再重名");
        let long = "x".repeat(META_TEXT_MAX + 1);
        assert!(s.set_project_meta(&input("e:/work/app", Some(&long), false)).is_err());
        assert!(s.set_project_meta(&ProjectMetaInput { project_key: "e:/work/app".into(), note: Some(long), ..Default::default() }).is_err());
        assert!(s.set_project_meta(&input("e:/work/app", Some(&"é".repeat(META_TEXT_MAX)), false)).is_ok(), "按字符计数");
        assert!(s.set_project_meta(&input("e:/work/app", Some("a\nb"), false)).is_err());
        assert!(s.set_project_meta(&input("  ", None, false)).is_err());
        // 空 alias = 清除
        s.set_project_meta(&input("e:/work/app", Some("   "), false)).unwrap();
        assert!(!s.project_aliases().contains_key("e:/work/app"));
    }

    #[test]
    fn hidden_projects_leave_project_views_only() {
        let mut s = fixture();
        let rule = ScratchRule::DEFAULT;
        s.set_project_meta(&input("e:/work/big", None, true)).unwrap();
        let rows = project_totals(&s, &rule);
        assert!(rows.iter().all(|r| r.0 != "e:/work/big"));
        let (d0, d1) = ("2026-09-01", "2026-09-30");
        let series = s.effort_series(d0, d1, "day", "project", "turns", None, &rule).unwrap();
        assert!(!series.series_keys.contains(&"e:/work/big".to_string()), "下拉候选（effort 系列键）不含隐藏项目");
        let tasks = s.task_list(0, i64::MAX, &TaskFilters::default(), &TaskSort::default(), &TaskPageReq { offset: 0, limit: 500 }, &rule).unwrap();
        assert!(tasks.rows.iter().all(|t| t.project_raw != "e:/work/big"));
        assert_eq!(tasks.total, 9);
        assert_eq!(s.data_span(Some("e:/work/big"), &rule), None, "隐藏项目无生命周期");
        // Scratch 伪项目整体隐藏
        s.set_project_meta(&input(SCRATCH_KEY, None, true)).unwrap();
        let rows = project_totals(&s, &rule);
        assert!(rows.iter().all(|r| r.0 != SCRATCH_KEY));
        let list = s.list_project_meta(&rule).unwrap();
        assert!(list.scratch_hidden);
        let small = list.rows.iter().find(|r| r.key == "e:/tmp/small").unwrap();
        assert_eq!((small.status.as_str(), small.effective_key.as_deref(), small.managed), ("scratch", None, false));
        let big = list.rows.iter().find(|r| r.key == "e:/work/big").unwrap();
        assert_eq!((big.status.as_str(), big.effective_key.as_deref(), big.sessions, big.turns, big.tokens), ("hidden", None, 2, 6, 60));
        assert_eq!(big.agents, vec!["Claude Code".to_string(), "Codex".to_string()]);
        // Unhide:行保留 → 显式
        s.set_project_meta(&input("e:/work/big", None, false)).unwrap();
        assert!(project_totals(&s, &rule).iter().any(|r| r.0 == "e:/work/big"));
    }

    #[test]
    fn manual_scratch_via_merge_into_scratch_key() {
        let mut s = fixture();
        let rule = ScratchRule::DEFAULT;
        // 大目录（规则不折叠）手动归入 Scratch
        assert_eq!(s.merge_projects(&keys(&["e:/work/big"]), SCRATCH_KEY).unwrap(), 1);
        assert_eq!(pmap(&s, &rule)["e:/work/big"], SCRATCH_KEY);
        let list = s.list_project_meta(&rule).unwrap();
        assert!(list.rows.iter().all(|r| r.key != SCRATCH_KEY), "Scratch 伪项目仍不进管理列表");
        let big = list.rows.iter().find(|r| r.key == "e:/work/big").unwrap();
        assert_eq!((big.status.as_str(), big.effective_key.as_deref(), big.managed), ("scratch", Some(SCRATCH_KEY), true));
        // Scratch 隐藏 → 连带不可见
        s.set_project_meta(&input(SCRATCH_KEY, None, true)).unwrap();
        assert!(!pmap(&s, &rule).contains_key("e:/work/big"));
        s.set_project_meta(&input(SCRATCH_KEY, None, false)).unwrap();
        // 回 Active：取消合并 + 保留行（Keep）
        assert_eq!(s.unmerge_projects(&keys(&["e:/work/big"])).unwrap(), 1);
        s.set_project_meta(&input("e:/work/big", None, false)).unwrap();
        assert_eq!(pmap(&s, &rule)["e:/work/big"], "e:/work/big");
        let big = s.list_project_meta(&rule).unwrap().rows.into_iter().find(|r| r.key == "e:/work/big").unwrap();
        assert_eq!(big.status, "active");
        // Scratch 仍不能作合并源
        assert!(s.merge_projects(&keys(&[SCRATCH_KEY]), "e:/work/big").is_err());
    }

    #[test]
    fn merge_one_level_and_rejects_cycles() {
        let mut s = fixture();
        let rule = ScratchRule::DEFAULT;
        assert_eq!(s.merge_projects(&keys(&["d:/other/app", "e:/tmp/small"]), "e:/work/app").unwrap(), 2);
        let m = pmap(&s, &rule);
        assert_eq!((m["d:/other/app"].as_str(), m["e:/tmp/small"].as_str()), ("e:/work/app", "e:/work/app"), "合并成员（含原 Scratch 成员）归目标");
        let rows = project_totals(&s, &rule);
        let app = rows.iter().find(|r| r.0 == "e:/work/app").unwrap();
        assert_eq!((app.1.as_str(), app.2), ("app", 50 + 50 + 20), "合并后同名冲突消失,取末段");
        // 项目维钻取 / 任务筛选 / 生命周期都按有效键
        let bd = s.project_breakdown("project", "e:/work/app", "2026-09", today(), &rule).unwrap();
        let agents: BTreeSet<String> = bd.iter().flat_map(|d| d.slices.iter().flatten().map(|x| x.key.clone())).collect();
        assert_eq!(agents, ["claude-code", "codex"].iter().map(|s| s.to_string()).collect());
        let f = TaskFilters { agent: None, project: Some("e:/work/app".into()) };
        let tasks = s.task_list(0, i64::MAX, &f, &TaskSort::default(), &TaskPageReq::default(), &rule).unwrap();
        assert_eq!(tasks.total, 5);
        assert!(tasks.rows.iter().all(|t| t.project == "e:/work/app" && t.project_label == "app"));
        assert!(tasks.rows.iter().any(|t| t.project_raw == "d:/other/app"));
        assert!(s.data_span(Some("e:/work/app"), &rule).is_some());

        // 拒绝:并入自身 / 目标已合并 / 源是他人目标 / Scratch
        assert!(s.merge_projects(&keys(&["e:/work/app"]), "e:/work/app").is_err(), "环:并入自身");
        let chain = s.merge_projects(&keys(&["e:/work/big"]), "d:/other/app");
        assert!(chain.unwrap_err().contains("already merged"), "目标本身已合并 → 拒绝两层");
        let target_as_source = s.merge_projects(&keys(&["e:/work/app"]), "e:/work/big");
        assert!(target_as_source.unwrap_err().contains("merge target"), "源是合并目标 → 拒绝（否则成环 / 两层）");
        assert!(s.merge_projects(&keys(&[SCRATCH_KEY]), "e:/work/big").is_err());
        assert!(s.merge_projects(&[], "e:/work/big").is_err());

        // 目标隐藏 → 成员连带不可见
        s.set_project_meta(&input("e:/work/app", None, true)).unwrap();
        let m = pmap(&s, &rule);
        assert!(!m.contains_key("d:/other/app") && !m.contains_key("e:/work/app"));
        s.set_project_meta(&input("e:/work/app", None, false)).unwrap();

        // 列表状态
        let list = s.list_project_meta(&rule).unwrap();
        let other = list.rows.iter().find(|r| r.key == "d:/other/app").unwrap();
        assert_eq!((other.status.as_str(), other.merged_label.as_deref(), other.effective_key.as_deref()), ("merged", Some("e:/work/app"), Some("e:/work/app")), "列表覆盖全部原始键:同名末段在列表内退回完整路径");

        // 取消合并:空行删除 → small 回到 Scratch;目标 reset 时仍被引用 → 保留行
        assert_eq!(s.unmerge_projects(&keys(&["e:/tmp/small", "e:/work/big"])).unwrap(), 1);
        assert_eq!(pmap(&s, &rule)["e:/tmp/small"], SCRATCH_KEY);
        s.set_project_meta(&ProjectMetaInput { project_key: "e:/work/app".into(), reset: true, ..Default::default() }).unwrap();
        assert_eq!(pmap(&s, &rule)["d:/other/app"], "e:/work/app", "被引用的目标 reset 保留行,成员不被卷进 Scratch");
        s.unmerge_projects(&keys(&["d:/other/app"])).unwrap();
        assert_eq!(pmap(&s, &rule)["d:/other/app"], "d:/other/app");
    }

    #[test]
    fn meta_never_changes_non_project_numbers() {
        let mut s = fixture();
        let rule = ScratchRule::DEFAULT;
        let snapshot = |s: &Store| {
            let rows = |dim: &str, metric: &str| {
                s.project_month_rows("2026-09", dim, metric, today(), &rule).unwrap().into_iter().map(|r| (r.key, r.values, r.message_counts)).collect::<Vec<_>>()
            };
            (
                rows("agent", "total"),
                rows("model", "turns"),
                s.effort_series("2026-09-01", "2026-09-30", "day", "agent", "wait", None, &rule).unwrap().points.into_iter().map(|p| p.values).collect::<Vec<_>>(),
                s.effort_series("2026-09-01", "2026-09-30", "day", "total", "total", None, &rule).unwrap().points.into_iter().map(|p| p.values).collect::<Vec<_>>(),
                s.month_rows("2026-09", "agent", "total", today()).unwrap().into_iter().map(|r| (r.key, r.values)).collect::<Vec<_>>(),
            )
        };
        let before = snapshot(&s);
        let usage_total: i64 = s.conn().query_row("SELECT SUM(total_tokens) FROM daily_usage", [], |r| r.get(0)).unwrap();
        assert_eq!(project_totals(&s, &rule).iter().map(|r| r.2).sum::<i64>(), usage_total, "Scratch 折叠无损");

        s.set_project_meta(&input("e:/work/big", Some("Big"), true)).unwrap();
        s.merge_projects(&keys(&["d:/other/app"]), "e:/work/app").unwrap();
        s.set_project_meta(&input(SCRATCH_KEY, None, true)).unwrap();

        assert_eq!(snapshot(&s), before, "agent / model / total 维与 daily_usage 读入口不受隐藏与合并影响");
        assert!(s.test_project_conservation().is_empty(), "原始表守恒不变:{:?}", s.test_project_conservation());
        // 可见项目 + 隐藏部分 = 全量（隐藏只是从项目维视图拿走,不丢数）
        let visible: i64 = project_totals(&s, &rule).iter().map(|r| r.2).sum();
        let hidden: i64 = s
            .conn()
            .query_row(
                &format!("WITH {} SELECT COALESCE(SUM(total_tokens), 0) FROM daily_project WHERE project_key NOT IN (SELECT raw_key FROM pmap)", resolve_cte(&rule)),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(visible + hidden, usage_total);
        // agent 行钻取按项目切片:隐藏部分归 Hidden projects 切片,切片和 = 格值
        let agent_rows = s.project_month_rows("2026-09", "agent", "total", today(), &rule).unwrap();
        for r in &agent_rows {
            let bd = s.project_breakdown("agent", &r.key, "2026-09", today(), &rule).unwrap();
            for (i, d) in bd.iter().enumerate() {
                let sum: i64 = d.slices.as_ref().map_or(0, |v| v.iter().map(|x| x.tokens).sum());
                assert_eq!(Some(sum), r.values[i], "{} {}", r.key, d.day);
            }
        }
        assert!(s
            .project_breakdown("agent", "claude-code", "2026-09", today(), &rule)
            .unwrap()
            .iter()
            .flat_map(|d| d.slices.iter().flatten())
            .any(|x| x.key == HIDDEN_SLICE_KEY && x.label == HIDDEN_SLICE_LABEL));
    }

    #[test]
    fn project_meta_survives_reset_migration() {
        let dir = std::env::temp_dir().join(format!("tc-project-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("collector.db");
        let _ = std::fs::remove_file(&path);
        {
            let mut s = Store::open(&path).unwrap();
            s.set_project_meta(&input("e:/work/app", Some("Work"), false)).unwrap();
            s.merge_projects(&keys(&["d:/other/app"]), "e:/work/app").unwrap();
            s.conn().execute_batch("PRAGMA user_version = 9;").unwrap();
        }
        let s = Store::open(&path).unwrap();
        let v: i64 = s.conn().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, crate::collector::store::SCHEMA_VERSION, "迁移清库已发生");
        assert_eq!(s.project_aliases().get("e:/work/app").map(String::as_str), Some("Work"));
        let merged: Option<String> = s.conn().query_row("SELECT merged_into FROM project_meta WHERE project_key = 'd:/other/app'", [], |r| r.get(0)).unwrap();
        assert_eq!(merged.as_deref(), Some("e:/work/app"), "project_meta 跨清库保留");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rule_prefs_roundtrip_and_bounds() {
        assert_eq!(ScratchRule::from_prefs("{}"), ScratchRule::DEFAULT);
        assert_eq!(ScratchRule::from_prefs("not json"), ScratchRule::DEFAULT);
        let r = ScratchRule { enabled: false, min_sessions: 3, min_turns: 12, unknown_as_scratch: false };
        let json = r.merge_into_prefs(Some(r#"{"idleThresholdMin":45,"scratchMinTurns":"x"}"#));
        assert_eq!(ScratchRule::from_prefs(&json), r);
        assert!(json.contains("\"idleThresholdMin\":45"), "其余键原样保留");
        let bad = ScratchRule::from_prefs(r#"{"scratchMinSessions":0,"scratchMinTurns":9999,"scratchRuleEnabled":"yes"}"#);
        assert_eq!(bad, ScratchRule::DEFAULT, "越界 / 类型错逐键回落默认");
        assert!(ScratchRule { min_sessions: 0, ..ScratchRule::DEFAULT }.validate().is_err());
        assert!(ScratchRule { min_turns: 501, ..ScratchRule::DEFAULT }.validate().is_err());
        assert!(ScratchRule::DEFAULT.validate().is_ok());
    }

    #[test]
    fn short_labels() {
        assert_eq!(project_short_label("e:/Projects/TokenCalendar"), "TokenCalendar");
        assert_eq!(project_short_label(UNKNOWN_PROJECT), UNKNOWN_LABEL);
        assert_eq!(project_short_label(SCRATCH_KEY), SCRATCH_LABEL);
        assert_eq!(project_short_label("/"), "/");
        let aliases: HashMap<String, String> = [("e:/b/x".to_string(), "Mine".to_string())].into();
        assert_eq!(project_labels(&keys(&["e:/a/x", "e:/b/x", "d:/c/x"]), &aliases), vec!["e:/a/x", "Mine", "d:/c/x"]);
    }
}
