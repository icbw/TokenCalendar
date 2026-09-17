//! 最小托盘（小步版）：「主窗口关闭 = 隐藏」必须给退出兜底，
//! 完整托盘菜单（采集暂停/导出等）仍归。
//!
//! 行为（左键不再切换挂件）：
//! - 菜单：挂件 ✓ / 主窗口 ✓ / 退出（勾选态随窗口显隐实时同步）；
//! - 左键单击 = **显示主界面**（show 语义而非 toggle——挂件是常驻贴片，
//!   开关语义别扭;重复点击左键也不该把主界面藏回去）；
//! - 右键 = 菜单；
//! - 显隐反馈 = 窗口本身（不再发 tray:action toast，
//!   弹泡提示全部去除）。

use std::sync::atomic::Ordering;

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::visibility;
use crate::AppState;

const TRAY_ID: &str = "tokencalendar-tray";
const ID_TOGGLE_WIDGET: &str = "toggle_widget";
const ID_TOGGLE_MAIN: &str = "toggle_main";
/// 悬浮球显隐（第三勾选项）。
const ID_TOGGLE_ORB: &str = "toggle_orb";
/// 项目推进时间轴显隐（第四勾选项）。
const ID_TOGGLE_TIMELINE: &str = "toggle_timeline";
const ID_QUIT: &str = "quit";

/// 托盘勾选项句柄：显隐变更时同步勾选态（避免重建菜单）。
pub struct TrayHandles {
    pub widget: CheckMenuItem<Wry>,
    pub main: CheckMenuItem<Wry>,
    pub orb: CheckMenuItem<Wry>,
    pub timeline: CheckMenuItem<Wry>,
}

pub fn init(app: &AppHandle) -> Result<(), tauri::Error> {
    let state = app.state::<AppState>();
    let toggle_widget = CheckMenuItem::with_id(
        app,
        ID_TOGGLE_WIDGET,
        "挂件 Widget",
        true,
        state.widget_visible.load(Ordering::SeqCst),
        None::<&str>,
    )?;
    let toggle_main = CheckMenuItem::with_id(
        app,
        ID_TOGGLE_MAIN,
        "主窗口 Main",
        true,
        state.main_visible.load(Ordering::SeqCst),
        None::<&str>,
    )?;
    let toggle_orb = CheckMenuItem::with_id(
        app,
        ID_TOGGLE_ORB,
        "悬浮球 Orb",
        true,
        state.orb_visible.load(Ordering::SeqCst),
        None::<&str>,
    )?;
    let toggle_timeline = CheckMenuItem::with_id(
        app,
        ID_TOGGLE_TIMELINE,
        "时间轴 Timeline",
        true,
        state.timeline_visible.load(Ordering::SeqCst),
        None::<&str>,
    )?;
    let quit_item = MenuItem::with_id(app, ID_QUIT, "退出 TokenCalendar", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[&toggle_widget, &toggle_main, &toggle_orb, &toggle_timeline, &sep, &quit_item],
    )?;

    // 句柄存入 AppState，供 sync_checks 在任何显隐路径上更新勾选态
    if let Some(s) = app.try_state::<AppState>() {
        *s.tray.lock().unwrap() = Some(TrayHandles {
            widget: toggle_widget,
            main: toggle_main,
            orb: toggle_orb,
            timeline: toggle_timeline,
        });
    }

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(
            app.default_window_icon()
                .expect("tauri.conf.json declares bundle icons")
                .clone(),
        )
        .tooltip("TokenCalendar")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            ID_TOGGLE_WIDGET => tray_toggle(app, visibility::WIDGET_LABEL, "挂件"),
            ID_TOGGLE_MAIN => tray_toggle(app, visibility::MAIN_LABEL, "主窗口"),
            ID_TOGGLE_ORB => tray_toggle(app, visibility::ORB_LABEL, "悬浮球"),
            ID_TOGGLE_TIMELINE => tray_toggle(app, visibility::TIMELINE_LABEL, "时间轴"),
            ID_QUIT => quit_app(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event
            {
                let app = tray.app_handle();
                // 左键 = 显示主界面（show 语义:已可见时保持可,只唤回不隐藏;
                // visibility:set_visible 内部已处理 focus/重复 show 幂等）
                if let Err(e) = visibility::set_visible(app, visibility::MAIN_LABEL, true) {
                    crate::dev_log!("[tray] show main failed: {}", e);
                }
            }
        })
        .build(app)?;
    Ok(())
}

/// 托盘发起的切换：执行即可（窗口显隐本身即反馈,无 toast）。
fn tray_toggle(app: &AppHandle, label: &str, _display: &str) {
    if let Err(e) = visibility::toggle(app, label) {
        crate::dev_log!("[tray] toggle {} failed: {}", label, e);
    }
}

/// 退出前落盘（含可见性），再退出。CloseRequested 不触发，必须手动 persist。
fn quit_app(app: &AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        crate::window_state::persist(app, &state);
    }
    app.exit(0);
}

/// 任何显隐路径之后同步托盘勾选态。
pub fn sync_checks(state: &tauri::State<'_, AppState>) {
    let guard = state.tray.lock().unwrap();
    if let Some(handles) = guard.as_ref() {
        let _ = handles
            .widget
            .set_checked(state.widget_visible.load(Ordering::SeqCst));
        let _ = handles
            .main
            .set_checked(state.main_visible.load(Ordering::SeqCst));
        let _ = handles
            .orb
            .set_checked(state.orb_visible.load(Ordering::SeqCst));
        let _ = handles
            .timeline
            .set_checked(state.timeline_visible.load(Ordering::SeqCst));
    }
}
