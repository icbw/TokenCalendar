//! 契约冻结用的确定性 fixture 数据。
//!
//! 设计目标：
//! - 形状与真实采集器完全一致（get_monthly_matrix / get_breakdown），
//!   届时仅替换数据来源，前端零改动。
//! - 守恒：任意一天 Σagent 行值 == Σmodel 行值 == Σ（agent×model) 明细；
//!   breakdown 切片之和 == 对应矩阵单元格值。
//! - null ≠ 0 硬语义：未来日期 → None（前端透明格）；无数据 → Some（0)（灰格）。
//! - 确定性：同 （日期, agent, model) 永远产出同一个数（mulberry32 按种子派生），
//!   重启/重扫不会变化——这是 fixture 而非随机 mock 的意义。
//!
//! 起退役为纯测试资产（main.rs 以 #[cfg（test)] 挂载,生产零引用）,
//! 守恒与 null 语义测试仍是契约的活文档。个别字段仅测试部分路径消费。

use chrono::{Datelike, NaiveDate};

/// fixture 的数据起点：更早的月份一律 Some（0)（真实零，灰格），
/// 用于验证网格左边缘的零值渲染。
pub const HISTORY_START_YM: (i32, u32) = (2025, 10);

pub struct AgentDef {
    pub key: &'static str,
    pub label: &'static str,
    pub weight: f64,
}

pub const AGENTS: [AgentDef; 10] = [
    AgentDef { key: "zcode", label: "ZCode", weight: 1.0 },
    AgentDef { key: "workbuddy", label: "WorkBuddy", weight: 0.85 },
    AgentDef { key: "claude-code", label: "Claude Code", weight: 0.7 },
    AgentDef { key: "codex", label: "Codex", weight: 0.6 },
    AgentDef { key: "gemini-cli", label: "Gemini CLI", weight: 0.5 },
    AgentDef { key: "aider", label: "Aider", weight: 0.4 },
    AgentDef { key: "cursor", label: "Cursor", weight: 0.3 },
    AgentDef { key: "copilot", label: "Copilot", weight: 0.25 },
    AgentDef { key: "windsurf", label: "Windsurf", weight: 0.2 },
    AgentDef { key: "aichat", label: "AIChat", weight: 0.1 },
];

pub struct ModelDef {
    pub key: &'static str,
    pub label: &'static str,
    pub share: f64,
}

pub const MODELS: [ModelDef; 5] = [
    ModelDef { key: "gpt-5", label: "GPT-5", share: 0.35 },
    ModelDef { key: "claude-opus-4", label: "Claude Opus 4", share: 0.3 },
    ModelDef { key: "claude-sonnet-4", label: "Claude Sonnet 4", share: 0.2 },
    ModelDef { key: "glm-5.3", label: "GLM-5.3", share: 0.1 },
    ModelDef { key: "deepseek-v4", label: "DeepSeek V4", share: 0.05 },
];

/// mulberry32 PRNG（与旧项目 mock 同族的规范实现），跨语言可复现。
fn mulberry32(seed: u32) -> impl FnMut() -> f64 {
    let mut s = seed;
    move || {
        s = s.wrapping_add(0x6D2B79F5);
        let mut t = (s ^ (s >> 15)).wrapping_mul(1 | s);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        (t ^ (t >> 14)) as f64 / 4294967296.0
    }
}

/// （日期, agent, model) → 种子。大素数异或混合，避免规律性碰撞。
fn day_seed(date: NaiveDate, agent_idx: usize, model_idx: usize) -> u32 {
    let y = date.year() as u32;
    let m = date.month() as u32;
    let d = date.day() as u32;
    y.wrapping_mul(0x9E37_79B1)
        ^ m.wrapping_mul(0x85EB_CA77)
        ^ d.wrapping_mul(0xC2B2_AE3D)
        ^ ((agent_idx as u32 + 1).wrapping_mul(0x2722_0A95))
        ^ (model_idx as u32)
}

/// 每周活跃日模式（0=周日），按 agent 轮换，模拟真实工作节律。
fn active_days(agent_idx: usize) -> [u32; 5] {
    match agent_idx % 4 {
        0 => [1, 2, 3, 4, 5],
        1 => [1, 2, 3, 4, 1],
        2 => [2, 3, 4, 2, 3],
        _ => [1, 3, 5, 1, 3],
    }
}

/// 某天某 agent×model 的 token 总量（守恒的原子明细）。
pub fn agent_model_tokens(date: NaiveDate, agent_idx: usize, model_idx: usize) -> i64 {
    let wd = date.weekday().num_days_from_sunday();
    let agent = &AGENTS[agent_idx];
    let model = &MODELS[model_idx];
    let mut rng = mulberry32(day_seed(date, agent_idx, model_idx));

    let is_active = active_days(agent_idx).contains(&wd) && rng() >= 0.09;
    if !is_active {
        return 0;
    }
    let base = 2_000_000.0 * agent.weight * model.share;
    let weekend = if wd == 0 || wd == 6 { 0.35 } else { 1.0 };
    let dip = if date.day() % 11 == 0 { 0.15 } else { 1.0 };
    let spike = if rng() < 0.06 { 6.0 } else { 1.0 };
    (base * weekend * dip * (0.4 + rng()) * spike) as i64
}

