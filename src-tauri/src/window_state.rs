//! 窗口几何与可见性持久化（window-state.json）。
//!
//! 单一源在 AppState 的各窗口可见性原子标志与形态状态；本模块只负责：
//! - restore：启动时读取 window-state.json，恢复各窗口位置/尺寸 + 填充标志
//!   （**不直接 show**——显示统一走 window_ready，由前端首帧提交后裁决，保白闪对策）；
//! - persist：把当前几何写回（Moved/Resized 事件 2s 节流 + 显隐变更时即时写）。
//!
//! 边界校验：保存的矩形与任一显示器无交集时视为脏数据（拔显示器/DPI 变更），
//! 回退为居中，避免窗口恢复到屏幕外。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize};

use crate::AppState;

const PERSIST_MIN_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct WindowGeom {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct WindowStateFile {
    widget: Option<WindowGeom>,
    main: Option<WindowGeom>,
    /// 悬浮球几何（serde default 向后兼容旧文件）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    orb: Option<WindowGeom>,
    #[serde(default = "default_true")]
    widget_visible: bool,
    #[serde(default)]
    main_visible: bool,
    /// 悬浮球可见性（默认 false——新窗口形态默认不弹）。
    #[serde(default, skip_serializing_if = "is_false")]
    orb_visible: bool,
    /// 挂件吸附状态（None = 自由态；serde default 向后兼容旧文件）。
    /// 枚举类型化：坏值（如 edge:"bogus"）在整体反序列化时失败 → 走剥离
    /// 重试路径（几何保住、吸附回自由态），写侧由类型保证无坏值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    widget_snap: Option<crate::snap::SnapState>,
    /// 吸附开关（应急停用杠杆，设置页可改；default true）。
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    widget_snap_enabled: bool,
    /// 悬浮球贴边停靠态（拖到屏幕边缘松手→收起竖条贴边;serde default 向后兼容）。
    /// None = 自由态。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    orb_dock: Option<crate::orb_dock::OrbDockState>,
    /// 悬浮球启动形态（启动恢复上次退出前的形态与位置,首次默认表盘;
    /// serde default 向后兼容——旧文件缺失时按落盘尺寸反推形态）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    orb_expanded: Option<bool>,
    /// 项目推进时间轴几何（serde default 向后兼容旧文件）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeline: Option<WindowGeom>,
    /// 时间轴可见性（默认 false——新窗口默认不弹）。
    #[serde(default, skip_serializing_if = "is_false")]
    timeline_visible: bool,
    /// 时间轴形态（board 看板 / strip 条态），由 set_timeline_form 写入、restore 恢复上次形态。
    /// 枚举类型化：坏值整体反序列化失败 → 走剥离重试路径回 None（看板态）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeline_form: Option<TimelineForm>,
    /// 条态水平位置（物理像素 x；屏归属按看板态所在屏）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeline_strip_x: Option<i32>,
    /// 条态内容宽（CSS 像素，前端量出）——启动恢复条态时首帧即用上次宽度，
    /// 不等前端数据加载再跳一次宽。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeline_strip_w: Option<f64>,
}

/// 时间轴两态（单窗口两态）。运行时单一源 = `AppState.timeline_form`，
/// 执行者 = `timeline_form:set_form`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineForm {
    Board,
    Strip,
}

fn is_true(v: &bool) -> bool {
    *v
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn default_true() -> bool {
    true
}

impl Default for WindowStateFile {
    fn default() -> Self {
        // 首次启动（无状态文件）：挂件可见、主窗口隐藏（产品核心是挂件常驻）；
        // 悬浮球 / 时间轴默认隐藏，托盘/设置页开启
        Self {
            widget: None,
            main: None,
            orb: None,
            widget_visible: true,
            main_visible: false,
            orb_visible: false,
            widget_snap: None,
            widget_snap_enabled: true,
            orb_dock: None,
            orb_expanded: None,
            timeline: None,
            timeline_visible: false,
            timeline_form: None,
            timeline_strip_x: None,
            timeline_strip_w: None,
        }
    }
}

fn state_path(app: &AppHandle) -> Option<PathBuf> {
    Some(crate::data_root::current(app).ok()?.window_state_path())
}

fn load(app: &AppHandle) -> WindowStateFile {
    let Some(path) = state_path(app) else { return WindowStateFile::default() };
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str(&raw) {
            Ok(file) => file,
            // 读取防御：枚举类型化字段坏值不应拖垮几何恢复——剥离这些
            // 字段重试（JSON Value 侧删键），仍失败才回默认。
            Err(_) => strip_snap_fields_retry(&raw).unwrap_or_default(),
        },
        Err(_) => WindowStateFile::default(),
    }
}

