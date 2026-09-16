//! 桌面窗口聚焦：亮起的项目行 / 条上项目名点击 → 把该会话所在 agent 的
//! 桌面窗口带到前台。
//!
//! 解析链：**agent（+ host）→ 进程映像名（登记表 `HOST_PROCESSES`）→ 该进程的可见顶层窗口
//! → `SetForegroundWindow`**。按进程找、不按标题猜;标题只在同一进程多窗口（VS Code / IDE 多开,
//! 一窗一项目）时用项目目录尾段消歧。
//!
//! - 找到 → 前置（最小化先还原）并**顺手确认**该会话的等待（「聚焦即确认」）;前台锁定下系统只让
//!   任务栏图标闪、不真正切前台,本场景可接受（弱提示,不抢焦点),不做 `AttachThreadInput` 强切。
//! - 找不到 → 返回 `found=false` 并把该会话条目从注意力表**移除**：agent 已关但会话文件停在答完
//!   之后,是伪等待;源再写入时条目自动重建。前端据此把项目行降级为灰色「上次停在这里」,
//!   点击回退打开目录。
//! - 只读红线：枚举窗口 + 查映像名,不注入、不发消息、不 kill;进程名是本文件常量,
//!   不拼接用户输入。
//!
//! 登记表取值按本机安装（任务管理器 / 卸载表 / 安装目录）；Claude Code 是唯一有两种
//! 桌面宿主的 agent,靠 jsonl 行内 `entrypoint`（`claude-desktop` / `claude-vscode`）区分,采集器随
//! `TurnState.host` 带出（serde default,不动 schema）。

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::collector::{self, store::now_millis};
use crate::AppState;

/// 一个 agent（+ 宿主）的目标进程：映像名候选按优先级排列（大小写不敏感）,`by_title` = 同进程多窗口时
/// 用项目目录尾段消歧（IDE 多开一窗一项目）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostTarget {
    pub exes: &'static [&'static str],
    pub by_title: bool,
}

/// 登记表（一处可改）。`host` = `TurnState.host`（Claude Code 的 `entrypoint`;其余源 None）。
/// Claude Code 宿主未知（旧游标 / 缺字段）时按「桌面应用 → VS Code」顺序都试。
/// 注意 Claude Code CLI 自身也叫 `claude.exe`,但它没有可见顶层窗口,枚举天然排除。
pub fn resolve_target(agent: &str, host: Option<&str>) -> Option<HostTarget> {
    const CLAUDE_DESKTOP: &[&str] = &["claude.exe"];
    const CLAUDE_VSCODE: &[&str] = &["code.exe"];
    const CLAUDE_ANY: &[&str] = &["claude.exe", "code.exe"];
    const CODEX: &[&str] = &["codex.exe", "chatgpt.exe"];
    const DSH: &[&str] = &["deepseek harness.exe"];
    const ZCODE: &[&str] = &["zcode.exe"];
    const CODEBUDDY: &[&str] = &["codebuddy cn.exe", "codebuddy.exe"];
    const WORKBUDDY: &[&str] = &["workbuddy.exe"];
    let t = match (agent, host) {
        ("claude-code", Some("claude-desktop")) => HostTarget { exes: CLAUDE_DESKTOP, by_title: false },
        ("claude-code", Some("claude-vscode")) => HostTarget { exes: CLAUDE_VSCODE, by_title: true },
        ("claude-code", _) => HostTarget { exes: CLAUDE_ANY, by_title: true },
        ("codex", _) => HostTarget { exes: CODEX, by_title: false },
        ("dsh", _) => HostTarget { exes: DSH, by_title: false },
        ("zcode", _) => HostTarget { exes: ZCODE, by_title: false },
        ("codebuddy", _) => HostTarget { exes: CODEBUDDY, by_title: true },
        ("workbuddy", _) => HostTarget { exes: WORKBUDDY, by_title: true },
        _ => return None,
    };
    Some(t)
}

