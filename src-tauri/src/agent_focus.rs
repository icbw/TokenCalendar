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

/// 前台自动确认用的登记表（`attention` 的前台判据）。比聚焦**更保守**——认错窗口会把没看过的等待
/// 熄掉,所以宿主不确定就不参与：Claude Code 只认 `claude-desktop` / `claude-vscode`（CLI 跑在终端里,
/// 宿主未知时不猜）;Codex 只认 `codex.exe`（`chatgpt.exe` 前台多半在用 ChatGPT 本身）。
pub fn watch_target(agent: &str, host: Option<&str>) -> Option<HostTarget> {
    match (agent, host) {
        ("claude-code", Some("claude-desktop" | "claude-vscode")) => resolve_target(agent, host),
        ("claude-code", _) => None,
        ("codex", _) => Some(HostTarget { exes: &["codex.exe"], by_title: false }),
        _ => resolve_target(agent, host),
    }
}

/// 前台窗口是否是这个会话的宿主窗口：映像名命中;按标题消歧的宿主还要标题含项目目录尾段（无尾段 → 不算）。
pub fn window_matches(target: HostTarget, exe: &str, title: &str, project_key: &str) -> bool {
    target.exes.contains(&exe)
        && (!target.by_title || dir_tail(project_key).is_some_and(|tail| title_has_project(title, &tail)))
}

/// 窗口标题是否指向该项目（`tail` = `dir_tail`,已小写）。IDE 标题形如
/// `● main.rs - TokenCalendar [WSL: Ubuntu] - Visual Studio Code`：按 ` - ` / ` — ` 切段,每段去掉未保存标记
/// 与 ` [..]` / ` （..)` 后缀后**整段相等**才算。不做子串匹配——`calendar` 既不能命中 `TokenCalendar`
/// （别的项目窗口）,也不能命中 `calendar.rs`（文件名段）。
pub fn title_has_project(title: &str, tail: &str) -> bool {
    title.to_lowercase().replace(" \u{2014} ", " - ").split(" - ").any(|seg| {
        let seg = seg.trim().trim_start_matches(['\u{25cf}', '\u{2022}', '*']).trim();
        let seg = seg.split(" [").next().unwrap_or(seg);
        seg.split(" (").next().unwrap_or(seg).trim() == tail
    })
}

/// 当前前台窗口（根窗口;取不到进程映像名 → None）。
pub fn foreground_window() -> Option<crate::collector::attention::ForegroundWindow> {
    platform::foreground_window()
}

/// 安装前台切换事件钩子（独立线程阻塞在消息循环里,只在切换窗口时回调记下进前台时刻;不轮询）。
pub fn spawn_foreground_hook() {
    platform::spawn_foreground_hook()
}

/// 距最近一次键鼠输入的毫秒数（取不到 → None）。
pub fn input_idle_ms() -> Option<u64> {
    platform::input_idle_ms()
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
                    if let Some(i) = titles.iter().position(|t| title_has_project(t, tail)) {
                        return Some(i);
                    }
                }
            }
            Some(0)
        }
    }
}

// ---------- 双击会话跳转 ----------
//
// 时间轴会话条双击 → 到该会话所在的 agent：窗口在 → 前置;不在 → 启动宿主（IDE 类带项目目录）;
// 宿主找不到 → 打开项目目录。精度是「对的应用 + 对的项目窗口」,不含会话级深链接（另立项）。
//
// 启动红线：**只启动登记表内的宿主**——可执行文件名是本文件
// 常量,路径只取自系统安装登记（App Paths / 卸载表 / 已安装的应用包）或正在运行的同名窗口进程,且文件名
// 必须与登记名相等;项目目录作为**独立参数**传入（不拼命令行字符串、不经 shell）,且只取库中会话自己的
// 目录键;前端只传 agent 键 + 会话 id,不传任何路径。

/// 宿主的启动方式：IDE 类带项目目录参数（已开该目录的窗口会被宿主自己前置）;应用类只启动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchKind {
    Ide,
    App,
}

/// 应用包（MSIX）身份：包名 + 发布者哈希（二者组成包族名,跨版本稳定）+ 应用 id。
/// 本机 `Get-StartApps`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppPackage {
    pub name: &'static str,
    pub publisher: &'static str,
    pub app: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchSpec {
    pub exe: &'static str,
    pub kind: LaunchKind,
    /// 应用包优先（包内 exe 不能按路径直接启动）;没装包再按 Win32 安装登记找 `exe`。
    pub package: Option<AppPackage>,
}

