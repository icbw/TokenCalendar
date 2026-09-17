//! 时间轴两态：看板（board）↔ 条态（strip）。
//!
//! **形态切换一个执行者**（硬约束 4）：`set_timeline_form` 在 Rust 侧原子完成
//! 尺寸 + 位置 + alwaysOnTop + 可缩放锁,前端只发意图、按 `timeline-form-changed` 跟随,
//! 不自行补偿位置。
//!
//! - 看板态：自由窗口,不置顶,可缩放（min 480×200）;几何由 window_state 通用路径持久化。
//! - 条态：贴**看板所在显示器**工作区顶缘的细条,高 `STRIP_H_LOGICAL`,宽 = 前端量出的
//!   内容 CSS 宽（钳到工作区宽）;水平位置记 `strip_x`（物理像素绝对坐标,不在本屏则按
//!   看板中心居中）;置顶、不可缩放（同时让 tauri drag-region 的双击最大化失效——
//!   `internal_toggle_maximize` 只对 resizable 窗口生效,双击留给前端「展开」）。
//! - 条态拖动：子类化 `WM_MOVING` 把窗口钉在光标所在屏的顶缘,只能横向滑动;
//!   `WM_EXITSIZEMOVE` 落定 `strip_x` 并落盘。拖到另一块屏 → 看板几何随之平移到该屏
//!   （保持「条与看板同屏」不变式,于是重启恢复与展开都不会跑回旧屏）。
//!
//! 多显示器口径（红线）：「显示器枚举 + 该屏 rcWork + 该屏 scale」,不用窗口缓存
//! scale;scale 乘文本缩放（text_scale.rs：WebView 内容按 DPI × 文本大小渲染,CSS 像素换
//! 物理必须带上它——跟随系统文本大小,不用 set_zoom 抵消）。
//! 窥视态 peek：条态的第二档——条态 5 秒无操作（指针不在、无亮起项目,前端计时）
//! 收成贴顶的短小把手（`PEEK_W_LOGICAL` × `PEEK_H_LOGICAL`,居中于原条;窗口内 CSS 画 4px 半透明条 + 窄阴影,
//! 整条宽的透明边白底下看不见、又宽易误触 → 缩窄 + 加阴影）,不挡全屏窗口;指针移入 / 出现亮起项目 / 托盘显示 → 回完整条态。
//! peek 是运行时子态,不落盘（持久化形态仍是 strip,重启回完整条态再计时）;对前端表现为第三个形态名 "peek"。
//!
//! 遮挡自动折：时间轴不进任务栏,被遮住就难召回——看板态下后台线程每
//! `OCCLUSION_POLL` 查一次前台窗口,前台窗口（非本窗口、非桌面 / 任务栏 / 任务视图外壳）覆盖看板所在
//! 显示器工作区（全屏或最大化）→ 立即折成条态（条态置顶,随后按上面的规则收成 peek）。
//!
//! 窗口风格：看板态按 `floating` 施加 Shadow（主窗口组合）或 Flat 边缘组合,条态一律
//! Flat;前端挂载与设置变更时经 `set_timeline_style` 下发,形态切换时本文件同步重施。
//! **不复用 orb_dock.rs**：只有一种形态一条边,几何全部是本文件的纯函数（有单测）。

use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

use crate::window_state::TimelineForm;
use crate::AppState;

/// 条态高度（CSS / 逻辑像素;~36,首期常量不进设置）。
/// 条态加与窥视把手同款窄阴影 → 窗口高 = 36 可见条 + 5 下方阴影透明边
/// （左右各 5 的阴影边由前端量宽时计入;与 timeline.css `--tl-edge-pad` 同源）。
pub const STRIP_H_LOGICAL: f64 = 41.0;
/// 窥视态窗口尺寸（CSS / 逻辑像素）:宽 = 短把手;高 = 4px 可见条 + 下方 / 两侧留给 CSS 窄阴影的透明边。
/// 与 timeline.css `.is-peek` 的把手几何同源,改一处两处同改。
pub const PEEK_W_LOGICAL: f64 = 88.0;
pub const PEEK_H_LOGICAL: f64 = 9.0;
/// 遮挡检测轮询间隔（看板态且可见时才做实际检查）。
const OCCLUSION_POLL: std::time::Duration = std::time::Duration::from_millis(500);
/// 条态宽度下限（前端未量出 / 空项目时的兜底;CSS 像素）。
pub const STRIP_MIN_W_LOGICAL: f64 = 160.0;
/// 条态宽度缺省（首次折条前端尚未量出时）。
pub const STRIP_DEFAULT_W_LOGICAL: f64 = 480.0;
/// 看板态最小尺寸（与 chrome:apply_timeline_chrome 同值,切回看板时恢复）。
const BOARD_MIN_W_LOGICAL: f64 = 480.0;
const BOARD_MIN_H_LOGICAL: f64 = 200.0;

