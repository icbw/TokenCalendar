//! 真库副本双轨检查（手动：`cargo test alignment_real_db_copy_smoke -- --ignored --nocapture`）。
//! 真库只作为复制源；open 可能触发的迁移 / 备份也只发生在副本。

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::alignment::rebuild_5h;
use super::model::Platform;
use super::price;
use super::store::SubStore;
use crate::collector::project_meta::scratch_rule;
use crate::collector::store::Store;

fn real_file(env_name: &str, file_name: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(env_name) {
        return Some(PathBuf::from(path));
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/debug/data")
        .join(file_name);
    path.exists().then_some(path)
}

fn clone_db(source: &Path, out: &Path) -> PathBuf {
    let destination = out.join(source.file_name().expect("database file name"));
    for suffix in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", source.display()));
        if from.exists() {
            std::fs::copy(&from, format!("{}{suffix}", destination.display()))
                .expect("clone database");
        }
    }
    destination
}

#[test]
#[ignore]
fn alignment_real_db_copy_smoke() {
    let Some(sub_source) = real_file("TC_SUB_DB", "subscriptions.db") else {
        eprintln!("没有找到 subscriptions.db，跳过（设 TC_SUB_DB=<路径>）");
        return;
    };
    let Some(collector_source) = real_file("TC_COLLECTOR_DB", "collector.db") else {
        eprintln!("没有找到 collector.db，跳过（设 TC_COLLECTOR_DB=<路径>）");
        return;
    };
    let out = std::env::var_os("TC_ALIGNMENT_SMOKE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "tokencalendar-alignment-smoke-{}",
                std::process::id()
            ))
        });
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("create smoke directory");
    let sub = SubStore::open(&clone_db(&sub_source, &out)).expect("open subscriptions copy");
    price::load_from(&sub);
    let collector = Store::open(&clone_db(&collector_source, &out)).expect("open collector copy");

    for platform in [Platform::Codex, Platform::Claude] {
        let Some(latest) = sub.quota_reading_before(platform, "5h", i64::MAX) else {
            println!("{platform:?}: 没有 5h 读数，跳过");
            continue;
        };
        let to = latest.t;
        let from = to - super::alignment::FIVE_HOURS_SECS;
        let started = Instant::now();
        let quota = rebuild_5h(
            platform,
            sub.quota_reading_before(platform, "5h", from),
            sub.quota_readings(platform, "5h", from, to),
            sub.quota_reading_after(platform, "5h", to),
            from,
            to,
        );
        let quota_elapsed = started.elapsed();
        let started = Instant::now();
        let tasks = collector
            .task_alignment(platform, from * 1000, to * 1000, None, &scratch_rule())
            .expect("alignment query on collector copy");
        let task_elapsed = started.elapsed();
        println!(
            "{platform:?}: latest={} readings={}/{} windows={} discontinuities={} tasks={}/{} turns={} truncated={} quota={:?} tasks_time={:?}",
            latest.t,
            quota.returned_readings,
            quota.raw_readings,
            quota.windows.len(),
            quota.discontinuities.len(),
            tasks.returned_tasks,
            tasks.total_tasks,
            tasks.returned_turns,
            tasks.truncated,
            quota_elapsed,
            task_elapsed,
        );
        assert!(quota.returned_readings <= quota.raw_readings);
        assert!(tasks.returned_tasks <= 50);
        assert!(tasks.returned_turns <= 5000);
    }
    if std::env::var_os("TC_ALIGNMENT_SMOKE_OUT").is_none() {
        let _ = std::fs::remove_dir_all(&out);
    }
}
