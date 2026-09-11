//! 真实六源数据 smoke（仅本机手动验证,`cargo test -- --ignored` 触发）。
//!
//! 只读扫描本机全部源 → 内存库聚合 → 断言产出与守恒。不触碰真实 collector.db
//! （内存库游标与聚合都随进程消失）,不写任何用户目录。

use chrono::Datelike;

use super::store::Store;
use super::default_adapters;


#[test]
#[ignore]
fn real_sources_smoke() {
    let mut store = Store::open_in_memory().expect("in-memory store");
    let mut summary = String::from("=== 真实六源 smoke ===\n");

    for adapter in default_adapters() {
        let meta = adapter.meta();
        let probe = adapter.probe();
        summary.push_str(&format!(
            "[{}] probe={} fp={:?}\n",
            meta.id, probe.status, probe.fingerprint
        ));

        match adapter.collect(&mut store) {
            Ok(outcome) => {
                summary.push_str(&format!(
                    "[{}] events={} months={:?}\n",
                    meta.id,
                    outcome.events,
                    outcome.months.iter().collect::<Vec<_>>()
                ));
                // 有数据的源必须产出月份
                if outcome.events > 0 {
                    assert!(!outcome.months.is_empty(), "{} events but no months", meta.id);
                }
            }
            Err(e) => summary.push_str(&format!("[{}] collect error {}: {}\n", meta.id, e.code, e.message)),
        }
    }

    // 聚合结果抽查：当前月矩阵与守恒
    let today = chrono::Local::now().date_naive();
    let month = today.format("%Y-%m").to_string();
    let agents = store.month_rows(&month, "agent", "total", today).unwrap_or_default();
    let models = store.month_rows(&month, "model", "total", today).unwrap_or_default();
    let sum_agents: i64 = agents.iter().map(|r| r.month_total).sum();
    let sum_models: i64 = models.iter().map(|r| r.month_total).sum();
    summary.push_str(&format!(
        "当月({month}) agent 行数={} Σ={} | model 行数={} Σ={} | 守恒={}\n",
        agents.len(),
        sum_agents,
        models.len(),
        sum_models,
        sum_agents == sum_models
    ));
    for r in &agents {
        let today_v = r.values[today.day() as usize - 1];
        let msgs: i64 = r.message_counts.iter().sum();
        summary.push_str(&format!(
            "  agent {:<12} 月计={:>12} today={:?} turns={}\n",
            r.key, r.month_total, today_v, msgs
        ));
    }
    let total_turns: i64 = agents.iter().map(|r| r.message_counts.iter().sum::<i64>()).sum();
    summary.push_str(&format!("当月对话轮总数 = {}\n", total_turns));
    assert!(total_turns > 0, "request_count 应随真实数据采集入库");
    for r in models.iter().take(8) {
        summary.push_str(&format!("  model {:<24} 月计={:>12}\n", r.key, r.month_total));
    }

    // 钻取抽查（取 agent 行首行）
    if let Some(top) = agents.first() {
        let bd = store.breakdown("agent", &top.key, &month, today).unwrap_or_default();
        let last = bd.iter().rev().find(|d| d.slices.is_some());
        summary.push_str(&format!("钻取 {} 最近有数据日 {:?}\n", top.key, last.map(|d| (&d.day, d.slices.as_ref().unwrap().len()))));
    }

    println!("{summary}");
    assert_eq!(sum_agents, sum_models, "agent/model 行和必须守恒");
    assert!(sum_agents > 0, "本机四源应有真实数据");
}
