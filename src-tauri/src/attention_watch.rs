//! 注意力自动退出：按需醒着的守护线程。
//!
//! **没有未确认提示时挂起（`park`,零唤醒）**;采集线程每轮派生后发现有未确认提示才 `unpark`,
//! 提示全部确认 / 熄灭后下一拍重新挂起（判据 `AttentionTable:watch_needs`）。醒着时：
//! - **前台采样**（有未确认 waiting 时 1s 一拍）:前台根窗口 + 映像名（同窗口缓存）+ 标题;键鼠闲置
//!   ≥ `INPUT_IDLE_MS` 视为人不在,喂 None。进前台时刻来自 `agent_focus` 的前台切换事件钩子
//!   （只在切换窗口时回调,不轮询）,所以挂起期间切窗口也不丢「什么时候进的前台」。
//! - **快速探针**（每 3s）：未确认提示的源文件 mtime 与采集时不同 → `collector:wake` 提前跑一轮增量
//!   采集。同一 mtime 只唤醒一次,防「文件变了但没有新整行」时反复空转。只剩 tool_pending 时只探针、3s 一拍。
//! - 判据全在 `AttentionTable`（可单测）,这里只采样与广播。只读红线同 `agent_focus`;表锁不跨 Win32 调用持有。

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::collector::{self, attention::ForegroundWindow, attention::INPUT_IDLE_MS, jsonl, store::now_millis, task_store};
use crate::AppState;

const FG_TICK: Duration = Duration::from_secs(1);
const PROBE_TICK: Duration = Duration::from_secs(3);
const PROBE_EVERY_FG_TICKS: u32 = 3;

static WATCHER: OnceLock<std::thread::Thread> = OnceLock::new();

pub fn spawn(app: AppHandle) {
    crate::agent_focus::spawn_foreground_hook();
    if let Ok(handle) = std::thread::Builder::new().name("attention-watch".into()).spawn(move || run(app)) {
        let _ = WATCHER.set(handle.thread().clone());
    }
}

/// 唤醒挂起中的守护线程（醒着时调用无副作用）。
pub fn unpark() {
    if let Some(t) = WATCHER.get() {
        t.unpark();
    }
}

/// 采样前台窗口;人不在（键鼠闲置 ≥ `INPUT_IDLE_MS`）→ None。
pub fn sample() -> Option<ForegroundWindow> {
    let present = crate::agent_focus::input_idle_ms().map_or(true, |ms| ms < INPUT_IDLE_MS);
    present.then(crate::agent_focus::foreground_window).flatten()
}

fn run(app: AppHandle) {
    let mut woken: HashMap<String, i64> = HashMap::new();
    let mut n: u32 = 0;
    loop {
        let Some(state) = app.try_state::<AppState>() else {
            std::thread::park();
            continue;
        };
        let needs = match state.attention.lock() {
            Ok(table) => table.watch_needs(now_millis(), task_store::idle_threshold_ms()),
            Err(_) => return,
        };
        let (need_fg, need_probe) = needs;
        if !need_fg && !need_probe {
            woken.clear();
            std::thread::park();
            continue;
        }
        std::thread::sleep(if need_fg { FG_TICK } else { PROBE_TICK });
        n = n.wrapping_add(1);

        if need_fg {
            let fg = sample();
            let (now, idle) = (now_millis(), task_store::idle_threshold_ms());
            let changed = match state.attention.lock() {
                Ok(mut table) => table.set_foreground(fg, now, idle),
                Err(_) => return,
            };
            if changed {
                collector::notify_attention(&app);
            }
        }
        if !need_probe || (need_fg && n % PROBE_EVERY_FG_TICKS != 0) {
            continue;
        }
        let watch = match state.attention.lock() {
            Ok(table) => table.watch_list(now_millis(), task_store::idle_threshold_ms()),
            Err(_) => return,
        };
        woken.retain(|p, _| watch.iter().any(|(w, _)| w == p));
        let mut wake = false;
        for (path, base) in watch {
            let Some((_, mtime)) = jsonl::generation(Path::new(&path)) else { continue };
            if mtime != base && woken.get(&path) != Some(&mtime) {
                woken.insert(path, mtime);
                wake = true;
            }
        }
        if wake {
            collector::wake();
        }
    }
}