/// 物理像素矩形（x, y, w, h）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// 运行时形态状态（AppState 单一源;window_state restore 填充、persist 消费）。
#[derive(Debug, Clone, Copy)]
pub struct FormState {
    pub form: TimelineForm,
    /// 看板态几何。
    pub board: Option<Rect>,
    /// 条态左缘（物理像素绝对坐标）。
    pub strip_x: Option<i32>,
    /// 条态内容宽（CSS 像素;前端量出后随 set_timeline_form 传入）。
    pub strip_w: Option<f64>,
    /// 看板态窗口风格:true = Shadow（DWM 阴影 + 呼吸位）/ false = Flat。运行时值,前端 prefs 下发。
    pub floating: bool,
    /// 条态子态:true = 窥视态（几像素细边）。运行时值,不落盘;离开条态即复位。
    pub peek: bool,
}

impl Default for FormState {
    fn default() -> Self {
        Self { form: TimelineForm::Board, board: None, strip_x: None, strip_w: None, floating: true, peek: false }
    }
}

/// 显示器：完整矩形 + 工作区（物理像素,[l, t, r, b]）+ 该屏 scale（已乘文本缩放）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Screen {
    pub rect: [i32; 4],
    pub work: [i32; 4],
    pub scale: f64,
}

// ---------- 纯几何（单测覆盖） ----------

/// 点落在哪台显示器：矩形命中优先,全不命中取中心最近者。
pub fn screen_at(screens: &[Screen], x: i32, y: i32) -> Option<usize> {
    if screens.is_empty() {
        return None;
    }
    if let Some(i) = screens
        .iter()
        .position(|s| x >= s.rect[0] && x < s.rect[2] && y >= s.rect[1] && y < s.rect[3])
    {
        return Some(i);
    }
    let dist = |s: &Screen| {
        let cx = (s.rect[0] as i64 + s.rect[2] as i64) / 2;
        let cy = (s.rect[1] as i64 + s.rect[3] as i64) / 2;
        (cx - x as i64).pow(2) + (cy - y as i64).pow(2)
    };
    (0..screens.len()).min_by_key(|&i| dist(&screens[i]))
}

/// 条态矩形：贴 `screen` 工作区顶缘;宽 = CSS 宽 × scale（钳到工作区宽）;
/// x = 记住的 `strip_x`（左缘落在本屏工作区内才采用）否则以 `anchor_cx` 居中;最后钳进工作区。
pub fn strip_rect(screen: &Screen, css_w: f64, strip_x: Option<i32>, anchor_cx: i32) -> Rect {
    let [wl, wt, wr, _] = screen.work;
    let work_w = (wr - wl).max(1);
    let w = ((css_w.max(STRIP_MIN_W_LOGICAL) * screen.scale).round() as i32).clamp(1, work_w);
    let h = ((STRIP_H_LOGICAL * screen.scale).round() as i32).max(1);
    let x = match strip_x {
        Some(x) if x >= wl && x < wr => x,
        _ => anchor_cx - w / 2,
    };
    let x = x.clamp(wl, wr - w);
    Rect { x, y: wt, width: w as u32, height: h as u32 }
}

/// 看板矩形平移到另一块屏：保持相对工作区左上的偏移与逻辑尺寸（按两屏 scale 比换算）,
/// 再钳进目标工作区（放不下时左上对齐工作区）。
pub fn move_board_to(board: Rect, from: &Screen, to: &Screen) -> Rect {
    let ratio = if from.scale > 0.0 { to.scale / from.scale } else { 1.0 };
    let [tl, tt, tr, tb] = to.work;
    let w = ((board.width as f64 * ratio).round() as i32).clamp(1, (tr - tl).max(1));
    let h = ((board.height as f64 * ratio).round() as i32).clamp(1, (tb - tt).max(1));
    let dx = ((board.x - from.work[0]) as f64 * ratio).round() as i32;
    let dy = ((board.y - from.work[1]) as f64 * ratio).round() as i32;
    let x = (tl + dx).clamp(tl, tr - w);
    let y = (tt + dy).clamp(tt, tb - h);
    Rect { x, y, width: w as u32, height: h as u32 }
}