/// input/output 拆分（旧 UI 的 Input/Output 指标位）：input 占比确定性落在
/// [0.62, 0.82)，output = total − input，保证 input + output == total 恒成立。
pub fn agent_model_io(date: NaiveDate, agent_idx: usize, model_idx: usize) -> (i64, i64) {
    let total = agent_model_tokens(date, agent_idx, model_idx);
    if total == 0 {
        return (0, 0);
    }
    let mut rng = mulberry32(day_seed(date, agent_idx, model_idx).wrapping_add(0x1B87_3CA9));
    let input = (total as f64 * (0.62 + 0.2 * rng())) as i64;
    (input, total - input)
}

/// 该月是否在 fixture 数据范围内（早于 HISTORY_START 的月份全零）。
fn in_history(ym: (i32, u32)) -> bool {
    ym >= HISTORY_START_YM
}

#[allow(dead_code)] // 退役测试资产：部分字段仅旧生产路径消费
pub struct FixtureRow {
    pub key: String,
    pub label: String,
    /// 长度 = days_in_month；下标 0 = 1 号。None=未来日期，Some（0)=真实零。
    pub values: Vec<Option<i64>>,
    pub month_total: i64,
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

pub fn days_in_month(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let first_next = NaiveDate::from_ymd_opt(ny, nm, 1).expect("valid next month");
    (first_next - NaiveDate::from_ymd_opt(y, m, 1).expect("valid month")).num_days() as u32
}

fn month_dates(month: &str) -> Option<(NaiveDate, u32)> {
    let (y, m) = parse_month(month)?;
    let first = NaiveDate::from_ymd_opt(y, m, 1)?;
    Some((first, days_in_month(y, m)))
}

/// 月度矩阵行（group_by: "agent" | "model"；metric: "total" | "input" | "output"）。
/// today 用于 null 截断。
pub fn month_rows(month: &str, group_by: &str, metric: &str, today: NaiveDate) -> Option<Vec<FixtureRow>> {
    let (first, dim) = month_dates(month)?;
    let (y, m) = parse_month(month)?;
    let historical = in_history((y, m));

    let n = match group_by {
        "model" => MODELS.len(),
        _ => AGENTS.len(),
    };

    let mut rows: Vec<FixtureRow> = Vec::with_capacity(n);
    for i in 0..n {
        let (key, label) = match group_by {
            "model" => (MODELS[i].key, MODELS[i].label),
            _ => (AGENTS[i].key, AGENTS[i].label),
        };
        let mut values = Vec::with_capacity(dim as usize);
        let mut month_total = 0i64;
        for d in 1..=dim {
            let date = first + chrono::Duration::days((d - 1) as i64);
            if date > today {
                values.push(None);
                continue;
            }
            let day_total = if historical {
                match group_by {
                    "model" => (0..AGENTS.len())
                        .map(|a| match metric {
                            "input" => agent_model_io(date, a, i).0,
                            "output" => agent_model_io(date, a, i).1,
                            _ => agent_model_tokens(date, a, i),
                        })
                        .sum(),
                    _ => (0..MODELS.len())
                        .map(|mo| match metric {
                            "input" => agent_model_io(date, i, mo).0,
                            "output" => agent_model_io(date, i, mo).1,
                            _ => agent_model_tokens(date, i, mo),
                        })
                        .sum(),
                }
            } else {
                0
            };
            month_total += day_total;
            values.push(Some(day_total));
        }
        rows.push(FixtureRow { key: key.to_string(), label: label.to_string(), values, month_total });
    }
    rows.sort_by(|a, b| b.month_total.cmp(&a.month_total).then(a.key.cmp(&b.key)));
    Some(rows)
}

#[allow(dead_code)]
pub struct FixtureSlice {
    pub key: String,
    pub label: String,
    pub tokens: i64,
}

#[allow(dead_code)]
pub struct FixtureBreakdownDay {
    pub day: String,
    pub slices: Option<Vec<FixtureSlice>>,
}

/// 行钻取：kind="agent" → 每日模型构成；kind="model" → 每日 Agent 构成。
/// 与 month_rows 消费同一份 agent×model 明细，保证守恒。
pub fn breakdown(kind: &str, key: &str, month: &str, today: NaiveDate) -> Option<Vec<FixtureBreakdownDay>> {
    let (first, dim) = month_dates(month)?;
    let (y, m) = parse_month(month)?;
    let historical = in_history((y, m));

    let outer: Option<usize> = match kind {
        "model" => MODELS.iter().position(|mo| mo.key == key),
        _ => AGENTS.iter().position(|a| a.key == key),
    };
    let outer = match outer {
        Some(i) => i,
        None => return Some(Vec::new()),
    };

    let mut out = Vec::with_capacity(dim as usize);
    for d in 1..=dim {
        let date = first + chrono::Duration::days((d - 1) as i64);
        if date > today {
            continue; // 旧契约：未来日期不产出 breakdown day
        }
        let mut slices: Vec<FixtureSlice> = Vec::new();
        if historical {
            let inner = match kind {
                "model" => (0..AGENTS.len()).collect::<Vec<_>>(),
                _ => (0..MODELS.len()).collect::<Vec<_>>(),
            };
            for i in inner {
                let tokens = match kind {
                    "model" => agent_model_tokens(date, i, outer),
                    _ => agent_model_tokens(date, outer, i),
                };
                if tokens <= 0 {
                    continue;
                }
                let (skey, slabel) = match kind {
                    "model" => (AGENTS[i].key, AGENTS[i].label),
                    _ => (MODELS[i].key, MODELS[i].label),
                };
                slices.push(FixtureSlice { key: skey.to_string(), label: slabel.to_string(), tokens });
            }
            slices.sort_by(|a, b| b.tokens.cmp(&a.tokens));
        }
        out.push(FixtureBreakdownDay {
            day: date.format("%Y-%m-%d").to_string(),
            slices: if slices.is_empty() { None } else { Some(slices) },
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TODAY: NaiveDate = NaiveDate::from_ymd_opt(2026, 9, 2).unwrap();

    #[test]
    fn deterministic() {
        let d = NaiveDate::from_ymd_opt(2026, 8, 14).unwrap();
        assert_eq!(agent_model_tokens(d, 2, 1), agent_model_tokens(d, 2, 1));
    }

    #[test]
    fn conservation_agent_vs_model_rows() {
        let month = "2026-08";
        let agents = month_rows(month, "agent", "total", TODAY).unwrap();
        let models = month_rows(month, "model", "total", TODAY).unwrap();
        let dim = agents[0].values.len();
        for d in 0..dim {
            let sa: i64 = agents.iter().filter_map(|r| r.values[d]).sum();
            let sm: i64 = models.iter().filter_map(|r| r.values[d]).sum();
            assert_eq!(sa, sm, "day index {} agent-sum != model-sum", d);
        }
        let sum_agents: i64 = agents.iter().map(|r| r.month_total).sum();
        let sum_models: i64 = models.iter().map(|r| r.month_total).sum();
        assert_eq!(sum_agents, sum_models);
    }

    #[test]
    fn io_splits_conserve_total() {
        let month = "2026-08";
        let total = month_rows(month, "agent", "total", TODAY).unwrap();
        let input = month_rows(month, "agent", "input", TODAY).unwrap();
        let output = month_rows(month, "agent", "output", TODAY).unwrap();
        let dim = total[0].values.len();
        for d in 0..dim {
            for ((rt, ri), ro) in total.iter().zip(input.iter()).zip(output.iter()) {
                let (t, i, o) = (rt.values[d], ri.values[d], ro.values[d]);
                if t.is_none() {
                    assert!(i.is_none() && o.is_none());
                } else {
                    assert_eq!(i.unwrap() + o.unwrap(), t.unwrap(), "input+output != total");
                }
            }
        }
    }

    #[test]
    fn null_semantics() {
        // 当前月：今天之前 Some，之后 None
        let rows = month_rows("2026-09", "agent", "total", TODAY).unwrap();
        assert!(rows[0].values[0].is_some(), "9月1日应为 Some");
        assert!(rows[0].values[1].is_some(), "9月2日(今天)应为 Some");
        assert!(rows[0].values[2].is_none(), "9月3日(未来)应为 None");
        // 早于 HISTORY_START 的月份：全部 Some(0)
        let old = month_rows("2025-06", "agent", "total", TODAY).unwrap();
        assert!(old[0].values.iter().all(|v| *v == Some(0)));
        // 纯未来月份：全部 None
        let future = month_rows("2027-01", "agent", "total", TODAY).unwrap();
        assert!(future[0].values.iter().all(|v| v.is_none()));
    }

    #[test]
    fn breakdown_conserves_cell() {
        let month = "2026-08";
        let agents = month_rows(month, "agent", "total", TODAY).unwrap();
        let zcode = agents.iter().find(|r| r.key == "zcode").unwrap();
        let bd = breakdown("agent", "zcode", month, TODAY).unwrap();
        assert_eq!(bd.len(), 31);
        for (i, day) in bd.iter().enumerate() {
            let cell = zcode.values[i].expect("过去日期应为 Some");
            let slice_sum: i64 =
                day.slices.as_ref().map(|s| s.iter().map(|x| x.tokens).sum()).unwrap_or(0);
            assert_eq!(cell, slice_sum, "day {} cell != slices sum", day.day);
        }
    }

    #[test]
    fn days_in_month_correct() {
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 9), 30);
        assert_eq!(days_in_month(2026, 12), 31);
    }

    #[test]
    fn weekday_distribution_exists() {
        // 活跃模式应有真实波动：全月不应全为 0，也应存在周末/休息日零值
        let rows = month_rows("2026-08", "agent", "total", TODAY).unwrap();
        let z = rows.iter().find(|r| r.key == "zcode").unwrap();
        let non_zero = z.values.iter().filter(|v| v.is_some_and(|x| x > 0)).count();
        assert!(non_zero > 15, "活跃天数过少: {}", non_zero);
        assert!(non_zero < 31, "应存在零值日");
    }
}
