//! 真实六源数据 smoke（仅本机手动验证,`cargo test -- --ignored` 触发）。
//!
//! 只读扫描本机全部源 → 内存库聚合 → 断言产出与守恒。不触碰真实 collector.db
//! （内存库游标与聚合都随进程消失）,不写任何用户目录。
//! 设环境变量 `TC_SMOKE_DB=<文件路径>` 时改写入该文件（先删旧文件）,供事后只读 SQL 复核;
//! 路径由调用方指定（放临时目录）,同样不触碰真实 collector.db。

use chrono::Datelike;

use super::store::Store;
use super::{default_adapters, Adapter};


/// Codex 两种轮信号同日数量级对比。只读逐行扫 `~/.codex` 的事件类型与
/// session_meta.parent_thread_id,不读正文;再跑一遍真实适配器,核对入库轮次 == 主会话
/// task_started（按 token_count 消费,允许无回合轮差异）。
#[test]
#[ignore]
fn codex_turn_signal_compare() {
    use std::collections::BTreeMap;
    let base = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| super::home_dir().map(|h| h.join(".codex")))
        .expect("codex home");
    let mut files = Vec::new();
    super::jsonl::discover(&base.join("sessions"), true, &mut files);
    super::jsonl::discover(&base.join("archived_sessions"), true, &mut files);

    // day → [user_message, task_started（main), task_started（sub)]
    let mut per_day: BTreeMap<String, [i64; 3]> = BTreeMap::new();
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        let mut subagent = false;
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if v.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
                subagent = v.pointer("/payload/parent_thread_id").and_then(|x| x.as_str()).map_or(false, |x| !x.is_empty());
                continue;
            }
            let slot = match v.pointer("/payload/type").and_then(|t| t.as_str()) {
                Some("user_message") => 0,
                Some("task_started") if !subagent => 1,
                Some("task_started") => 2,
                _ => continue,
            };
            let Some((day, _)) = v.get("timestamp").and_then(|t| t.as_str()).and_then(super::rfc3339_to_local_day_hour) else { continue };
            per_day.entry(day).or_default()[slot] += 1;
        }
    }

    let mut store = Store::open_in_memory().expect("in-memory store");
    let adapter = super::codex::CodexAdapter::new();
    let _ = adapter.collect(&mut store);

    let mut out = format!("=== Codex 轮信号对比（{} 个 rollout）===\nday         user_message  task_started(main)  task_started(sub)  stored_turns\n", files.len());
    let today = chrono::Local::now().date_naive();
    let mut totals = [0i64; 4];
    let mut month_turns: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for (day, c) in &per_day {
        let month = day[..7].to_string();
        let counts = month_turns.entry(month.clone()).or_insert_with(|| {
            store
                .month_rows(&month, "agent", "total", today)
                .unwrap_or_default()
                .into_iter()
                .find(|r| r.key == "codex")
                .map(|r| r.message_counts)
                .unwrap_or_default()
        });
        let d: usize = day[8..].parse().unwrap_or(1);
        let stored = counts.get(d - 1).copied().unwrap_or(0);
        out.push_str(&format!("{day}  {:>12}  {:>18}  {:>17}  {:>12}\n", c[0], c[1], c[2], stored));
        for i in 0..3 {
            totals[i] += c[i];
        }
        totals[3] += stored;
    }
    out.push_str(&format!("合计        {:>12}  {:>18}  {:>17}  {:>12}\n", totals[0], totals[1], totals[2], totals[3]));
    println!("{out}");
    assert!(totals[1] >= totals[3], "入库轮次不应超过主会话 task_started");
}

