//! 双轨下轨：绝对区间内的根任务与轮执行块。
//!
//! 与任务列表的「会话开始在范围内」不同，这里按轮与区间重叠选取；跨左边界的长任务
//! 不会漏掉。开放结束时间的轮只在自身起点落入区间时出现，不向后伪造成永久运行。

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use super::project_meta::{project_labels, resolve_cte, ScratchRule};
use super::store::Store;
use super::task_query::{price_turn_parts, ModelCost};
use crate::subscription::model::Platform;

const ALIGNMENT_TASK_MAX: usize = 50;
const ALIGNMENT_TURN_MAX: usize = 5000;

#[derive(Debug, Clone, Serialize)]
pub struct AlignmentTurn {
    pub turn_seq: i64,
    pub project: String,
    pub project_raw: String,
    pub project_label: String,
    pub model: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub wall_ms: Option<i64>,
    pub model_ms: Option<i64>,
    pub tool_ms: Option<i64>,
    pub steps: i64,
    pub tool_calls: i64,
    pub total_tokens: i64,
    pub aborted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd_by_model: Option<Vec<ModelCost>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlignmentTask {
    pub agent: String,
    pub session_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub project: String,
    pub project_raw: String,
    pub project_label: String,
    pub turns: Vec<AlignmentTurn>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AlignmentTasks {
    pub total_tasks: usize,
    pub returned_tasks: usize,
    pub returned_turns: usize,
    pub truncated: bool,
    pub tasks: Vec<AlignmentTask>,
}

#[derive(Debug)]
struct RootRow {
    agent: String,
    session_id: String,
    started_at: i64,
    ended_at: Option<i64>,
}

fn overlaps(turn: &AlignmentTurn, from_ms: i64, to_ms: i64) -> bool {
    (turn.started_at >= from_ms && turn.started_at < to_ms)
        || (turn.started_at < from_ms && turn.ended_at.is_some_and(|ended| ended > from_ms))
}

impl Store {
    /// 双轨下轨的一次批量查询：命中根任务、这些任务的轮时间窗、会话树切片分别批量取出，
    /// 成本仍走共享的 `price_turn_parts`。全程只读，不写库、不访问网络。
    pub fn task_alignment(
        &self,
        platform: Platform,
        from_ms: i64,
        to_ms: i64,
        project: Option<&str>,
        rule: &ScratchRule,
    ) -> Result<AlignmentTasks, String> {
        if from_ms >= to_ms {
            return Err("invalid alignment range".into());
        }
        let agent = platform.collector_source();
        let cte = resolve_cte(rule);
        let overlap_sql = "(t.started_at >= ?2 AND t.started_at < ?3)
             OR (t.started_at < ?2 AND t.ended_at IS NOT NULL AND t.ended_at > ?2)";
        let mut root_params: Vec<rusqlite::types::Value> =
            vec![agent.to_string().into(), from_ms.into(), to_ms.into()];
        let mut root_where =
            format!("s.parent_id IS NULL AND t.agent_key = ?1 AND ({overlap_sql})");
        if let Some(project) = project.filter(|project| !project.is_empty()) {
            root_params.push(project.to_string().into());
            root_where.push_str(&format!(" AND p.eff_key = ?{}", root_params.len()));
        }
        let root_sql = format!(
            "WITH {cte}
             SELECT s.agent_key, s.session_id, s.started_at, s.ended_at, MAX(t.started_at) AS latest
               FROM session s
               JOIN turn t ON t.agent_key = s.agent_key AND t.session_id = s.session_id
               JOIN pmap p ON p.raw_key = t.project_key
              WHERE {root_where}
              GROUP BY s.agent_key, s.session_id, s.started_at, s.ended_at
              ORDER BY latest DESC, s.session_id"
        );
        let mut statement = self
            .conn()
            .prepare(&root_sql)
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(root_params.iter()), |row| {
                Ok(RootRow {
                    agent: row.get(0)?,
                    session_id: row.get(1)?,
                    started_at: row.get(2)?,
                    ended_at: row.get(3)?,
                })
            })
            .map_err(|error| error.to_string())?;
        let mut roots: Vec<RootRow> = rows.flatten().collect();
        drop(statement);
        let total_tasks = roots.len();
        let mut truncated = total_tasks > ALIGNMENT_TASK_MAX;
        roots.truncate(ALIGNMENT_TASK_MAX);
        if roots.is_empty() {
            return Ok(AlignmentTasks {
                total_tasks,
                returned_tasks: 0,
                returned_turns: 0,
                truncated,
                tasks: Vec::new(),
            });
        }

        let ids: Vec<String> = roots.iter().map(|root| root.session_id.clone()).collect();
        let id_marks = (0..ids.len())
            .map(|index| format!("?{}", index + 2))
            .collect::<Vec<_>>()
            .join(",");
        let to_param = ids.len() + 2;
        let turn_sql = format!(
            "WITH {cte}
             SELECT t.session_id, t.turn_seq, t.project_key, p.eff_key, t.model_key,
                    t.started_at, t.ended_at, t.wall_ms, t.model_ms, t.tool_ms,
                    t.model_calls, t.tool_calls, t.total_tokens, t.aborted
               FROM turn t
               JOIN pmap p ON p.raw_key = t.project_key
              WHERE t.agent_key = ?1 AND t.session_id IN ({id_marks}) AND t.started_at < ?{to_param}
              ORDER BY t.session_id, t.started_at, t.turn_seq"
        );
        let mut turn_params: Vec<rusqlite::types::Value> = vec![agent.to_string().into()];
        turn_params.extend(ids.iter().cloned().map(Into::into));
        turn_params.push(to_ms.into());
        let mut statement = self
            .conn()
            .prepare(&turn_sql)
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(turn_params.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    AlignmentTurn {
                        turn_seq: row.get(1)?,
                        project_raw: row.get(2)?,
                        project: row.get(3)?,
                        project_label: String::new(),
                        model: row.get(4)?,
                        started_at: row.get(5)?,
                        ended_at: row.get(6)?,
                        wall_ms: row.get(7)?,
                        model_ms: row.get(8)?,
                        tool_ms: row.get(9)?,
                        steps: row.get(10)?,
                        tool_calls: row.get(11)?,
                        total_tokens: row.get(12)?,
                        aborted: row.get::<_, i64>(13)? != 0,
                        usd: None,
                        usd_by_model: None,
                    },
                ))
            })
            .map_err(|error| error.to_string())?;
        let mut turns_by_root: BTreeMap<String, Vec<AlignmentTurn>> = BTreeMap::new();
        for row in rows.flatten() {
            turns_by_root.entry(row.0).or_default().push(row.1);
        }
        drop(statement);

        let agent_param = ids.len() + 2;
        let to_param = ids.len() + 3;
        let parts_sql = format!(
            "WITH RECURSIVE tree(root_sid, sid) AS (
                 SELECT session_id, session_id FROM session
                  WHERE agent_key = ?1 AND session_id IN ({id_marks})
                 UNION ALL
                 SELECT tree.root_sid, s.session_id FROM session s JOIN tree ON s.parent_id = tree.sid
             )
             SELECT tree.root_sid, r.started_at, p.model_key,
                    SUM(p.input_tokens), SUM(p.output_tokens), SUM(p.cache_read_tokens), SUM(p.cache_write_tokens)
               FROM tree
               JOIN turn_part p ON p.agent_key = ?{agent_param} AND p.session_id = tree.sid
               JOIN turn_raw r ON r.agent_key = p.agent_key AND r.session_id = p.session_id AND r.turn_seq = p.turn_seq
              WHERE r.started_at < ?{to_param}
              GROUP BY tree.root_sid, p.session_id, p.turn_seq, p.model_key"
        );
        let mut part_params: Vec<rusqlite::types::Value> = vec![agent.to_string().into()];
        part_params.extend(ids.iter().cloned().map(Into::into));
        part_params.push(agent.to_string().into());
        part_params.push(to_ms.into());
        let mut statement = self
            .conn()
            .prepare(&parts_sql)
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(part_params.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    [
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                    ],
                ))
            })
            .map_err(|error| error.to_string())?;
        let mut parts_by_root: HashMap<String, Vec<(i64, String, [i64; 4])>> = HashMap::new();
        for row in rows.flatten() {
            parts_by_root
                .entry(row.0)
                .or_default()
                .push((row.1, row.2, row.3));
        }
        drop(statement);

        let mut tasks = Vec::new();
        for root in roots {
            let all_turns = turns_by_root.remove(&root.session_id).unwrap_or_default();
            let keys: Vec<(i64, i64)> = all_turns
                .iter()
                .map(|turn| (turn.turn_seq, turn.started_at))
                .collect();
            let costs = price_turn_parts(
                platform,
                &keys,
                parts_by_root.remove(&root.session_id).unwrap_or_default(),
            );
            let mut turns: Vec<AlignmentTurn> = all_turns
                .into_iter()
                .filter(|turn| overlaps(turn, from_ms, to_ms))
                .filter(|turn| {
                    project.is_none_or(|selected| selected.is_empty() || turn.project == selected)
                })
                .map(|mut turn| {
                    if let Some((usd, by_model)) = costs.get(&turn.turn_seq) {
                        turn.usd = Some(*usd);
                        turn.usd_by_model = Some(
                            by_model
                                .iter()
                                .map(|(model, usd)| ModelCost {
                                    model_key: model.clone(),
                                    usd: *usd,
                                })
                                .collect(),
                        );
                    }
                    turn
                })
                .collect();
            turns.sort_by(|a, b| {
                a.started_at
                    .cmp(&b.started_at)
                    .then(a.turn_seq.cmp(&b.turn_seq))
            });
            if !turns.is_empty() {
                tasks.push(AlignmentTask {
                    agent: root.agent,
                    session_id: root.session_id,
                    started_at: root.started_at,
                    ended_at: root.ended_at,
                    project: String::new(),
                    project_raw: String::new(),
                    project_label: String::new(),
                    turns,
                });
            }
        }

        let visible_turns: usize = tasks
            .iter()
            .map(|task: &AlignmentTask| task.turns.len())
            .sum();
        if visible_turns > ALIGNMENT_TURN_MAX {
            truncated = true;
            let mut remaining = ALIGNMENT_TURN_MAX;
            let mut limited = Vec::new();
            for mut task in tasks {
                if remaining == 0 {
                    break;
                }
                if task.turns.len() > remaining {
                    let keep_from = task.turns.len() - remaining;
                    task.turns = task.turns.split_off(keep_from);
                }
                remaining -= task.turns.len();
                limited.push(task);
            }
            tasks = limited;
        }

        let mut project_keys: Vec<String> = tasks
            .iter()
            .flat_map(|task| task.turns.iter().map(|turn| turn.project.clone()))
            .collect();
        project_keys.sort();
        project_keys.dedup();
        let labels: HashMap<String, String> = project_keys
            .iter()
            .cloned()
            .zip(project_labels(&project_keys, &self.project_aliases()))
            .collect();
        for task in &mut tasks {
            for turn in &mut task.turns {
                turn.project_label = labels
                    .get(&turn.project)
                    .cloned()
                    .unwrap_or_else(|| turn.project.clone());
            }
            if let Some(first) = task.turns.first() {
                task.project = first.project.clone();
                task.project_raw = first.project_raw.clone();
                task.project_label = first.project_label.clone();
            }
        }
        let returned_tasks = tasks.len();
        let returned_turns = tasks.iter().map(|task| task.turns.len()).sum();
        Ok(AlignmentTasks {
            total_tasks,
            returned_tasks,
            returned_turns,
            truncated,
            tasks,
        })
    }
}
