//! 窗口可见性原语。
//!
//! 单一源在 AppState.widget_visible / main_visible / orb_visible / timeline_visible；所有变更路径
//! （前端命令、托盘菜单、托盘左键、主窗口关闭钮）都汇入这里，统一：
//! show/hide 窗口 → 更新标志 → 落盘 → 同步托盘勾选态 → 广播事件。
//!
//! 事件：`widget-visibility-changed` / `main-visibility-changed` /
//! `orb-visibility-changed` / `timeline-visibility-changed`，载荷为 bool。
//! 前端不维护本地真相，按钮状态以 get_visibility 初值 + 事件跟随为准。

use std::sync::atomic::Ordering;

use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use crate::tray;
use crate::window_state;
use crate::AppState;

pub const WIDGET_LABEL: &str = "widget";
pub const MAIN_LABEL: &str = "main";
/// 悬浮球窗口（默认隐藏，设置页/托盘开启）。
pub const ORB_LABEL: &str = "orb";
/// 项目推进时间轴窗口（默认隐藏，设置页/托盘开启）。
pub const TIMELINE_LABEL: &str = "timeline";

fn label_ok(label: &str) -> bool {
    label == WIDGET_LABEL || label == MAIN_LABEL || label == ORB_LABEL || label == TIMELINE_LABEL
}

fn is_visible(state: &AppState, label: &str) -> bool {
    match label {
        WIDGET_LABEL => state.widget_visible.load(Ordering::SeqCst),
        MAIN_LABEL => state.main_visible.load(Ordering::SeqCst),
        ORB_LABEL => state.orb_visible.load(Ordering::SeqCst),
        TIMELINE_LABEL => state.timeline_visible.load(Ordering::SeqCst),
        _ => false,
    }
}

fn store_visible(state: &AppState, label: &str, visible: bool) {
    match label {
        WIDGET_LABEL => state.widget_visible.store(visible, Ordering::SeqCst),
        MAIN_LABEL => state.main_visible.store(visible, Ordering::SeqCst),
        ORB_LABEL => state.orb_visible.store(visible, Ordering::SeqCst),
        TIMELINE_LABEL => state.timeline_visible.store(visible, Ordering::SeqCst),
        _ => {}
    }
}

fn event_name(label: &str) -> &'static str {
    match label {
        WIDGET_LABEL => "widget-visibility-changed",
        ORB_LABEL => "orb-visibility-changed",
        TIMELINE_LABEL => "timeline-visibility-changed",
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
        // 时间轴从托盘 / 设置召回时,窥视态（几像素细边）回完整条态,免得「显示了却看不见」
        if label == TIMELINE_LABEL {
            crate::timeline_form::unpeek(app);
        }
    } else {
        window.hide().map_err(|e| e.to_string())?;
    }
    // 悬浮球的「指针让出」态跨显隐不可残留——隐藏期间没有鼠标
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

/// 切换可见性，返回切换后的状态。
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
/// 窗口带 visible 创建会画白首帧（tauri#4881 家族），
/// 所以所有窗口都 visible:false 创建，show 的时机统一到这里。
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
        // orb 补显隐钩子（与 set_visible 同款）：tao 的 show 会 apply_diff 把整组
        // 带框样式写回并触发框架重算（region 被重置回整窗）——托盘召回走
        // set_visible 有钩子兜，启动首秀走这里也必须有，两条路径副作用一致。
        if label == ORB_LABEL {
            crate::orb_dock::reset_pointer_pass(window, true);
        }
    }
}

/// 启动显示看门狗（setup 末尾 spawn）：前端 window_ready 缺席的兜底。
///
/// 背景：首次显示的唯一路径是「前端首帧 → window_ready IPC → ready show」。
/// 开机自启时系统忙，WebView2 逐窗初始化慢、页面加载与 IPC 往返都可能缺席或
/// 迟到（orb 还多一层 get_orb_form 前置），这条链一旦断掉，后端没有任何机制
/// 会发现「标志说可见、窗口实际没显示」——悬浮球就停在不可，直到用户手动
/// 托盘开关（自启后托盘在、悬浮球不在）。手动启动 WebView2 秒级
/// 就绪，链路瞬间闭合，所以从未暴露。
///
/// 做法：延迟两轮（8s / 25s）核对全部窗口——「可见性标志 = true 但窗口实际
/// 隐藏」即统一 set_visible（true) 补显示（含 orb 钩子 / 落盘 / 托盘同步 /
/// 事件广播）。安全性：前端正常路径（毫秒级 ready）下窗口已显示，核对跳过、
/// 零干预；用户若先手动隐藏（标志翻 false）自动豁免；两轮后不再干预。
pub fn spawn_show_watchdog(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        // 第一轮 8s 兜住大多数慢启动；第二轮累计 25s 兜极端慢（登入瞬间
        // 磁盘/CPU 饱和、杀软扫描）。再晚的缺席说明前端已死透，强 show 也
        // 只是空窗，交给用户手动处理。
        const CHECK_DELAYS_SECS: [u64; 2] = [8, 17];
        for delay in CHECK_DELAYS_SECS {
            std::thread::sleep(std::time::Duration::from_secs(delay));
            let Some(state) = app.try_state::<AppState>() else { return };
            for label in [WIDGET_LABEL, MAIN_LABEL, ORB_LABEL, TIMELINE_LABEL] {
                if !is_visible(&state, label) {
                    continue;
                }
                let Some(window) = app.get_webview_window(label) else { continue };
                if window.is_visible().unwrap_or(true) {
                    continue;
                }
                crate::dev_log!("[watchdog] {} flagged visible but hidden, forcing show", label);
                if let Err(e) = set_visible(&app, label, true) {
                    crate::dev_log!("[watchdog] force show {} failed: {}", label, e);
                }
            }
        }
    });
}

#[tauri::command]
pub fn get_visibility(state: tauri::State<'_, AppState>) -> serde_json::Value {
    serde_json::json!({
        "widget": state.widget_visible.load(Ordering::SeqCst),
        "main": state.main_visible.load(Ordering::SeqCst),
        "orb": state.orb_visible.load(Ordering::SeqCst),
        "timeline": state.timeline_visible.load(Ordering::SeqCst),
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
// 时间轴显隐（托盘与设置页共用；看板不抢焦点——set_visible 只对 main set_focus）
visibility_command!(show_timeline, "timeline", true);
visibility_command!(hide_timeline, "timeline", false);

/// 时间轴显隐切换（托盘勾选走 toggle；返回切换后状态）。
#[tauri::command]
pub fn toggle_timeline(app: tauri::AppHandle) -> Result<bool, String> {
    toggle(&app, TIMELINE_LABEL)
}

#[tauri::command]
pub fn toggle_widget(app: tauri::AppHandle) -> Result<(), String> {
    toggle(&app, WIDGET_LABEL).map(|_| ())
}

/// 悬浮球显隐切换（主界面顶栏 Orbit 按钮；orb 是 glanceable
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