/// 项目目录尾段（小写;消歧用）。`e:/projects/tokencalendar` → `tokencalendar`;Scratch / 非路径键 → None。
pub fn dir_tail(project_key: &str) -> Option<String> {
    let tail = project_key.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next()?.trim();
    (!tail.is_empty() && !tail.ends_with(':')).then(|| tail.to_lowercase())
}

/// 同一进程的候选窗口里选一个：0 个 → None;1 个 → 它;多个 → `by_title` 且标题含目录尾段的第一个,
/// 否则第一个（前提「一窗一项目」不成立时至少把 agent 带到前面）。
pub fn pick_window(titles: &[String], tail: Option<&str>, by_title: bool) -> Option<usize> {
    match titles.len() {
        0 => None,
        1 => Some(0),
        _ => {
            if by_title {
                if let Some(tail) = tail {
                    if let Some(i) = titles.iter().position(|t| t.to_lowercase().contains(tail)) {
                        return Some(i);
                    }
                }
            }
            Some(0)
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct FocusResult {
    pub found: bool,
}

/// 聚焦一个等待中会话的 agent 窗口。找到 → 前置 + 确认;找不到 → 移除条目（伪等待）。
/// 表锁不跨 Win32 枚举持有。
#[tauri::command(rename_all = "snake_case")]
pub fn focus_agent_window(agent: String, session_id: String, app: AppHandle, state: State<'_, AppState>) -> Result<FocusResult, String> {
    let looked = state.attention.lock().map_err(|_| "attention lock poisoned".to_string())?.lookup(&agent, &session_id);
    let Some((host, project_key)) = looked else {
        // 点击与派生之间条目已被剔除:不动表,让前端回退
        return Ok(FocusResult { found: false });
    };
    let target = resolve_target(&agent, host.as_deref());
    let found = match target {
        Some(t) => platform::focus(t, dir_tail(&project_key).as_deref()),
        None => false,
    };
    crate::dev_log!("[focus] agent={} host={:?} found={}", agent, host, found);
    let now = now_millis();
    let idle = crate::collector::task_store::idle_threshold_ms();
    let changed = {
        let mut table = state.attention.lock().map_err(|_| "attention lock poisoned".to_string())?;
        if found {
            table.ack(&agent, &session_id, now, idle)
        } else {
            table.remove(&agent, &session_id, now, idle)
        }
    };
    if changed {
        collector::notify_attention(&app);
    }
    Ok(FocusResult { found })
}

#[cfg(windows)]
mod platform {
    use std::collections::HashMap;
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM};
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetAncestor, GetWindowLongPtrW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
        SetForegroundWindow, ShowWindow, GA_ROOTOWNER, GWL_EXSTYLE, SW_RESTORE, WS_EX_TOOLWINDOW,
    };

    use super::{pick_window, HostTarget};

    struct Win {
        hwnd: HWND,
        pid: u32,
        title: String,
    }

    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = &mut *(lparam as *mut Vec<Win>);
        if IsWindowVisible(hwnd) == 0 || GetAncestor(hwnd, GA_ROOTOWNER) != hwnd {
            return 1;
        }
        if (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW != 0 {
            return 1;
        }
        let mut cloaked: u32 = 0;
        if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as *mut c_void, 4) == 0 && cloaked != 0 {
            return 1;
        }
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return 1;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return 1;
        }
        out.push(Win { hwnd, pid, title: String::from_utf16_lossy(&buf[..n as usize]) });
        1
    }

    /// 顶层可见、非 cloaked、非工具窗、有标题的窗口。
    fn top_windows() -> Vec<Win> {
        let mut out: Vec<Win> = Vec::new();
        unsafe {
            EnumWindows(Some(enum_cb), &mut out as *mut Vec<Win> as LPARAM);
        }
        out
    }

    /// 进程映像文件名（小写,不含路径);打不开（权限 / 已退出）→ None。
    fn image_name(pid: u32) -> Option<String> {
        unsafe {
            let h: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(h);
            if ok == 0 {
                return None;
            }
            let full = String::from_utf16_lossy(&buf[..len as usize]);
            full.rsplit(['\\', '/']).next().map(|s| s.to_lowercase())
        }
    }

    pub fn focus(target: HostTarget, tail: Option<&str>) -> bool {
        let wins = top_windows();
        let mut names: HashMap<u32, Option<String>> = HashMap::new();
        let mut by_exe: Vec<Vec<&Win>> = vec![Vec::new(); target.exes.len()];
        for w in &wins {
            let name = names.entry(w.pid).or_insert_with(|| image_name(w.pid));
            if let Some(name) = name.as_deref() {
                if let Some(i) = target.exes.iter().position(|e| *e == name) {
                    by_exe[i].push(w);
                }
            }
        }
        for group in by_exe {
            let titles: Vec<String> = group.iter().map(|w| w.title.clone()).collect();
            if let Some(i) = pick_window(&titles, tail, target.by_title) {
                let hwnd = group[i].hwnd;
                unsafe {
                    if IsIconic(hwnd) != 0 {
                        ShowWindow(hwnd, SW_RESTORE);
                    }
                    // 前台锁定下失败只会闪任务栏图标,仍算找到
                    SetForegroundWindow(hwnd);
                }
                return true;
            }
        }
        false
    }
}