/// 窥视态：贴原条顶缘、以原条中心居中的短把手（宽不超过原条）。
pub fn peek_rect(strip: Rect, scale: f64) -> Rect {
    let w = ((PEEK_W_LOGICAL * scale).round() as u32).clamp(1, strip.width.max(1));
    let h = ((PEEK_H_LOGICAL * scale).round() as u32).max(1);
    let x = strip.x + (strip.width as i32 - w as i32) / 2;
    Rect { x, y: strip.y, width: w, height: h }
}

/// 条态宽度更新（不切形态）：只在条态（含窥视态）且宽度有效、确有变化时记下并要求重施几何。
/// 看板态一律忽略——前端量宽回调与形态广播先后不定（展开时窗口先变大、`timeline-form-changed` 后到）,
/// 迟到的宽度更新不得把刚展开的看板折回条态（无监测组时展开即缩回）。
pub fn update_strip_width(st: &mut FormState, w: f64) -> bool {
    if st.form != TimelineForm::Strip || !w.is_finite() || w <= 0.0 {
        return false;
    }
    if st.strip_w.is_some_and(|cur| (cur - w).abs() < 1.0) {
        return false;
    }
    st.strip_w = Some(w);
    true
}

fn center(r: &Rect) -> (i32, i32) {
    (r.x + r.width as i32 / 2, r.y + r.height as i32 / 2)
}

// ---------- 显示器枚举 ----------

#[cfg(windows)]
fn monitor_work_at(x: i32, y: i32) -> Option<[i32; 4]> {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    let empty = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        rcMonitor: empty,
        rcWork: empty,
        dwFlags: 0,
    };
    unsafe {
        let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() || GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
    }
    let rc = info.rcWork;
    Some([rc.left, rc.top, rc.right, rc.bottom])
}

#[cfg(not(windows))]
fn monitor_work_at(_x: i32, _y: i32) -> Option<[i32; 4]> {
    None
}

/// 全部显示器（tauri 枚举给矩形与 scale,rcWork 走 Win32;scale 乘文本缩放）。
fn screens(window: &WebviewWindow) -> Vec<Screen> {
    let Ok(monitors) = window.available_monitors() else { return Vec::new() };
    monitors
        .iter()
        .map(|m| {
            let p = *m.position();
            let s = *m.size();
            let rect = [p.x, p.y, p.x + s.width as i32, p.y + s.height as i32];
            let work = monitor_work_at(p.x + s.width as i32 / 2, p.y + s.height as i32 / 2)
                .unwrap_or(rect);
            Screen { rect, work, scale: m.scale_factor() * crate::text_scale::factor() }
        })
        .collect()
}

fn window_rect(window: &WebviewWindow) -> Option<Rect> {
    let p = window.outer_position().ok()?;
    let s = window.outer_size().ok()?;
    Some(Rect { x: p.x, y: p.y, width: s.width, height: s.height })
}

// ---------- 执行 ----------

fn place(window: &WebviewWindow, r: Rect) {
    // 先位置后尺寸：跨 DPI 落位时系统先按 WM_DPICHANGED 建议矩形重排,再以目标尺寸覆盖
    let _ = window.set_position(PhysicalPosition::new(r.x, r.y));
    let _ = window.set_size(PhysicalSize::new(r.width, r.height));
}

/// 按当前状态施加条态几何（不改 form;调用方保证 form = Strip 或即将是）。
/// 返回施加的矩形。
fn apply_strip(window: &WebviewWindow, st: &FormState) -> Option<Rect> {
    let all = screens(window);
    let board = st.board.or_else(|| window_rect(window))?;
    let (cx, cy) = center(&board);
    let screen = all[screen_at(&all, cx, cy)?];
    let r = strip_rect(&screen, st.strip_w.unwrap_or(STRIP_DEFAULT_W_LOGICAL), st.strip_x, cx);
    let r = if st.peek { peek_rect(r, screen.scale) } else { r };
    crate::chrome::apply_timeline_style_chrome(window, false);
    let _ = window.set_min_size(None::<PhysicalSize<u32>>);
    let _ = window.set_resizable(false);
    let _ = window.set_maximizable(false);
    place(window, r);
    let _ = window.set_always_on_top(true);
    Some(r)
}