#[test]
#[ignore]
fn real_sources_smoke() {
    let mut store = match std::env::var_os("TC_SMOKE_DB") {
        Some(path) => {
            let path = std::path::PathBuf::from(path);
            for suffix in ["", "-wal", "-shm"] {
                let mut p = path.clone().into_os_string();
                p.push(suffix);
                let _ = std::fs::remove_file(p);
            }
            Store::open(&path).expect("smoke file store")
        }
        None => Store::open_in_memory().expect("in-memory store"),
    };
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
    // cache 两列 + 小时表守恒（hourly 日合计 == daily,六源全部走 hour 路径）
    for (agent, cr, cw, hourly, daily) in store.cache_totals(&month) {
        summary.push_str(&format!(
            "  cache {:<12} read={:>12} write={:>10} | hourly Σ={} daily Σ={} 守恒={}\n",
            agent, cr, cw, hourly, daily, hourly == daily
        ));
        assert_eq!(hourly, daily, "{agent} 小时表按日合计须等于日表");
    }
    // 任务层（六源 turn 非空 / 时间段非负 / 子会话不进任务列表 / 项目维守恒）
    for adapter in default_adapters() {
        let id = adapter.meta().id;
        let (raw, merged, negative, children, tasks) = store.test_task_stats(id);
        let turns = store.test_turns(id);
        let calls: i64 = turns.iter().map(|t| t.model_calls).sum();
        let tools: i64 = turns.iter().map(|t| t.tool_calls).sum();
        let sub_calls: i64 = turns.iter().map(|t| t.subagent_calls).sum();
        let aborted = turns.iter().filter(|t| t.aborted).count();
        let wall: i64 = turns.iter().filter_map(|t| t.wall_ms).sum();
        let titled = store.test_sessions(id).iter().filter(|s| s.title.is_some()).count();
        summary.push_str(&format!(
            "  task  {:<12} raw_turns={:>5} turns={:>5} tasks={:>4} child_sessions={:>4} model_calls={:>6} tool_calls={:>6} subagent_calls={:>5} aborted={:>4} Σwall={:>6}min titled={:>3} negative={}\n",
            id, raw, merged, tasks, children, calls, tools, sub_calls, aborted, wall / 60_000, titled, negative
        ));
        let (ab, err_turns, err_sum, both) = store.test_abort_error_stats(id);
        summary.push_str(&format!(
            "  abort {:<12} aborted_turns={ab} error_turns={err_turns} Σerror={err_sum} aborted∧error={both}\n",
            id
        ));
        assert!(raw > 0 && merged > 0, "{id} turn 表应非空");
        assert_eq!(negative, 0, "{id} 时间段不得为负");
        assert_eq!(store.test_child_sessions_in_tasks(id), 0, "{id} 子会话不得单独出现在任务列表");
    }
    // 当月与全量的 request_count / 物化轮 / 零调用轮 / Σerror 对比
    for id in ["claude-code", "zcode"] {
        let (rc, rows, zero, errors) = store.test_month_turn_stats(id, &month);
        let (rc_all, rows_all, zero_all, errors_all) = store.test_month_turn_stats(id, "");
        summary.push_str(&format!(
            "  turns {id:<12} {month}: request_count={rc} turn_rows={rows} zero_call={zero} errors={errors} | 全量: request_count={rc_all} turn_rows={rows_all} zero_call={zero_all} errors={errors_all}\n"
        ));
    }
    let (_, _, _, codex_children, _) = store.test_task_stats("codex");
    assert!(codex_children > 0, "codex 子代理会话应落 session 表且 parent_id 非空");
    // 阈值全表重算耗时 + 直方图 within 与 daily_project.idle_ms 守恒（真实数据）
    for minutes in [60i64, 30] {
        let ms = minutes * 60_000;
        let started = std::time::Instant::now();
        let days = store.recompute_projects(ms).expect("recompute");
        let elapsed = started.elapsed().as_millis();
        let h = store.gap_histogram("2000-01-01", "2099-12-31", ms, &super::project_meta::ScratchRule::OFF).expect("histogram");
        let rows = store.project_month_rows(&month, "project", "human", today, &super::project_meta::ScratchRule::OFF).unwrap_or_default();
        let effort = store.effort_series("2000-01-01", "2099-12-31", "day", "total", "human", None, &super::project_meta::ScratchRule::OFF).expect("effort");
        let idle_all: i64 = effort.points.iter().map(|p| p.values[0]).sum();
        summary.push_str(&format!(
            "  idle  threshold={minutes}min recompute {days} day(s) in {elapsed}ms | gaps={} within={} ({}min) beyond={} | 当月项目数={} Σidle(全量)={}min\n",
            h.total, h.within_count, h.within_ms / 60_000, h.beyond_count, rows.len(), idle_all / 60_000
        ));
        assert_eq!(h.within_ms, idle_all, "直方图 within 须等于 daily_project.idle_ms 全量和");
    }
    let tasks = store.task_list(0, i64::MAX, &super::task_query::TaskFilters::default(), &super::task_query::TaskSort::default(), &super::task_query::TaskPageReq::default(), &super::project_meta::ScratchRule::OFF).expect("tasks");
    summary.push_str(&format!("  tasks total={} first_page={}\n", tasks.total, tasks.rows.len()));
    // 默认折叠规则下的项目分布（只读 list_project_meta,无 meta 行时 = 纯规则效果）
    let rule = super::project_meta::ScratchRule::DEFAULT;
    let list = store.list_project_meta(&rule).expect("project meta");
    let count = |st: &str| list.rows.iter().filter(|r| r.status == st).count();
    let default_rows = store.project_month_rows(&month, "project", "total", today, &rule).unwrap_or_default();
    let default_sum: i64 = default_rows.iter().map(|r| r.month_total).sum();
    let raw_sum: i64 = store.project_month_rows(&month, "project", "total", today, &super::project_meta::ScratchRule::OFF).unwrap_or_default().iter().map(|r| r.month_total).sum();
    summary.push_str(&format!(
        "  projects keys={} active={} scratch={} hidden={} merged={} | 当月项目行 default={} Σ={} vs 规则关 Σ={}
",
        list.rows.len(), count("active"), count("scratch"), count("hidden"), count("merged"), default_rows.len(), default_sum, raw_sum
    ));
    let mismatches = store.test_project_conservation();
    summary.push_str(&format!("项目维守恒不一致格数 = {}\n", mismatches.len()));
    for m in mismatches.iter().take(10) {
        summary.push_str(&format!("  ✗ {m}\n"));
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
    assert!(mismatches.is_empty(), "daily_project 折叠项目后须与 daily_usage 逐格守恒");
    assert_eq!(sum_agents, sum_models, "agent/model 行和必须守恒");
    assert!(sum_agents > 0, "本机四源应有真实数据");
}

/// 真实六源注意力派生（只读扫描 → 内存库 → 注意力表）,打印会话级状态核对各源信号。
/// 只打 agent / 状态 / 距今分钟 / 项目尾段,不打标题。
#[test]
#[ignore]
fn attention_live_smoke() {
    use super::attention::AttentionTable;
    let mut store = Store::open_in_memory().expect("in-memory store");
    let mut table = AttentionTable::default();
    for adapter in default_adapters() {
        let _ = adapter.collect(&mut store);
        table.apply(store.take_live());
    }
    let now = super::store::now_millis();
    let idle = 24 * 3_600_000; // 放宽到 24h,看到更多样本
    table.prune(now, idle);
    let mut out = String::from("=== attention (last 24h) ===\n");
    for it in table.items(now, idle) {
        let tail = it.project_key.rsplit('/').next().unwrap_or("");
        out.push_str(&format!("{:<12} {:<12} {:>5}m  {}\n", it.agent, it.state, (now - it.since) / 60_000, tail));
    }
    println!("{out}");
}

/// 标题就地升级:对安装版 collector.db 的**副本**（TC_TITLE_DB）跑真实 Codex / DSH 源,
/// 打印两源已有会话的标题覆盖率前后对比 + 近 7 天会话样本。
#[test]
#[ignore]
fn title_upgrade_on_db_copy() {
    let path = std::env::var_os("TC_TITLE_DB").expect("TC_TITLE_DB = collector.db 副本路径");
    let mut store = Store::open(std::path::Path::new(&path)).expect("open copy");
    let coverage = |store: &Store| -> Vec<(String, i64, i64)> {
        let mut stmt = store
            .conn()
            .prepare("SELECT agent_key, COUNT(*), COUNT(title) FROM session WHERE agent_key IN ('codex','dsh') AND parent_id IS NULL GROUP BY 1")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().flatten().collect()
    };
    let before = coverage(&store);
    let mut out = format!("before (agent, root sessions, titled) = {before:?}\n");
    for adapter in default_adapters().into_iter().filter(|a| matches!(a.meta().id, "codex" | "dsh")) {
        let r = adapter.collect(&mut store);
        out.push_str(&format!("[{}] collect ok={}\n", adapter.meta().id, r.is_ok()));
    }
    out.push_str(&format!("after  (agent, root sessions, titled) = {:?}\n", coverage(&store)));
    let since = chrono::Local::now().timestamp_millis() - 7 * 86_400_000;
    let mut stmt = store
        .conn()
        .prepare(
            "SELECT agent_key, project_key, datetime(started_at/1000,'unixepoch','localtime'), COALESCE(title,'<NULL>')
             FROM session WHERE agent_key IN ('codex','dsh') AND parent_id IS NULL AND ended_at >= ?1 ORDER BY started_at",
        )
        .unwrap();
    for row in stmt
        .query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))
        .unwrap()
        .flatten()
    {
        let tail = row.1.rsplit('/').next().unwrap_or_default().to_string();
        out.push_str(&format!("  {:<5} {:<14} {} {}\n", row.0, tail, row.2, row.3));
    }
    // 看板实际拿到的格子（与 get_project_timeline 同一查询）
    let today = chrono::Local::now().date_naive();
    let from = (today - chrono::Duration::days(7)).format("%Y-%m-%d").to_string();
    let to = today.format("%Y-%m-%d").to_string();
    let tl = store.project_timeline(&from, &to, today, &super::project_meta::ScratchRule::DEFAULT).expect("timeline");
    for p in &tl.projects {
        for c in &p.cells {
            for it in c.items.iter().filter(|it| matches!(it.agent_key.as_str(), "codex" | "dsh")) {
                out.push_str(&format!("  cell {:<14} {} {:<5} {}\n", p.label, c.day, it.agent_key, it.title.as_deref().unwrap_or("<NULL → HH:mm>")));
            }
        }
    }
    println!("{out}");
}