#[cfg(not(windows))]
mod platform {
    use super::HostTarget;

    pub fn focus(_target: HostTarget, _tail: Option<&str>) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_six_sources_and_claude_hosts() {
        assert_eq!(resolve_target("claude-code", Some("claude-desktop")), Some(HostTarget { exes: &["claude.exe"], by_title: false }));
        assert_eq!(resolve_target("claude-code", Some("claude-vscode")), Some(HostTarget { exes: &["code.exe"], by_title: true }));
        assert_eq!(resolve_target("claude-code", None).map(|t| t.exes), Some(&["claude.exe", "code.exe"][..]), "宿主未知两种都试");
        for a in ["codex", "dsh", "zcode", "codebuddy", "workbuddy"] {
            let t = resolve_target(a, None).unwrap_or_else(|| panic!("{a} 未登记"));
            assert!(!t.exes.is_empty());
            assert!(t.exes.iter().all(|e| e.ends_with(".exe") && *e == e.to_lowercase()), "映像名小写带扩展名: {a}");
        }
        assert!(resolve_target("codebuddy", None).unwrap().by_title && resolve_target("workbuddy", None).unwrap().by_title, "IDE 多开按标题消歧");
        assert_eq!(resolve_target("unknown-agent", None), None);
    }

    #[test]
    fn dir_tail_takes_last_segment() {
        assert_eq!(dir_tail("e:/projects/tokencalendar"), Some("tokencalendar".into()));
        assert_eq!(dir_tail("E:\\Work\\TokenCalendar\\"), Some("tokencalendar".into()));
        assert_eq!(dir_tail("/home/u/work"), Some("work".into()));
        assert_eq!(dir_tail("e:"), None, "盘根无尾段");
        assert_eq!(dir_tail(""), None);
    }

    #[test]
    fn pick_window_rules() {
        let titles = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(pick_window(&[], Some("x"), true), None);
        assert_eq!(pick_window(&titles(&["Claude"]), Some("other"), false), Some(0), "单窗口不看标题");
        let vs = titles(&["main.rs - OtherProj - Visual Studio Code", "AGENTS.md - TokenCalendar - Visual Studio Code"]);
        assert_eq!(pick_window(&vs, Some("tokencalendar"), true), Some(1), "多窗口按目录尾段消歧（大小写不敏感）");
        assert_eq!(pick_window(&vs, Some("nomatch"), true), Some(0), "无命中退回第一个");
        assert_eq!(pick_window(&vs, None, true), Some(0));
        assert_eq!(pick_window(&vs, Some("tokencalendar"), false), Some(0), "不按标题消歧的 agent 取第一个");
    }
}