fn apply_board(window: &WebviewWindow, board: Option<Rect>, floating: bool) {
    crate::chrome::apply_timeline_style_chrome(window, floating);
    let _ = window.set_always_on_top(false);
    let _ = window.set_resizable(true);
    let _ = window.set_maximizable(true);
    if let Some(b) = board {
        place(window, b);
    }
    let _ = window.set_min_size(Some(tauri::LogicalSize::new(BOARD_MIN_W_LOGICAL, BOARD_MIN_H_LOGICAL)));
}

/// 对前端的形态名：board / strip / peek（peek = 条态 + 窥视子态）。
fn form_name(form: TimelineForm, peek: bool) -> &'static str {
    match form {
        TimelineForm::Board => "board",
        TimelineForm::Strip if peek => "peek",
        TimelineForm::Strip => "strip",
    }
}

/// 切换形态（或条态下更新宽度）。顺序约束（Moved/Resized 可能同步回调 persist）：
/// - 折条：先记看板几何 → 置 form = Strip → 施加条态几何（persist 此后读到 Strip,写记下的看板几何）;
/// - 展开：先施加看板几何（form 仍 Strip,persist 写记下的看板几何）→ 再置 form = Board。
/// `peek` 只对条态有意义（条态 ↔ 窥视态只改高度;看板态恒复位）。返回前端形态名。
pub fn set_form(app: &AppHandle, form: TimelineForm, peek: bool, strip_w: Option<f64>) -> Result<&'static str, String> {
    let window = app
        .get_webview_window(crate::visibility::TIMELINE_LABEL)
        .ok_or("timeline window not found")?;
    let state = app.state::<AppState>();
    let (prev, prev_peek) = {
        let st = state.timeline_form.lock().unwrap();
        (st.form, st.peek)
    };
    let prev_name = form_name(prev, prev_peek);
    // 窥视态只从条态进入：前端 peek 计时器迟到（期间已展开）时不得把看板折回去
    if form == TimelineForm::Strip && peek && prev == TimelineForm::Board {
        return Ok(prev_name);
    }
    match form {
        TimelineForm::Strip => {
            if prev == TimelineForm::Board && window.is_maximized().unwrap_or(false) {
                let _ = window.unmaximize();
            }
            let st = {
                let mut st = state.timeline_form.lock().unwrap();
                if prev == TimelineForm::Board {
                    if let Some(r) = window_rect(&window) {
                        st.board = Some(r);
                    }
                }
                if let Some(w) = strip_w.filter(|w| w.is_finite() && *w > 0.0) {
                    st.strip_w = Some(w);
                }
                st.form = TimelineForm::Strip;
                st.peek = peek;
                *st
            };
            let r = apply_strip(&window, &st);
            if form_name(form, peek) != prev_name {
                crate::dev_log!("[timeline-form] {} (from {}) -> {:?}", form_name(form, peek), prev_name, r);
            }
        }
        TimelineForm::Board => {
            if prev == TimelineForm::Board {
                return Ok(prev_name);
            }
            let (board, floating) = {
                let st = state.timeline_form.lock().unwrap();
                (st.board, st.floating)
            };
            apply_board(&window, board, floating);
            {
                let mut st = state.timeline_form.lock().unwrap();
                st.form = TimelineForm::Board;
                st.peek = false;
            }
            crate::dev_log!("[timeline-form] board -> {:?}", board);
        }
    }
    crate::window_state::persist(app, &state);
    let name = form_name(form, form == TimelineForm::Strip && peek);
    if name != prev_name {
        let _ = app.emit("timeline-form-changed", name);
    }
    Ok(name)
}

/// 召回（托盘 / 设置显示时间轴）:窥视态回完整条态;其余形态不动。
pub fn unpeek(app: &AppHandle) {
    let st = *app.state::<AppState>().timeline_form.lock().unwrap();
    if st.form == TimelineForm::Strip && st.peek {
        let _ = set_form(app, TimelineForm::Strip, false, None);
    }
}