/// 映像名 → 启动方式。`chatgpt.exe` 不启动（那是 ChatGPT 本体,不是 Codex 宿主）。
pub fn launch_spec(exe: &'static str) -> Option<LaunchSpec> {
    let (kind, package) = match exe {
        "claude.exe" => (LaunchKind::App, Some(AppPackage { name: "Claude", publisher: "pzs8sxrjxfjjc", app: "Claude" })),
        "codex.exe" => (LaunchKind::App, Some(AppPackage { name: "OpenAI.Codex", publisher: "2p2nqsd0c76g0", app: "App" })),
        "chatgpt.exe" => return None,
        "code.exe" | "codebuddy cn.exe" | "codebuddy.exe" | "workbuddy.exe" => (LaunchKind::Ide, None),
        _ => (LaunchKind::App, None),
    };
    Some(LaunchSpec { exe, kind, package })
}

/// 会话跳转的目标宿主。与 `resolve_target` 的差别：Claude Code 的宿主线索明确不是桌面端 / VS Code
/// （如 CLI 跑在终端里）→ None——没有可认的桌面窗口,不去前置一个无关的 Claude 窗口。
pub fn open_target(agent: &str, host: Option<&str>) -> Option<HostTarget> {
    match (agent, host) {
        ("claude-code", Some(h)) if h != "claude-desktop" && h != "claude-vscode" => None,
        _ => resolve_target(agent, host),
    }
}

/// 会话跳转的选窗：按标题消歧的宿主**必须**标题命中项目才算（不退回第一个——没命中说明该项目没开窗口,
/// 走启动）;无目录尾段（Scratch / 非路径键）或单窗口宿主取第一个。
pub fn pick_window_strict(titles: &[String], tail: Option<&str>, by_title: bool) -> Option<usize> {
    match (by_title, tail) {
        (true, Some(tail)) => titles.iter().position(|t| title_has_project(t, tail)),
        _ => (!titles.is_empty()).then_some(0),
    }
}

/// 安装登记里的图标 / 路径值 → 可执行文件路径：去引号、去 `,N` 图标序号。
pub fn clean_exe_value(raw: &str) -> String {
    let s = raw.trim().trim_matches('"');
    match s.rsplit_once(',') {
        Some((head, idx)) if idx.trim().parse::<i32>().is_ok() => head.trim().trim_matches('"').to_string(),
        _ => s.to_string(),
    }
}