/// 剥离吸附与时间轴形态字段后重试解析：它们是枚举类型化字段，坏值会让整文件反序列化失败、
/// 拖垮全部几何恢复。`orb_snap` 已无对应字段，保留在列表里无害。
fn strip_snap_fields_retry(raw: &str) -> Option<WindowStateFile> {
    let mut value: serde_json::Value = serde_json::from_str(raw).ok()?;
    if let Some(obj) = value.as_object_mut() {
        for key in ["widget_snap", "orb_snap", "widget_snap_enabled", "timeline_form", "timeline_strip_x", "timeline_strip_w"] {
            obj.remove(key);
        }
    }
    serde_json::from_value(value).ok()
}

/// 保存的矩形是否与任一显示器相交（物理像素）。
fn rect_on_any_monitor(app: &AppHandle, geom: &WindowGeom) -> bool {
    let Ok(monitors) = app.available_monitors() else { return false };
    for m in monitors {
        let PhysicalPosition { x, y } = *m.position();
        let PhysicalSize { width, height } = *m.size();
        let (x, y) = (x as i64, y as i64);
        let (w, h) = (width as i64, height as i64);
        let intersects = (geom.x as i64) < x + w
            && (geom.x as i64) + geom.width as i64 > x
            && (geom.y as i64) < y + h
            && (geom.y as i64) + geom.height as i64 > y;
        if intersects {
            return true;
        }
    }
    false
}

fn read_geom(window: &tauri::WebviewWindow) -> Option<WindowGeom> {
    // 最小化时 Windows 会把窗口挪到屏幕外（-32000），此时的几何是脏数据
    if window.is_minimized().unwrap_or(true) {
        return None;
    }
    // 最大化几何是「向工作区外溢一圈边框」的临时态（如 -40,-167 / 2618×1690 vs 工作区
    // 2560×1368），落盘后下次启动被当普通尺寸恢复 → 非 maximized 态却顶满屏幕。返回 None =
    // persist 保留上次落盘的还原几何；unmaximize 后的 Resized 事件正常落盘。
    if window.is_maximized().unwrap_or(false) {
        return None;
    }
    let PhysicalPosition { x, y } = window.outer_position().ok()?;
    // 尺寸必须记 inner（client）——set_size 的语义就是设置 inner；若记 outer，
    // 每次「restore→set_size」会把边框再叠一层，decorated 窗口每会话膨胀一圈。
    let PhysicalSize { width, height } = window.inner_size().ok()?;
    Some(WindowGeom { x, y, width, height })
}

/// 记录几何是否为「最大化污染」：
/// 记录矩形中心所在显示器的**工作区**双维都被超出——最大化 inner 恒 ≥ 工作区
/// （覆盖工作区再外溢边框），而自由态大窗口极难双维同时超工作区（保守判据，
/// 单维超出的窗口不受影响）。拿不到工作区/非 Windows → false（宁可不判）。
#[cfg(windows)]
fn geom_is_maximized_pollution(geom: &WindowGeom) -> bool {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    let cx = geom.x + geom.width as i32 / 2;
    let cy = geom.y + geom.height as i32 / 2;
    let monitor =
        unsafe { MonitorFromPoint(POINT { x: cx, y: cy }, MONITOR_DEFAULTTONEAREST) };
    let empty = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        rcMonitor: empty,
        rcWork: empty,
        dwFlags: 0,
    };
    unsafe {
        if monitor.is_null() || GetMonitorInfoW(monitor, &mut info) == 0 {
            return false;
        }
    }
    let work_w = (info.rcWork.right - info.rcWork.left) as u32;
    let work_h = (info.rcWork.bottom - info.rcWork.top) as u32;
    geom.width > work_w && geom.height > work_h
}

#[cfg(not(windows))]
fn geom_is_maximized_pollution(_geom: &WindowGeom) -> bool {
    false
}