/// 启动恢复（window_state:restore 装载状态后调用）：上次是条态就恢复条态。
pub fn restore(app: &AppHandle) {
    let state = app.state::<AppState>();
    let st = *state.timeline_form.lock().unwrap();
    if st.form != TimelineForm::Strip {
        return;
    }
    let Some(window) = app.get_webview_window(crate::visibility::TIMELINE_LABEL) else { return };
    let r = apply_strip(&window, &st);
    crate::dev_log!("[timeline-form] restore strip -> {:?}", r);
}

/// 条态下按当前环境重施几何（显示器 / 文本大小变化）;看板态无操作。
fn reapply_if_strip(app: &AppHandle) {
    let state = app.state::<AppState>();
    let st = *state.timeline_form.lock().unwrap();
    if st.form != TimelineForm::Strip {
        return;
    }
    if let Some(window) = app.get_webview_window(crate::visibility::TIMELINE_LABEL) {
        let _ = apply_strip(&window, &st);
    }
}

/// 条态拖动落定：按光标所在屏归位,记 strip_x;换屏则看板几何随之平移。
fn on_strip_drag_end(app: &AppHandle, cursor: (i32, i32)) {
    let state = app.state::<AppState>();
    let Some(window) = app.get_webview_window(crate::visibility::TIMELINE_LABEL) else { return };
    let all = screens(&window);
    let Some(cur) = window_rect(&window) else { return };
    let Some(to_i) = screen_at(&all, cursor.0, cursor.1) else { return };
    let to = all[to_i];
    let st = {
        let mut st = state.timeline_form.lock().unwrap();
        // 窥视态不记位置（把手几何 ≠ 条几何;指针移入已回完整条态,正常不会在 peek 下拖动）
        if st.form != TimelineForm::Strip || st.peek {
            return;
        }
        if let Some(board) = st.board {
            let (bx, by) = center(&board);
            if let Some(from_i) = screen_at(&all, bx, by) {
                if from_i != to_i {
                    st.board = Some(move_board_to(board, &all[from_i], &to));
                }
            }
        }
        // 先钳进目标屏再记（记录的是用户最终看到的位置）
        let r = strip_rect(&to, st.strip_w.unwrap_or(STRIP_DEFAULT_W_LOGICAL), Some(cur.x.clamp(to.work[0], to.work[2] - 1)), cur.x);
        st.strip_x = Some(r.x);
        *st
    };
    let r = apply_strip(&window, &st);
    crate::dev_log!("[timeline-form] strip drag end -> {:?}", r);
    crate::window_state::persist(app, &state);
}

// ---------- 命令 ----------

#[tauri::command]
pub fn get_timeline_form(state: tauri::State<'_, AppState>) -> &'static str {
    let st = state.timeline_form.lock().unwrap();
    form_name(st.form, st.peek)
}

/// 窗口风格下发（前端挂载 + 设置变更）:记运行时值,看板态立即重施边缘组合;条态保持 Flat,展开时生效。
#[tauri::command]
pub fn set_timeline_style(app: AppHandle, floating: bool) -> Result<(), String> {
    let window = app
        .get_webview_window(crate::visibility::TIMELINE_LABEL)
        .ok_or("timeline window not found")?;
    let state = app.state::<AppState>();
    let form = {
        let mut st = state.timeline_form.lock().unwrap();
        st.floating = floating;
        st.form
    };
    if form == TimelineForm::Board {
        crate::chrome::apply_timeline_style_chrome(&window, floating);
    }
    crate::dev_log!("[timeline-form] style floating={} (form {:?})", floating, form);
    Ok(())
}

/// `form` = "board" | "strip" | "peek";`strip_width` = 条态内容 CSS 宽（随形态切换一并带上;
/// 单纯的宽度更新走 `set_timeline_strip_width`,不经本命令）。
#[tauri::command]
pub fn set_timeline_form(app: AppHandle, form: String, strip_width: Option<f64>) -> Result<&'static str, String> {
    let form = match form.as_str() {
        "board" => (TimelineForm::Board, false),
        "strip" => (TimelineForm::Strip, false),
        "peek" => (TimelineForm::Strip, true),
        other => return Err(format!("unknown timeline form: {other}")),
    };
    set_form(&app, form.0, form.1, strip_width)
}