/// 应用包全名（`<name>_<version>_<arch>_<resource>_<publisher>`）是否属于该包族。
pub fn package_matches(full_name: &str, pkg: AppPackage) -> bool {
    full_name.strip_prefix(pkg.name).is_some_and(|rest| rest.starts_with('_')) && full_name.ends_with(&format!("_{}", pkg.publisher))
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct OpenResult {
    /// `focused` 已前置 / `launched` 已启动宿主 / `folder` 回退打开了项目目录 / `none` 无事可做。
    pub outcome: &'static str,
}

/// 双击会话条：到该会话所在的 agent。会话的项目目录与宿主线索都从库里取（前端不传路径）。
#[tauri::command(rename_all = "snake_case")]
pub fn open_agent_session(agent: String, session_id: String, app: AppHandle, state: State<'_, AppState>) -> Result<OpenResult, String> {
    if resolve_target(&agent, None).is_none() {
        return Err("unknown agent".into());
    }
    let project_key = crate::commands::with_reader(&state, |store| Ok(store.session_project_key(&agent, &session_id)))?
        .ok_or_else(|| "unknown session".to_string())?;
    // 宿主线索：活跃会话取注意力表（最新）,否则查采集游标
    let live = state.attention.lock().map_err(|_| "attention lock poisoned".to_string())?.lookup(&agent, &session_id);
    let host = match live.and_then(|(h, _)| h) {
        Some(h) => Some(h),
        None => crate::commands::with_reader(&state, |store| Ok(store.session_host(&agent, &session_id)))?,
    };
    let folder = crate::commands::project_folder(&project_key);
    let tail = dir_tail(&project_key);
    let outcome = match open_target(&agent, host.as_deref()) {
        Some(t) => platform::open(t, tail.as_deref(), folder.as_deref()),
        None => "none",
    };
    let outcome = if outcome == "none" && folder.as_deref().is_some_and(platform::open_folder) { "folder" } else { outcome };
    crate::dev_log!("[open] agent={} host={:?} outcome={}", agent, host, outcome);
    // 到了 agent 面前即确认该会话的等待（与聚焦同口径）;没到不动表
    if outcome == "focused" || outcome == "launched" {
        let now = now_millis();
        let idle = crate::collector::task_store::idle_threshold_ms();
        let changed = state.attention.lock().map_err(|_| "attention lock poisoned".to_string())?.ack(&agent, &session_id, now, idle);
        if changed {
            collector::notify_attention(&app);
        }
    }
    Ok(OpenResult { outcome })
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
    use std::sync::atomic::{AtomicI64, AtomicIsize, Ordering};
    use std::sync::Mutex;

    use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM};
    use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_EXPAND_SZ,
        RRF_RT_REG_SZ,
    };
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, EnumWindows, GetAncestor, GetForegroundWindow, GetMessageW, GetWindowLongPtrW, GetWindowTextW,
        GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow, EVENT_SYSTEM_FOREGROUND, GA_ROOTOWNER,
        GWL_EXSTYLE, MSG, SW_RESTORE, WINEVENT_OUTOFCONTEXT, WS_EX_TOOLWINDOW,
    };

    use super::{pick_window, HostTarget};
    use crate::collector::attention::ForegroundWindow;

    /// 前台根窗口与它进前台的时刻（钩子回调写;先写时刻后写句柄,读方句柄对上即时刻可信）。
    static FG_ROOT: AtomicIsize = AtomicIsize::new(0);
    static FG_SINCE: AtomicI64 = AtomicI64::new(0);
    /// （根窗口, pid, 映像名)——同一前台窗口不重复开进程句柄。
    static EXE_CACHE: Mutex<Option<(isize, u32, String)>> = Mutex::new(None);

    unsafe fn root_of(hwnd: HWND) -> HWND {
        let root = GetAncestor(hwnd, GA_ROOTOWNER);
        if root.is_null() {
            hwnd
        } else {
            root
        }
    }

    unsafe extern "system" fn on_foreground(_: HWINEVENTHOOK, _: u32, hwnd: HWND, _: i32, _: i32, _: u32, _: u32) {
        if hwnd.is_null() {
            return;
        }
        let root = root_of(hwnd) as isize;
        if FG_ROOT.load(Ordering::SeqCst) != root {
            FG_SINCE.store(crate::collector::store::now_millis(), Ordering::SeqCst);
            FG_ROOT.store(root, Ordering::SeqCst);
        }
    }

    pub fn spawn_foreground_hook() {
        let _ = std::thread::Builder::new().name("foreground-hook".into()).spawn(|| unsafe {
            let cur = GetForegroundWindow();
            if !cur.is_null() {
                FG_SINCE.store(crate::collector::store::now_millis(), Ordering::SeqCst);
                FG_ROOT.store(root_of(cur) as isize, Ordering::SeqCst);
            }
            let hook = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                std::ptr::null_mut(),
                Some(on_foreground),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            );
            if hook.is_null() {
                crate::dev_log!("[attention] foreground hook install failed; falling back to sampled since");
                return;
            }
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                DispatchMessageW(&msg);
            }
        });
    }

    pub fn foreground_window() -> Option<ForegroundWindow> {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                return None;
            }
            let root = root_of(hwnd);
            let mut pid = 0u32;
            GetWindowThreadProcessId(root, &mut pid);
            if pid == 0 {
                return None;
            }
            let key = root as isize;
            let cached = EXE_CACHE.lock().ok().and_then(|c| c.as_ref().filter(|(w, p, _)| *w == key && *p == pid).map(|(_, _, e)| e.clone()));
            let exe = match cached {
                Some(e) => e,
                None => {
                    let e = image_name(pid)?;
                    if let Ok(mut c) = EXE_CACHE.lock() {
                        *c = Some((key, pid, e.clone()));
                    }
                    e
                }
            };
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(root, buf.as_mut_ptr(), buf.len() as i32).max(0);
            let since = if FG_ROOT.load(Ordering::SeqCst) == key { FG_SINCE.load(Ordering::SeqCst) } else { 0 };
            Some(ForegroundWindow { window: key, exe, title: String::from_utf16_lossy(&buf[..n as usize]), since })
        }
    }

    pub fn input_idle_ms() -> Option<u64> {
        unsafe {
            let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
            if GetLastInputInfo(&mut info) == 0 {
                return None;
            }
            Some(GetTickCount().wrapping_sub(info.dwTime) as u64)
        }
    }

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

    /// 进程映像全路径;打不开（权限 / 已退出）→ None。
    fn image_path(pid: u32) -> Option<String> {
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
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        }
    }

    fn file_name_lower(path: &str) -> Option<String> {
        path.rsplit(['\\', '/']).next().map(|s| s.to_lowercase())
    }

    /// 进程映像文件名（小写,不含路径);打不开（权限 / 已退出）→ None。
    fn image_name(pid: u32) -> Option<String> {
        image_path(pid).as_deref().and_then(file_name_lower)
    }

    unsafe fn bring_to_front(hwnd: HWND) {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        // 前台锁定下失败只会闪任务栏图标,仍算找到
        SetForegroundWindow(hwnd);
    }

    /// 会话跳转：窗口在 → 前置（`focused`）;不在 → 按登记顺序启动第一个找得到的宿主（`launched`）;
    /// 都没有 → `none`（调用方回退打开目录）。IDE 类已在运行（开着别的项目）时直接用它的映像路径。
    pub fn open(target: HostTarget, tail: Option<&str>, folder: Option<&std::path::Path>) -> &'static str {
        let wins = top_windows();
        let mut paths: HashMap<u32, Option<String>> = HashMap::new();
        let mut by_exe: Vec<Vec<(&Win, String)>> = vec![Vec::new(); target.exes.len()];
        for w in &wins {
            let Some(path) = paths.entry(w.pid).or_insert_with(|| image_path(w.pid)).clone() else { continue };
            if let Some(i) = file_name_lower(&path).and_then(|n| target.exes.iter().position(|e| *e == n)) {
                by_exe[i].push((w, path));
            }
        }
        for (group, exe) in by_exe.iter().zip(target.exes) {
            let titles: Vec<String> = group.iter().map(|(w, _)| w.title.clone()).collect();
            // 标题消歧只对 IDE 类映像生效（宿主未知的 Claude Code 目标里,桌面端单窗口不看标题）
            let by_title = target.by_title && super::launch_spec(exe).is_some_and(|s| s.kind == super::LaunchKind::Ide);
            if let Some(i) = super::pick_window_strict(&titles, tail, by_title) {
                unsafe { bring_to_front(group[i].0.hwnd) };
                return "focused";
            }
        }
        for (i, exe) in target.exes.iter().enumerate() {
            let Some(spec) = super::launch_spec(exe) else { continue };
            if launch(spec, by_exe[i].first().map(|(_, p)| p.as_str()), folder) {
                return "launched";
            }
        }
        "none"
    }

    fn launch(spec: super::LaunchSpec, running: Option<&str>, folder: Option<&std::path::Path>) -> bool {
        use std::process::Command;
        if let Some(pkg) = spec.package.filter(|p| package_installed(*p)) {
            // 应用包经 shell 的 AppsFolder 激活（包内 exe 不能按路径启动）;参数只由本文件常量拼成
            let target = format!("shell:AppsFolder\\{}_{}!{}", pkg.name, pkg.publisher, pkg.app);
            return Command::new("explorer.exe").arg(target).spawn().is_ok();
        }
        let Some(path) = running.map(std::path::PathBuf::from).or_else(|| find_exe(spec.exe)) else { return false };
        let mut cmd = Command::new(&path);
        if let (super::LaunchKind::Ide, Some(folder)) = (spec.kind, folder) {
            cmd.arg(folder);
        }
        if let Some(dir) = path.parent() {
            cmd.current_dir(dir);
        }
        cmd.spawn().is_ok()
    }

    pub fn open_folder(path: &std::path::Path) -> bool {
        std::process::Command::new("explorer").arg(path).spawn().is_ok()
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// 注册表字符串值（`value = None` 取默认值;REG_EXPAND_SZ 由系统展开）。
    fn reg_string(root: HKEY, sub_key: &str, value: Option<&str>) -> Option<String> {
        let sub = wide(sub_key);
        let val = value.map(wide);
        let mut buf = [0u16; 1024];
        let mut size = (buf.len() * 2) as u32;
        let rc = unsafe {
            RegGetValueW(
                root,
                sub.as_ptr(),
                val.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut c_void,
                &mut size,
            )
        };
        if rc != 0 {
            return None;
        }
        let n = (size as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..n]);
        let s = s.trim_end_matches('\0').trim();
        (!s.is_empty()).then(|| s.to_string())
    }

    fn reg_subkeys(root: HKEY, sub_key: &str) -> Vec<String> {
        let sub = wide(sub_key);
        let mut out = Vec::new();
        unsafe {
            let mut key: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(root, sub.as_ptr(), 0, KEY_READ, &mut key) != 0 {
                return out;
            }
            for index in 0.. {
                let mut name = [0u16; 256];
                let mut len = name.len() as u32;
                let rc = RegEnumKeyExW(
                    key,
                    index,
                    name.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                if rc != 0 {
                    break;
                }
                out.push(String::from_utf16_lossy(&name[..len as usize]));
            }
            RegCloseKey(key);
        }
        out
    }

    fn package_installed(pkg: super::AppPackage) -> bool {
        const PACKAGES: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";
        reg_subkeys(HKEY_CURRENT_USER, PACKAGES).iter().any(|n| super::package_matches(n, pkg))
    }

    /// 按安装登记找 Win32 宿主的可执行文件：App Paths → 卸载表（`DisplayIcon` 本身 / 它所在目录 /
    /// `InstallLocation` 下的同名文件）。文件名必须与登记名相等（大小写不敏感）且文件存在。
    fn find_exe(exe: &str) -> Option<std::path::PathBuf> {
        let ok = |p: std::path::PathBuf| {
            (p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case(exe)) && p.is_file()).then_some(p)
        };
        for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
            let sub = format!(r"Software\Microsoft\Windows\CurrentVersion\App Paths\{exe}");
            if let Some(p) = reg_string(root, &sub, None).and_then(|v| ok(super::clean_exe_value(&v).into())) {
                return Some(p);
            }
        }
        const UNINSTALL: [(bool, &str); 3] = [
            (true, r"Software\Microsoft\Windows\CurrentVersion\Uninstall"),
            (false, r"Software\Microsoft\Windows\CurrentVersion\Uninstall"),
            (false, r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
        ];
        for (user, base) in UNINSTALL {
            let root = if user { HKEY_CURRENT_USER } else { HKEY_LOCAL_MACHINE };
            for k in reg_subkeys(root, base) {
                let sub = format!(r"{base}\{k}");
                let icon = reg_string(root, &sub, Some("DisplayIcon")).map(|v| std::path::PathBuf::from(super::clean_exe_value(&v)));
                let loc = reg_string(root, &sub, Some("InstallLocation")).map(|v| std::path::PathBuf::from(super::clean_exe_value(&v)));
                let candidates = [icon.clone(), icon.and_then(|p| p.parent().map(|d| d.join(exe))), loc.map(|d| d.join(exe))];
                if let Some(p) = candidates.into_iter().flatten().find_map(&ok) {
                    return Some(p);
                }
            }
        }
        None
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
                unsafe { bring_to_front(group[i].hwnd) };
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

    pub fn open(_target: HostTarget, _tail: Option<&str>, _folder: Option<&std::path::Path>) -> &'static str {
        "none"
    }

    pub fn open_folder(_path: &std::path::Path) -> bool {
        false
    }

    pub fn foreground_window() -> Option<crate::collector::attention::ForegroundWindow> {
        None
    }

    pub fn spawn_foreground_hook() {}

    pub fn input_idle_ms() -> Option<u64> {
        None
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
    fn watch_registry_is_stricter_than_focus() {
        assert_eq!(watch_target("claude-code", None), None, "宿主未知不参与前台确认");
        assert_eq!(watch_target("claude-code", Some("cli")), None);
        assert_eq!(watch_target("claude-code", Some("claude-desktop")), resolve_target("claude-code", Some("claude-desktop")));
        assert_eq!(watch_target("codex", None).map(|t| t.exes), Some(&["codex.exe"][..]));
        assert_eq!(watch_target("zcode", None), resolve_target("zcode", None));
        let vs = watch_target("claude-code", Some("claude-vscode")).unwrap();
        assert!(window_matches(vs, "code.exe", "a.rs - TokenCalendar - Visual Studio Code", "e:/projects/tokencalendar"));
        assert!(!window_matches(vs, "code.exe", "a.rs - Other - Visual Studio Code", "e:/projects/tokencalendar"));
        assert!(!window_matches(vs, "code.exe", "anything", "e:"), "无目录尾段不按标题硬配");
        assert!(!window_matches(vs, "claude.exe", "TokenCalendar", "e:/projects/tokencalendar"));
        assert!(
            !window_matches(vs, "code.exe", "a.rs - TokenCalendar - Visual Studio Code", "e:/work/calendar"),
            "子串不算:calendar 不命中 TokenCalendar 窗口"
        );
        assert!(!window_matches(vs, "code.exe", "calendar.rs - Other - Visual Studio Code", "e:/work/calendar"), "文件名段不算");
        let desk = watch_target("claude-code", Some("claude-desktop")).unwrap();
        assert!(window_matches(desk, "claude.exe", "Claude", "e:/whatever"), "单窗口宿主不看标题");
    }

    #[test]
    fn title_segments_match_whole_project_name() {
        assert!(title_has_project("\u{25cf} main.rs - TokenCalendar - Visual Studio Code", "tokencalendar"), "未保存标记");
        assert!(title_has_project("main.rs - TokenCalendar [WSL: Ubuntu] - Visual Studio Code", "tokencalendar"), "远程后缀");
        assert!(title_has_project("TokenCalendar (Workspace) - Visual Studio Code", "tokencalendar"), "工作区后缀");
        assert!(title_has_project("main.rs \u{2014} TokenCalendar \u{2014} CodeBuddy", "tokencalendar"), "长破折号分隔");
        assert!(!title_has_project("main.rs - TokenCalendar - Visual Studio Code", "calendar"));
        assert!(!title_has_project("my-app-server - Visual Studio Code", "app"));
    }

    #[test]
    fn open_session_registry_and_strict_pick() {
        // 每个登记的映像名都有启动方式,只有 chatgpt.exe 不启动
        for a in ["claude-code", "codex", "dsh", "zcode", "codebuddy", "workbuddy"] {
            for exe in resolve_target(a, None).unwrap().exes {
                assert_eq!(launch_spec(exe).is_none(), *exe == "chatgpt.exe", "{exe}");
            }
        }
        assert_eq!(launch_spec("code.exe").unwrap().kind, LaunchKind::Ide);
        assert_eq!(launch_spec("zcode.exe").unwrap().kind, LaunchKind::App);
        assert!(launch_spec("claude.exe").unwrap().package.is_some() && launch_spec("code.exe").unwrap().package.is_none());
        // CLI 等非桌面宿主不认窗口;未知 / 桌面宿主照常
        assert_eq!(open_target("claude-code", Some("cli")), None);
        assert_eq!(open_target("claude-code", Some("claude-vscode")), resolve_target("claude-code", Some("claude-vscode")));
        assert_eq!(open_target("claude-code", None), resolve_target("claude-code", None));
        assert_eq!(open_target("zcode", None), resolve_target("zcode", None));
        // 严格选窗：IDE 没开该项目 → None（走启动）,不退回第一个
        let titles = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let vs = titles(&["main.rs - OtherProj - Visual Studio Code", "AGENTS.md - TokenCalendar - Visual Studio Code"]);
        assert_eq!(pick_window_strict(&vs, Some("tokencalendar"), true), Some(1));
        assert_eq!(pick_window_strict(&vs, Some("nomatch"), true), None);
        assert_eq!(pick_window_strict(&vs, None, true), Some(0), "无目录尾段取第一个");
        assert_eq!(pick_window_strict(&titles(&["Claude"]), Some("x"), false), Some(0));
        assert_eq!(pick_window_strict(&[], Some("x"), false), None);
    }

    #[test]
    fn install_registry_values() {
        assert_eq!(clean_exe_value(r"D:\Program\WorkBuddy\WorkBuddy.exe,0"), r"D:\Program\WorkBuddy\WorkBuddy.exe");
        assert_eq!(clean_exe_value(r#""D:\Program\Microsoft VS Code\Code.exe""#), r"D:\Program\Microsoft VS Code\Code.exe");
        assert_eq!(clean_exe_value(r#""C:\a, b\App.exe",-101"#), r"C:\a, b\App.exe");
        assert_eq!(clean_exe_value(r"C:\a, b\App.exe"), r"C:\a, b\App.exe", "路径里的逗号不是图标序号");
        let claude = launch_spec("claude.exe").unwrap().package.unwrap();
        assert!(package_matches("Claude_2.110.1.0_x64__pzs8sxrjxfjjc", claude));
        assert!(!package_matches("ClaudeOther_1.0.0.0_x64__pzs8sxrjxfjjc", claude), "包名整段相等");
        assert!(!package_matches("Claude_2.110.1.0_x64__aaaaaaaaaaaaa", claude), "发布者不同");
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