/// 启动恢复：几何 + 可见性标志。在 setup 中、托盘创建前调用。
pub fn restore(app: &AppHandle) {
    let file = load(app);
    let pairs: [(&str, Option<WindowGeom>); 4] = [
        ("widget", file.widget),
        ("main", file.main),
        ("orb", file.orb),
        // timeline 先按看板几何走通用恢复（位置 + 尺寸 + 越界回中）——条态落盘时
        // 这里存的也是看板几何；上次是条态则 setup 里 timeline_form:restore 再折条
        ("timeline", file.timeline),
    ];
    // orb 的最终形态（restore_orb 决定）——循环后与其余字段一起写进 AppState
    #[allow(unused_mut)]
    let mut orb_expanded: Option<bool> = None;
    for (label, geom) in pairs {
        let Some(window) = app.get_webview_window(label) else { continue };
        // orb 走专用恢复（恢复上次退出前的贴边位置,首次默认表盘;未贴边一律表盘,
        // 竖条只属于贴边）——几何为 None（首次启动）也要进,不能按「无记录跳过」。
        if label == "orb" {
            #[cfg(windows)]
            {
                orb_expanded = Some(crate::orb_dock::restore_orb(
                    &window,
                    geom.map(|g| (g.x, g.y, g.width, g.height)),
                    file.orb_expanded,
                    file.orb_dock,
                ));
            }
            #[cfg(not(windows))]
            if let Some(g) = geom {
                // 非 Windows 无显示器模型：只回记录位置（形态由前端默认）
                let _ = window.set_position(PhysicalPosition::new(g.x, g.y));
            }
            continue;
        }
        let Some(geom) = geom else { continue };
        // 最大化污染的存量脏数据（persist 拦截前落过盘）→ 不应用记录几何，
        // 保留 conf 初始尺寸（main = 1120×720）并居中——persist 侧已拦新增。
        if geom_is_maximized_pollution(&geom) {
            let _ = window.center();
            continue;
        }
        let on_screen = rect_on_any_monitor(app, &geom);
        let _ = window.set_position(PhysicalPosition::new(geom.x, geom.y));
        let _ = window.set_size(PhysicalSize::new(geom.width, geom.height));
        if !on_screen {
            // 脏位置：先落位再居中，避免窗口留在屏幕外
            let _ = window.center();
        }
    }
    if let Some(state) = app.try_state::<AppState>() {
        state.widget_visible.store(file.widget_visible, std::sync::atomic::Ordering::SeqCst);
        state.main_visible.store(file.main_visible, std::sync::atomic::Ordering::SeqCst);
        state.orb_visible.store(file.orb_visible, std::sync::atomic::Ordering::SeqCst);
        state
            .timeline_visible
            .store(file.timeline_visible, std::sync::atomic::Ordering::SeqCst);
        // 吸附状态与开关随文件装载（坏值已在 load 剥离重试中回自由态）
        *state.widget_snap.lock().unwrap() = file.widget_snap;
        state
            .widget_snap_enabled
            .store(file.widget_snap_enabled, std::sync::atomic::Ordering::SeqCst);
        // 贴边停靠状态随文件装载（重启后 OrbWindow 挂载查询回
        // 收起态;几何归位在前端按当前工作区重算——restore 只归位 orb 位置）
        *state.orb_dock.lock().unwrap() = file.orb_dock;
        // 时间轴形态状态装载。看板几何取恢复后的窗口值（越界回中已生效）；
        // 窗口拿不到时退回记录值
        {
            let board = app
                .get_webview_window("timeline")
                .and_then(|w| read_geom(&w))
                .or(file.timeline)
                .map(|g| crate::timeline_form::Rect { x: g.x, y: g.y, width: g.width, height: g.height });
            *state.timeline_form.lock().unwrap() = crate::timeline_form::FormState {
                form: file.timeline_form.unwrap_or(TimelineForm::Board),
                board,
                strip_x: file.timeline_strip_x,
                strip_w: file.timeline_strip_w,
                ..Default::default()
            };
        }
        // 悬浮球启动形态（restore_orb 结果:贴边 = 竖条 false、未贴边/首次 = 表盘 true）
        // ——前端挂载按它对齐（Rust 侧几何已先按此恢复）。
        if let Some(e) = orb_expanded {
            state.orb_expanded.store(e, std::sync::atomic::Ordering::SeqCst);
        }
        // 装载完成 → 放开 persist
        state.window_state_restored.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 即时持久化（显隐变更 / 退出前调用）。读改写合并：最小化窗口的几何是
/// 脏数据（read_geom 返回 None），保留上次落盘的值而不是丢弃。
/// snap 字段走 AppState 内存值（子类化线程经 set_widget_snap 更新），
/// 不从窗口读——吸附语义不属于几何采样。
pub fn persist(app: &AppHandle, state: &AppState) {
    // 装载前拒写：restore 途中的归位写入 / 抢跑的事件与命令都可能走到这里,
    // 此刻 AppState 还是初值,写下去就把上次的可见性、贴边态、形态全抹成默认值
    if !state.window_state_restored.load(std::sync::atomic::Ordering::SeqCst) {
        crate::dev_log!("[window-state] persist skipped: state not restored yet");
        return;
    }
    let mut file = load(app);
    if let Some(w) = app.get_webview_window("widget") {
        if let Some(geom) = read_geom(&w) {
            file.widget = Some(geom);
        }
    }
    if let Some(w) = app.get_webview_window("main") {
        if let Some(geom) = read_geom(&w) {
            file.main = Some(geom);
        }
    }
    if let Some(w) = app.get_webview_window("orb") {
        if let Some(geom) = read_geom(&w) {
            file.orb = Some(geom);
        }
    }
    // 条态时窗口几何是细条，不能写进看板几何——改写折条时记下的看板矩形
    {
        let tl = *state.timeline_form.lock().unwrap();
        match tl.form {
            TimelineForm::Board => {
                if let Some(geom) = app.get_webview_window("timeline").as_ref().and_then(read_geom) {
                    file.timeline = Some(geom);
                }
            }
            TimelineForm::Strip => {
                if let Some(b) = tl.board {
                    file.timeline = Some(WindowGeom { x: b.x, y: b.y, width: b.width, height: b.height });
                }
            }
        }
        file.timeline_form = Some(tl.form);
        file.timeline_strip_x = tl.strip_x;
        file.timeline_strip_w = tl.strip_w;
    }
    file.widget_visible = state.widget_visible.load(std::sync::atomic::Ordering::SeqCst);
    file.main_visible = state.main_visible.load(std::sync::atomic::Ordering::SeqCst);
    file.orb_visible = state.orb_visible.load(std::sync::atomic::Ordering::SeqCst);
    file.timeline_visible = state.timeline_visible.load(std::sync::atomic::Ordering::SeqCst);
    file.widget_snap = state.widget_snap.lock().unwrap().clone();
    file.widget_snap_enabled = state
        .widget_snap_enabled
        .load(std::sync::atomic::Ordering::SeqCst);
    file.orb_dock = state.orb_dock.lock().unwrap().clone();
    file.orb_expanded = Some(state.orb_expanded.load(std::sync::atomic::Ordering::SeqCst));
    let Some(path) = state_path(app) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(raw) = serde_json::to_string_pretty(&file) {
        let _ = std::fs::write(&path, raw);
    }
}

/// Moved/Resized 高频事件的节流持久化（拖动期间每像素都触发，直接写盘太浪费）。
/// 挂件尺寸由 set_widget_size 恢复路径兜底（同样触发 Resized → 此处落盘）。
pub fn persist_throttled(app: &AppHandle, state: &AppState) {
    {
        let mut last = state.last_persist.lock().unwrap();
        if let Some(t) = *last {
            if t.elapsed() < PERSIST_MIN_INTERVAL {
                return;
            }
        }
        *last = Some(Instant::now());
    }
    persist(app, state);
}

// ---------- 吸附状态访问（snap 子类化线程调用；单一源 = AppState） ----------

/// 吸附开关（应急停用杠杆；文件缺省 = 开；仅挂件参与格网吸附，
/// 悬浮球不格网化，此开关与 orb 无关）。
pub fn widget_snap_enabled(app: &AppHandle) -> bool {
    app.state::<AppState>()
        .widget_snap_enabled
        .load(std::sync::atomic::Ordering::SeqCst)
}

/// 指定窗口的吸附状态（None = 自由态；只有 widget 参与吸附，其余 label 回 None）。
pub fn snap_state_for(app: &AppHandle, label: &str) -> Option<crate::snap::SnapState> {
    let state = app.state::<AppState>();
    let guard = match label {
        "widget" => state.widget_snap.lock().unwrap(),
        _ => return None,
    };
    guard.clone()
}

/// 更新指定窗口的吸附状态并即时落盘（吸附是低频离散事件，不走节流；落盘失败
/// 静默——下次显隐/退出兜底重写）。写者 = snap 子类化线程（仅 widget）。
pub fn set_snap_state(
    app: &AppHandle,
    label: &str,
    snap: Option<crate::snap::SnapState>,
) {
    let state = app.state::<AppState>();
    match label {
        "widget" => *state.widget_snap.lock().unwrap() = snap,
        _ => return,
    }
    persist(app, &state);
}
