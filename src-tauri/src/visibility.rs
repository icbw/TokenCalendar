//! 窗口可见性原语（替代退役的 get_mode/set_mode 互斥模式）。
//!
//! 单一源在 AppState.widget_visible / main_visible / orb_visible；所有变更路径
//! （前端命令、托盘菜单、托盘左键、主窗口关闭钮）都汇入这里，统一：
//! show/hide 窗口 → 更新标志 → 落盘 → 同步托盘勾选态 → 广播事件。
//!
//! 事件：`widget-visibility-changed` / `main-visibility-changed` /
//! `orb-visibility-changed`，载荷为 bool。
//! 前端不维护本地真相，按钮状态以 get_visibility 初值 + 事件跟随为准。

use std::sync::atomic::Ordering;

use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use crate::tray;
use crate::window_state;
use crate::AppState;

pub const WIDGET_LABEL: &str = "widget";
pub const MAIN_LABEL: &str = "main";
/// 悬浮球（第三窗口；默认隐藏，设置页/托盘开启）。
pub const ORB_LABEL: &str = "orb";

fn label_ok(label: &str) -> bool {
    label == WIDGET_LABEL || label == MAIN_LABEL || label == ORB_LABEL
}

fn is_visible(state: &AppState, label: &str) -> bool {
    match label {
        WIDGET_LABEL => state.widget_visible.load(Ordering::SeqCst),
        MAIN_LABEL => state.main_visible.load(Ordering::SeqCst),
        ORB_LABEL => state.orb_visible.load(Ordering::SeqCst),
        _ => false,
    }
}

fn store_visible(state: &AppState, label: &str, visible: bool) {
    match label {
        WIDGET_LABEL => state.widget_visible.store(visible, Ordering::SeqCst),
        MAIN_LABEL => state.main_visible.store(visible, Ordering::SeqCst),
        ORB_LABEL => state.orb_visible.store(visible, Ordering::SeqCst),
        _ => {}
    }
}

fn event_name(label: &str) -> &'static str {
    match label {
        WIDGET_LABEL => "widget-visibility-changed",
        // orb 走同族命名
        ORB_LABEL => "orb-visibility-changed",
        _ => "main-visibility-changed",
    }
}

/// 显示/隐藏窗口（不动焦点——挂件是 glanceable 的，不能抢焦点；
/// 主窗口由托盘/按钮唤起时给焦点，避免「点开了却看不见」）。
pub fn set_visible(app: &AppHandle, label: &str, visible: bool) -> Result<(), String> {
    if !label_ok(label) {
        return Err(format!("unknown window: {}", label));
    }
    let state = app.state::<AppState>();
    let window: WebviewWindow = app
        .get_webview_window(label)
        .ok_or_else(|| format!("{} window not found", label))?;
    if visible {
        window.show().map_err(|e| e.to_string())?;
        if label == MAIN_LABEL {
            let _ = window.set_focus();
        }
    } else {
        window.hide().map_err(|e| e.to_string())?;
    }
    // ㊻/㊼：悬浮球的「指针让出」态跨显隐不可残留——隐藏期间没有鼠标
    // 消息去复位它,下次显示时整窗会被鼠标穿透（主体也点不动）;显隐两条路径都
    // 复位,并让常驻轮询随可见性起停（active = 本次操作后的可见性）。
    if label == ORB_LABEL {
        crate::orb_dock::reset_pointer_pass(&window, visible);
    }
    store_visible(&state, label, visible);
    window_state::persist(app, &state);
    tray::sync_checks(&state);
    app.emit(event_name(label), visible).map_err(|e| e.to_string())
}

/// 切换可见性，返回切换后的状态（托盘据此发 toast）。
pub fn toggle(app: &AppHandle, label: &str) -> Result<bool, String> {
    if !label_ok(label) {
        return Err(format!("unknown window: {}", label));
    }
    let state = app.state::<AppState>();
    let next = !is_visible(&state, label);
    set_visible(app, label, next)?;
    Ok(next)
}

/// 前端 window_ready：首帧提交后由后端按标志裁决是否 show。
/// 窗口带 visible 创建会画白首帧，
/// 所以两个窗口都 visible:false 创建，show 的时机统一到这里。
/// HMR / 手动刷新重入时幂等：标志说隐藏就拒绝 show。
pub fn ready(window: &WebviewWindow) {
    let app = window.app_handle();
    let label = window.label();
    if !label_ok(label) {
        return;
    }
    let state = app.state::<AppState>();
    if is_visible(&state, label) {
        let _ = window.show();
    }
}

#[tauri::command]
pub fn get_visibility(state: tauri::State<'_, AppState>) -> serde_json::Value {
    serde_json::json!({
        "widget": state.widget_visible.load(Ordering::SeqCst),
        "main": state.main_visible.load(Ordering::SeqCst),
        "orb": state.orb_visible.load(Ordering::SeqCst),
    })
}

macro_rules! visibility_command {
    ($fn_name:ident, $label:literal, $visible:literal) => {
        #[tauri::command]
        pub fn $fn_name(app: tauri::AppHandle) -> Result<(), String> {
            set_visible(&app, $label, $visible)
        }
    };
}

visibility_command!(show_widget, "widget", true);
visibility_command!(hide_widget, "widget", false);
visibility_command!(show_main, "main", true);
visibility_command!(hide_main, "main", false);
// 悬浮球显隐（托盘与设置页共用；orb 是 glanceable 贴片，show 不抢焦点——
// set_visible 只对 main set_focus，orb/widget 天然不聚焦）
visibility_command!(show_orb, "orb", true);
visibility_command!(hide_orb, "orb", false);

#[tauri::command]
pub fn toggle_widget(app: tauri::AppHandle) -> Result<(), String> {
    toggle(&app, WIDGET_LABEL).map(|_| ())
}

/// 悬浮球显隐切换（主界面顶栏 Orbit 按钮起；orb 是 glanceable
/// 贴片，show 不抢焦点）。返回切换后状态（前端按钮态以事件广播为准）。
#[tauri::command]
pub fn toggle_orb(app: tauri::AppHandle) -> Result<bool, String> {
    toggle(&app, ORB_LABEL)
}

#[tauri::command]
pub fn toggle_main(app: tauri::AppHandle) -> Result<(), String> {
    toggle(&app, MAIN_LABEL).map(|_| ())
}

#[tauri::command]
pub fn window_ready(window: tauri::WebviewWindow) {
    ready(&window);
}