/// 条态内容宽更新（前端 ResizeObserver 量宽回调专用）：不切形态,看板态无操作（见 `update_strip_width`）。
/// 形态切换只走 `set_timeline_form`。
#[tauri::command]
pub fn set_timeline_strip_width(app: AppHandle, width: f64) -> Result<(), String> {
    let window = app
        .get_webview_window(crate::visibility::TIMELINE_LABEL)
        .ok_or("timeline window not found")?;
    let state = app.state::<AppState>();
    let st = {
        let mut st = state.timeline_form.lock().unwrap();
        if !update_strip_width(&mut st, width) {
            return Ok(());
        }
        *st
    };
    let _ = apply_strip(&window, &st);
    crate::window_state::persist(&app, &state);
    Ok(())
}

// ---------- 遮挡自动折 ----------

/// 看板态且可见时,前台窗口覆盖看板所在显示器工作区 → 折成条态（主线程执行,执行前复核形态）。
#[cfg(windows)]
fn spawn_occlusion_watch(app: AppHandle, window: WebviewWindow) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || loop {
        std::thread::sleep(OCCLUSION_POLL);
        let state = app.state::<AppState>();
        if !state.timeline_visible.load(Ordering::SeqCst) || state.timeline_form.lock().unwrap().form != TimelineForm::Board {
            continue;
        }
        let Ok(hwnd) = window.hwnd() else { continue };
        let Some(cover) = win::foreground_covering(hwnd.0 as isize) else { continue };
        let inner = app.clone();
        let _ = app.run_on_main_thread(move || {
            let board = inner.state::<AppState>().timeline_form.lock().unwrap().form == TimelineForm::Board;
            if board {
                crate::dev_log!("[timeline-form] covered by {} -> fold to strip", cover);
                let _ = set_form(&inner, TimelineForm::Strip, false, None);
            }
        });
        // 等主线程折完再查,避免同一次遮挡重复排队
        std::thread::sleep(OCCLUSION_POLL);
    });
}

// ---------- 子类化（条态拖动约束 + 环境变化重施） ----------

pub fn install(app: &AppHandle) {
    #[cfg(windows)]
    if let Some(window) = app.get_webview_window(crate::visibility::TIMELINE_LABEL) {
        win::install(&window);
        spawn_occlusion_watch(app.clone(), window);
    }
    #[cfg(not(windows))]
    let _ = app;
}

#[cfg(windows)]
mod win {
    use std::sync::OnceLock;
    use std::time::Duration;

    use tauri::{Manager, WebviewWindow};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    use super::{screen_at, screens, TimelineForm};

    const SUBCLASS_ID: usize = 0x544C_4653; // "TLFS"
    const WM_MOVING: u32 = 0x0216;
    const WM_EXITSIZEMOVE: u32 = 0x0232;
    const WM_SETTINGCHANGE: u32 = 0x001A;
    const WM_DISPLAYCHANGE: u32 = 0x007E;
    /// 设置变更后延迟重施：text_scale 的缓存由 orb 子类化在同一广播里刷新,
    /// 两个窗口收到广播的先后不定——等一拍再读系数。
    const REAPPLY_DELAY: Duration = Duration::from_millis(300);

    static WINDOW: OnceLock<WebviewWindow> = OnceLock::new();

    /// 前台窗口外壳类名：桌面 / 任务栏 / 任务视图 / Alt+Tab 等覆盖整屏但不是「遮住看板的应用窗口」。
    const SHELL_CLASSES: &[&str] = &[
        "Progman",
        "WorkerW",
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
        "MultitaskingViewFrame",
        "XamlExplorerHostIslandWindow",
        "ForegroundStaging",
        "Windows.UI.Core.CoreWindow",
    ];

