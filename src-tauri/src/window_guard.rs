//! 置顶窗口的运行期对账：悬浮球 / 时间轴条态「标志说在、实际没在」的自愈。
//!
//! 背景：这两个窗口的价值全在「常驻最上层」，被遮住等于消失。tao 的置顶只在**标志翻转**
//! 时才去碰系统——`is_always_on_top` 读的是缓存标志，窗口创建时设过一次之后，运行期
//! 没有任何路径再核对系统里的真实状态。外部一旦把 `WS_EX_TOPMOST` 位拿掉（或窗口被系统
//! 层面藏起来），应用这边标志全是「开」、托盘勾选态也是「开」，窗口却被普通窗口盖住——
//! 症状：久置后托盘显示悬浮球开着、屏幕上看不，托盘关一次再开才出现（关再开会走
//! tao 的 `apply_diff`，把整组样式含 TOPMOST 位按缓存写回，同时窗口落到所在 Z 带最上）。
//!
//! 做法：后台线程每 3 秒对每个置顶窗口读一次系统真实状态，失配即，三者互斥：
//! - 标志可见、系统却不可见 ⇒ 先 `hide` 翻 tao 的缓存、再 `visibility:set_visible（true)`
//!   （含 orb 显隐钩子 / 落盘 / 托盘同步 / 事件广播；不先翻缓存的话 tao 会因「标志无差异」
//!   短路，show 不落到系统）；
//! - 缓存意图是置顶、`WS_EX_TOPMOST` 位却不在 ⇒ 直接 `SetWindowPos（HWND_TOPMOST)` 补回。
//!
//! 只在「位真的丢了」时才补置顶，**不**因「被别的窗口盖住」就抬到最上：被另一个置顶窗口
//! 盖住（全屏播放器、开始菜单、任务栏弹层）是置顶带内的正常次序，抢回来会和那些窗口互相
//! 抬升、遮住系统界面。用户主动隐藏（标志为 false）不在对账范围。

use std::time::Duration;

use tauri::{AppHandle, Manager, WebviewWindow};

use crate::visibility::{self, ORB_LABEL, TIMELINE_LABEL};
use crate::AppState;

/// 需要对账的窗口：只有「常驻最上层才有意义」的两个（挂件贴桌面、主窗口常规 Z 序，不在内）。
const GUARDED: [&str; 2] = [ORB_LABEL, TIMELINE_LABEL];
/// 对账周期。位丢了到被补回最长约 3 秒；每轮只读一次样式位，开销可忽略。
const POLL: Duration = Duration::from_secs(3);
/// 判「标志可见、系统不可见」后的复核等待：覆盖 `set_visible（false)` 里「先 hide、后翻标志」
/// 那一瞬间的读数错位，避免与用户的手动隐藏互相抢。
const RECHECK: Duration = Duration::from_millis(300);

#[derive(Debug, PartialEq, Eq)]
enum Repair {
    None,
    /// 标志可见但系统不可见：补显示。
    Show,
    /// 意图置顶但系统位不在：补置顶。
    Topmost,
}

fn diagnose(flagged_visible: bool, shown: bool, want_topmost: bool, has_topmost: bool) -> Repair {
    if !flagged_visible {
        Repair::None
    } else if !shown {
        Repair::Show
    } else if want_topmost && !has_topmost {
        Repair::Topmost
    } else {
        Repair::None
    }
}

/// setup 末尾 spawn（与 `visibility:spawn_show_watchdog` 并列：那个管启动期、这个管整个运行期）。
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(POLL);
        for label in GUARDED {
            check(&app, label);
        }
    });
}

fn check(app: &AppHandle, label: &str) {
    let Some(state) = app.try_state::<AppState>() else { return };
    let Some(window) = app.get_webview_window(label) else { return };
    let mut repair = diagnose(
        visibility::is_visible(&state, label),
        window.is_visible().unwrap_or(true),
        window.is_always_on_top().unwrap_or(false),
        has_topmost_bit(&window),
    );
    if repair == Repair::Show {
        std::thread::sleep(RECHECK);
        repair = diagnose(
            visibility::is_visible(&state, label),
            window.is_visible().unwrap_or(true),
            window.is_always_on_top().unwrap_or(false),
            has_topmost_bit(&window),
        );
    }
    match repair {
        Repair::None => {}
        Repair::Show => {
            crate::dev_log!("[window-guard] {label} flagged visible but hidden, forcing show");
            // tao 缓存里的 VISIBLE 仍是 true，直接 show 会因「标志无差异」被短路、
            // 根本不调 ShowWindow（外部 SW_HIDE 后 set_visible（true) 无效）。
            // 先 hide 把缓存翻回去，随后的 show 才真正落到系统——与托盘「关→开」同一路径，
            // 可见性标志全程不动，托盘勾选态不闪。
            let _ = window.hide();
            if let Err(e) = visibility::set_visible(app, label, true) {
                crate::dev_log!("[window-guard] force show {label} failed: {e}");
            }
        }
        Repair::Topmost => {
            // 发令前再看一眼意图：时间轴条态↔看板态切换时缓存会翻到 false，别在它后面补一刀
            if window.is_always_on_top().unwrap_or(false) && assert_topmost(&window) {
                crate::dev_log!("[window-guard] {label} lost WS_EX_TOPMOST, re-asserted");
            }
        }
    }
}

/// 系统里该窗口的 `WS_EX_TOPMOST` 位是否在。读不到句柄按「在」处理（不乱补）。
#[cfg(windows)]
fn has_topmost_bit(window: &WebviewWindow) -> bool {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_TOPMOST};
    let Ok(hwnd) = window.hwnd() else { return true };
    unsafe { (GetWindowLongPtrW(hwnd.0 as HWND, GWL_EXSTYLE) as u32) & WS_EX_TOPMOST != 0 }
}

/// 把窗口放回置顶带（同时恢复 `WS_EX_TOPMOST` 位）。不动位置 / 尺寸、不激活；
/// `ASYNCWINDOWPOS` 与 tao 自己的置顶写法一致，不会因 UI 线程忙而卡住对账线程。
#[cfg(windows)]
fn assert_topmost(window: &WebviewWindow) -> bool {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER,
        SWP_NOSIZE,
    };
    let Ok(hwnd) = window.hwnd() else { return false };
    unsafe {
        SetWindowPos(
            hwnd.0 as HWND,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
        ) != 0
    }
}

#[cfg(not(windows))]
fn has_topmost_bit(_window: &WebviewWindow) -> bool {
    true
}

#[cfg(not(windows))]
fn assert_topmost(_window: &WebviewWindow) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_hidden_windows_are_left_alone() {
        // 标志为 false = 用户主动隐藏：系统层不可见 / 置顶位不在都不构成问题
        assert_eq!(diagnose(false, false, true, false), Repair::None);
    }

    #[test]
    fn flagged_but_invisible_is_shown_again() {
        assert_eq!(diagnose(true, false, true, true), Repair::Show);
    }

    #[test]
    fn show_takes_precedence_over_topmost() {
        // 补显示会按缓存把 TOPMOST 位写回，不叠加第二个动作
        assert_eq!(diagnose(true, false, true, false), Repair::Show);
    }

    #[test]
    fn lost_topmost_bit_is_reasserted_only_when_intended() {
        assert_eq!(diagnose(true, true, true, false), Repair::Topmost);
        // 时间轴看板态：意图非置顶，系统位不在是对的
        assert_eq!(diagnose(true, true, false, false), Repair::None);
    }

    #[test]
    fn healthy_window_is_untouched() {
        assert_eq!(diagnose(true, true, true, true), Repair::None);
    }
}