    /// 前台窗口是否覆盖 `own`（时间轴窗口）所在显示器的工作区（全屏或最大化）。
    /// 覆盖 → 返回前台窗口类名;否则 None。
    pub fn foreground_covering(own: isize) -> Option<String> {
        use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow, GetWindowRect, IsIconic, IsWindowVisible};
        unsafe {
            let fg = GetForegroundWindow();
            if fg.is_null() || fg as isize == own || IsWindowVisible(fg) == 0 || IsIconic(fg) != 0 {
                return None;
            }
            let mut buf = [0u16; 128];
            let n = GetClassNameW(fg, buf.as_mut_ptr(), buf.len() as i32);
            let class = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
            if SHELL_CLASSES.contains(&class.as_str()) {
                return None;
            }
            let empty = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            let mut r = empty;
            if GetWindowRect(fg, &mut r) == 0 {
                return None;
            }
            let monitor = MonitorFromWindow(own as HWND, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, rcMonitor: empty, rcWork: empty, dwFlags: 0 };
            if monitor.is_null() || GetMonitorInfoW(monitor, &mut info) == 0 {
                return None;
            }
            let w = info.rcWork;
            (r.left <= w.left && r.top <= w.top && r.right >= w.right && r.bottom >= w.bottom).then_some(class)
        }
    }

    pub fn install(window: &WebviewWindow) {
        let Ok(hwnd) = window.hwnd() else {
            crate::dev_log!("[timeline-form] install failed: no hwnd");
            return;
        };
        let ok = unsafe { SetWindowSubclass(hwnd.0 as HWND, Some(subclass_proc), SUBCLASS_ID, 0) };
        if ok == 0 {
            crate::dev_log!("[timeline-form] SetWindowSubclass failed");
            return;
        }
        let _ = WINDOW.set(window.clone());
    }

    fn is_strip(window: &WebviewWindow) -> bool {
        window.app_handle().state::<crate::AppState>().timeline_form.lock().unwrap().form == TimelineForm::Strip
    }

    fn cursor() -> Option<(i32, i32)> {
        let mut p = POINT { x: 0, y: 0 };
        (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _ref_data: usize,
    ) -> LRESULT {
        if let Some(window) = WINDOW.get() {
            match msg {
                // 条态拖动：钉在光标所在屏的工作区顶缘,只横向滑动（高度保持系统当前值,
                // 跨 DPI 由 WM_DPICHANGED 重排、落定时再按目标屏重施）
                WM_MOVING if lparam != 0 && is_strip(window) => {
                    if let Some((cx, cy)) = cursor() {
                        let all = screens(window);
                        if let Some(i) = screen_at(&all, cx, cy) {
                            let rc = &mut *(lparam as *mut RECT);
                            let h = rc.bottom - rc.top;
                            rc.top = all[i].work[1];
                            rc.bottom = rc.top + h;
                        }
                    }
                    let r = DefSubclassProc(hwnd, msg, wparam, lparam);
                    return r.max(1);
                }
                WM_EXITSIZEMOVE if is_strip(window) => {
                    let r = DefSubclassProc(hwnd, msg, wparam, lparam);
                    if let Some(c) = cursor() {
                        super::on_strip_drag_end(window.app_handle(), c);
                    }
                    return r;
                }
                WM_SETTINGCHANGE | WM_DISPLAYCHANGE if is_strip(window) => {
                    let app = window.app_handle().clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(REAPPLY_DELAY);
                        let inner = app.clone();
                        let _ = app.run_on_main_thread(move || super::reapply_if_strip(&inner));
                    });
                }
                _ => {}
            }
        }
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scr(rect: [i32; 4], work: [i32; 4], scale: f64) -> Screen {
        Screen { rect, work, scale }
    }

    #[test]
    fn screen_at_hits_rect_then_nearest() {
        let a = scr([0, 0, 2560, 1440], [0, 0, 2560, 1392], 1.5);
        let b = scr([2560, 0, 4480, 1080], [2560, 0, 4480, 1040], 1.0);
        let all = [a, b];
        assert_eq!(screen_at(&all, 100, 100), Some(0));
        assert_eq!(screen_at(&all, 3000, 500), Some(1));
        // 两屏缝隙外（b 下方）→ 最近中心
        assert_eq!(screen_at(&all, 4000, 1300), Some(1));
        assert_eq!(screen_at(&[], 0, 0), None);
    }

    #[test]
    fn strip_sits_on_work_top_and_scales_with_screen() {
        // 工作区顶缘不是 0（任务栏在上）
        let s = scr([0, 0, 2560, 1440], [0, 48, 2560, 1440], 1.5);
        let r = strip_rect(&s, 400.0, None, 1280);
        assert_eq!(r.y, 48);
        assert_eq!(r.height, 62); // 41 × 1.5 = 61.5 → 62
        assert_eq!(r.width, 600);
        assert_eq!(r.x, 1280 - 300); // 无记忆 → 以看板中心居中
    }

    #[test]
    fn strip_keeps_remembered_x_on_same_screen_and_clamps() {
        let s = scr([0, 0, 1920, 1080], [0, 0, 1920, 1040], 1.0);
        assert_eq!(strip_rect(&s, 300.0, Some(200), 960).x, 200);
        // 记忆位置 + 宽度越过右缘 → 钳回
        assert_eq!(strip_rect(&s, 300.0, Some(1800), 960).x, 1620);
        // 记忆位置不在本屏（属于另一块屏）→ 按锚点居中
        assert_eq!(strip_rect(&s, 300.0, Some(2600), 960).x, 810);
        // 宽度超过工作区 → 钳到工作区宽,贴左
        let wide = strip_rect(&s, 5000.0, None, 960);
        assert_eq!((wide.x, wide.width), (0, 1920));
        // 过窄 → 下限
        assert_eq!(strip_rect(&s, 10.0, None, 960).width, STRIP_MIN_W_LOGICAL as u32);
    }

    #[test]
    fn strip_follows_text_scale_folded_into_screen_scale() {
        // 125% DPI × 150% 文本大小 = 1.875
        let s = scr([0, 0, 2560, 1440], [0, 0, 2560, 1392], 1.25 * 1.5);
        let r = strip_rect(&s, 320.0, None, 1280);
        assert_eq!(r.height, 77); // 41 × 1.875 = 76.875 → 77
        assert_eq!(r.width, 600);
    }

    #[test]
    fn peek_is_short_handle_centered_on_strip_top() {
        let s = scr([0, 0, 2560, 1440], [0, 48, 2560, 1440], 1.5);
        let strip = strip_rect(&s, 400.0, Some(300), 1280);
        let peek = peek_rect(strip, s.scale);
        assert_eq!(peek.y, strip.y);
        assert_eq!(peek.width, 132); // 88 × 1.5
        assert_eq!(peek.height, 14); // 9 × 1.5 = 13.5 → 14
        // 居中于原条
        assert_eq!(peek.x + peek.width as i32 / 2, strip.x + strip.width as i32 / 2);
        // 原条比把手还窄 → 不超过原条
        let narrow = Rect { width: 60, ..strip };
        assert_eq!(peek_rect(narrow, s.scale).width, 60);
    }

    #[test]
    fn strip_width_update_never_touches_board_form() {
        // 看板态：迟到的宽度更新被忽略,形态与记忆宽度都不变
        let mut st = FormState { strip_w: Some(158.0), ..FormState::default() };
        assert!(!update_strip_width(&mut st, 640.0));
        assert_eq!(st.form, TimelineForm::Board);
        assert_eq!(st.strip_w, Some(158.0));
        // 条态：有效且有变化才更新
        st.form = TimelineForm::Strip;
        assert!(update_strip_width(&mut st, 640.0));
        assert_eq!(st.strip_w, Some(640.0));
        assert!(!update_strip_width(&mut st, 640.4)); // 不足 1px 不重施
        assert!(!update_strip_width(&mut st, 0.0));
        assert!(!update_strip_width(&mut st, f64::NAN));
        assert_eq!(st.strip_w, Some(640.0));
        // 窥视态同属条态：更新宽度但不改 peek 子态
        st.peek = true;
        assert!(update_strip_width(&mut st, 300.0));
        assert!(st.peek);
        assert_eq!(st.form, TimelineForm::Strip);
    }

    #[test]
    fn board_moves_to_other_screen_keeping_offset_and_logical_size() {
        let a = scr([0, 0, 2560, 1440], [0, 0, 2560, 1392], 1.5);
        let b = scr([2560, 0, 4480, 1080], [2560, 0, 4480, 1040], 1.0);
        let board = Rect { x: 300, y: 150, width: 1350, height: 540 };
        let moved = move_board_to(board, &a, &b);
        assert_eq!(moved, Rect { x: 2560 + 200, y: 100, width: 900, height: 360 });
        // 放不下时钳进工作区
        let big = Rect { x: 2000, y: 1000, width: 2400, height: 1200 };
        let m = move_board_to(big, &a, &b);
        assert!(m.x >= 2560 && m.x + m.width as i32 <= 4480);
        assert!(m.y >= 0 && m.y + m.height as i32 <= 1040);
    }
}
