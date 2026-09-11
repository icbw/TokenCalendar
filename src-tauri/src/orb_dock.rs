//! 悬浮球贴边停靠——「拖到屏幕边缘松手
//! 自动贴边收起成竖条,从边缘拖出/双击展开回卡片」的 edge docking 范式。
//!
//! 机制：
//! - 拖动走 data-tauri-drag-region 的系统模态移动循环,「松手」时刻只有
//!   WM_EXITSIZEMOVE 可靠（前端无 moveEnd 事件,onMoved 停顿≠松手）;
//!   comctl32 子类化 orb 窗口,与 snap.rs（widget）同模式,互不干扰;
//! - 判定：EXIT 时窗口外沿贴近工作区（rcWork,物理像素,扣任务栏）左/右缘
//!   ≤ 阈值 → dock 该缘;顶部/底部不做（任务栏区/语义弱,v1 范围外）;
//! - dock 动作：set_size（32×96) + set_position（缘内,锚 Y 保持拖动结束时的
//!   竖条中心 Y),发 `orb-dock-changed` 事件——**形态切换由前端执行**
//!   （两态尺寸单一执行者原则,S4 同款;Rust 只做几何归位+状态+广播,
//!   前端收 React 态到收起并按 dock 位置校准）;
//! - undock：已 dock 态下 EXIT 时窗口离开缘阈值 → 清状态+事件,前端回展开;
//! - 双击展开（无拖动）不走 EXIT 路径——前端 expand 自行按 dock 侧
//!   向屏幕内生长（undock 语义）,经 `orb_undock` 命令清状态。
//!
//! 与格网吸附的关系：「悬浮球不参与格网吸附」不变——贴边停靠是
//! dock 语义,非顶点量化;widget 的 snap.rs 不感知本模块。

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 停靠态（window-state.json `orb_dock` 字段;跨平台编译）。
/// 只存边与锚点相对量,绝对坐标由当前工作区重算（显示器拔插/DPI 变更自适应,
/// 同 snap.rs SnapState 哲学）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OrbDockState {
    /// 停靠缘："left" | "right"（serde rename;顶/底 v1 不做）。
    pub edge: OrbDockEdge,
    /// 停靠时竖条中心 Y 相对工作区顶部的比例（0.0〜1.0;工作区高度变化时按
    /// 比例重放,避免绝对像素越界）。
    pub anchor_y_ratio: f64,
    /// 停靠时工作区矩形（物理像素;重算与回滚依据）。
    pub work: [i32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrbDockEdge {
    Left,
    Right,
}

/// 装配（setup 调用一次）：子类化 orb 窗口;窗口缺失仅告警不阻断。
pub fn install(app: &AppHandle) {
    let Some(window) = app.get_webview_window(crate::visibility::ORB_LABEL) else {
        crate::dev_log!("[orb-dock] install skipped: orb window not found");
        return;
    };
    #[cfg(windows)]
    win::install(&window);
    #[cfg(not(windows))]
    let _ = window;
}

// ---------- 状态访问（AppState 单一源;子类化线程与命令共用） ----------

pub fn dock_state_for(app: &AppHandle) -> Option<OrbDockState> {
    app.state::<crate::AppState>()
        .orb_dock
        .lock()
        .unwrap()
        .clone()
}

/// 更新停靠状态并即时落盘（低频离散事件,不走几何节流;失败静默——下次
/// 显隐/退出兜底重写）。**只存状态,不发事件**——事件由 dock/undock 动作方发。
pub fn set_dock_state(app: &AppHandle, dock: Option<OrbDockState>) {
    let state = app.state::<crate::AppState>();
    *state.orb_dock.lock().unwrap() = dock;
    crate::window_state::persist(app, &state);
}

/// 前端发起的 undock 入口（双击展开/拖离边缘展开）：清停靠状态,并按 edge
/// 把窗口位置做 expand-ready 归位——dock 态竖条贴死工作区缘,直接
/// set_size（240) 会把卡片推出屏外;按 dock 侧把窗口右/左缘先收进屏内
/// （留 8 逻辑像素呼吸位）。edge=None 时仅清状态。无事件——发起方（前端）
/// 已知新形态,状态清理保证落盘一致。
#[tauri::command]
pub fn orb_undock(app: AppHandle, edge: Option<String>) -> Result<(), String> {
    let window = app
        .get_webview_window(crate::visibility::ORB_LABEL)
        .ok_or("orb window not found")?;
    let dock = dock_state_for(&app).or_else(|| {
        // 调用方未带状态（如 undock 事件竞态已清）但显式传了 edge——
        // 以入参构造临时锚点完成归位。
        edge.as_deref().map(|e| OrbDockState {
            edge: match e {
                "left" => OrbDockEdge::Left,
                _ => OrbDockEdge::Right,
            },
            anchor_y_ratio: 0.5,
            work: [0, 0, 0, 0],
        })
    });
    set_dock_state(&app, None);
    if let Some(dock) = dock {
        #[cfg(windows)]
        win::expand_ready(&window, &dock);
        #[cfg(not(windows))]
        let _ = (&window, dock);
    }
    Ok(())
}

/// 前端启动恢复查询（OrbWindow 挂载时):有 dock 态 → 回收起态+贴边位置。
#[tauri::command]
pub fn get_orb_dock(app: AppHandle) -> Result<Option<OrbDockState>, String> {
    Ok(dock_state_for(&app))
}

/// dock/undock 广播载荷（前端消费:收起/展开形态切换+几何校准）。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct OrbDockChanged {
    pub docked: bool,
    pub edge: Option<OrbDockEdge>,
}

// ---------- Windows 实现 ----------

#[cfg(windows)]
mod win {
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};

    use super::{OrbDockChanged, OrbDockEdge, OrbDockState};
    use tauri::{Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

    const SUBCLASS_ID: usize = 0x704F_5242;

    // 同 snap.rs：本地常量,不为此引 Win32_UI_WindowsAndMessaging
    const WM_EXITSIZEMOVE: u32 = 0x0232;

    /// 贴边判定阈值（物理像素,按 scale_factor 换算的逻辑 24px）——拖动结束
    /// 时窗口缘距工作区缘 ≤ 此值即 dock。竖条宽 32 逻辑像素的量级,手感近似
    /// Aero Snap 的边缘敏感度（用户预期「贴到边上就算」）。
    const EDGE_THRESHOLD_LOGICAL: f64 = 24.0;

    /// 竖条尺寸（逻辑像素,与前端 COLLAPSED_SIZE 同源——Rust 侧几何归位用）。
    const PILL_W_LOGICAL: f64 = 32.0;
    const PILL_H_LOGICAL: f64 = 96.0;
    /// 贴边水平呼吸位（逻辑像素;竖条完全贴死缘会被显示器圆角/阴影吃掉）。
    const EDGE_GAP_LOGICAL: f64 = 0.0;

    struct Ctx {
        window: WebviewWindow,
        threshold: i32,
    }

    static CTX: OnceLock<Ctx> = OnceLock::new();

    /// 工作区（物理像素,rcWork 已扣任务栏）。
    struct WorkArea {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    pub fn install(window: &WebviewWindow) {
        let Ok(hwnd) = window.hwnd() else {
            crate::dev_log!("[orb-dock] install failed: no hwnd");
            return;
        };
        let scale = window.scale_factor().unwrap_or(1.0);
        let threshold = (EDGE_THRESHOLD_LOGICAL * scale).round() as i32;
        let hwnd = hwnd.0 as HWND;
        let ok = unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0) };
        if ok == 0 {
            crate::dev_log!("[orb-dock] SetWindowSubclass failed");
            return;
        }
        match CTX.set(Ctx { window: window.clone(), threshold }) {
            Ok(()) => crate::dev_log!("[orb-dock] installed, threshold = {threshold}px"),
            Err(_) => crate::dev_log!("[orb-dock] ctx already set (double install?)"),
        }
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _ref_data: usize,
    ) -> LRESULT {
        if msg == WM_EXITSIZEMOVE {
            on_exit(hwnd);
        }
        // 无条件转发子类化链（tao wndproc 行为不受影响;同 snap.rs 零迟滞约束）
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }

    /// 松手：dock 判定与执行（已 dock → 离缘 undock;自由 → 贴缘 dock）。
    fn on_exit(hwnd: HWND) {
        let Some(ctx) = CTX.get() else { return };
        let app = ctx.window.app_handle();
        let Some((pos, size)) = window_geom(ctx) else { return };
        let Some(work) = work_area(hwnd) else { return };

        let current = super::dock_state_for(app);
        // 距左右缘的有符号距离（正=窗口缘在工作区内侧,负=越出工作区缘——
        // 根因:拖动光标钳在屏内而抓取点在卡片中部,卡片右缘自然越出缘
        // 100px+,abs 把这种最常见手势判成「远离边缘」。语义:越出=贴缘意图,
        // 同样算数,且等效「磁吸回弹」——松手近缘/越缘都被吸到标准停靠位。
        // 参考 Rainmeter SnapEdges/Aero Snap 的宽容语义。）
        let d_left = pos.0 - work.left;
        let d_right = work.right - (pos.0 + size.0 as i32);

        match current {
            Some(dock) => {
                // 已停靠态:拖出缘阈值 → undock（清状态+广播,前端回展开卡片;
                // 展开位置钳回屏内由前端消费事件时处理——拖出距离不足以放下
                // 展开卡片时,展开会把卡片推回缘内）
                let near_dock_edge = match dock.edge {
                    OrbDockEdge::Left => d_left <= ctx.threshold,
                    OrbDockEdge::Right => d_right <= ctx.threshold,
                };
                if !near_dock_edge {
                    crate::dev_log!("[orb-dock] undock (dragged away from {:?})", dock.edge);
                    super::set_dock_state(app, None);
                    use tauri::Emitter;
                    let _ = app.emit(
                        "orb-dock-changed",
                        OrbDockChanged { docked: false, edge: None },
                    );
                }
            }
            None => {
                // 自由态:贴缘/越缘 → dock（几何立即归位为竖条贴边,状态+广播;
                // 形态由前端消费事件切换——两态尺寸单一执行者原则）。
                // 两侧同时达阈（屏太窄）取更近一侧。
                let edge = match (d_left <= ctx.threshold, d_right <= ctx.threshold) {
                    (true, true) => {
                        if d_left <= d_right { Some(OrbDockEdge::Left) } else { Some(OrbDockEdge::Right) }
                    }
                    (true, false) => Some(OrbDockEdge::Left),
                    (false, true) => Some(OrbDockEdge::Right),
                    (false, false) => None,
                };
                if let Some(edge) = edge {
                    // 锚 Y:拖动结束时窗口中心 Y → 工作区相对比例（钳 0〜1）
                    let center_y = pos.1 as f64 + size.1 as f64 * 0.5;
                    let ratio = ((center_y - work.top as f64)
                        / (work.bottom - work.top).max(1) as f64)
                        .clamp(0.0, 1.0);
                    let state = OrbDockState {
                        edge,
                        anchor_y_ratio: ratio,
                        work: [work.left, work.top, work.right, work.bottom],
                    };
                    crate::dev_log!("[orb-dock] dock {:?} anchor_ratio={ratio:.2}", edge);
                    super::set_dock_state(app, Some(state));
                    place_docked(ctx, &state);
                    use tauri::Emitter;
                    let _ = app.emit(
                        "orb-dock-changed",
                        OrbDockChanged { docked: true, edge: Some(edge) },
                    );
                }
            }
        }
    }

    /// dock 几何归位：32×96 竖条贴缘,锚 Y 按比例回放（物理像素;竖条中心对齐
    /// anchor_y_ratio·工作区高,钳在工作区内）。persist 不在此触发（Moved/
    /// Resized 事件通道 2s 节流自会跟上;状态落盘已由 set_dock_state 即时写）。
    fn place_docked(ctx: &Ctx, dock: &OrbDockState) {
        let Some(work) = work_area_hwnd(ctx) else { return };
        let scale = ctx.window.scale_factor().unwrap_or(1.0);
        let w = (PILL_W_LOGICAL * scale).round() as i32;
        let h = (PILL_H_LOGICAL * scale).round() as i32;
        let gap = (EDGE_GAP_LOGICAL * scale).round() as i32;
        let x = match dock.edge {
            OrbDockEdge::Left => work.left + gap,
            OrbDockEdge::Right => work.right - w - gap,
        };
        // 锚 Y:竖条中心 = top + ratio·工作区高 → 竖条 top = 中心 − h/2,钳界
        let center_y = work.top as f64 + dock.anchor_y_ratio * (work.bottom - work.top) as f64;
        let y = (center_y as i32 - h / 2).clamp(work.top, work.bottom - h);
        let _ = ctx.window.set_size(PhysicalSize::new(w as u32, h as u32));
        let _ = ctx.window.set_position(PhysicalPosition::new(x, y));
    }

    fn work_area_hwnd(ctx: &Ctx) -> Option<WorkArea> {
        let hwnd = ctx.window.hwnd().ok()?.0 as HWND;
        work_area(hwnd)
    }

    /// 双击展开/拖离展开的 expand-ready 归位（orb_undock 调用）：按 dock 侧
    /// 把窗口右/左缘收进工作区内 8 逻辑像素,让后续 set_size（240) 的展开卡片
    /// 完整落在屏内。高度方向不动（锚 Y 语义保持）。work 字段为 0（入参构造
    /// 的临时锚点）时跳过钳制——窗口已在屏内的常规 undock 无需挪位。
    pub fn expand_ready(window: &WebviewWindow, dock: &OrbDockState) {
        if dock.work == [0, 0, 0, 0] {
            return;
        }
        let Ok(hwnd) = window.hwnd() else { return };
        let Some(work) = work_area(hwnd.0 as HWND) else { return };
        let scale = window.scale_factor().unwrap_or(1.0);
        let Ok(pos) = window.outer_position() else { return };
        let Ok(size) = window.outer_size() else { return };
        let gap = (8.0_f64 * scale).round() as i32;
        let target_x = match dock.edge {
            OrbDockEdge::Left => work.left + gap,
            OrbDockEdge::Right => work.right - size.width as i32 - gap,
        };
        if target_x != pos.x {
            let _ = window.set_position(PhysicalPosition::new(target_x, pos.y));
        }
    }

    fn window_geom(ctx: &Ctx) -> Option<((i32, i32), (i32, i32))> {
        let pos = ctx.window.outer_position().ok()?;
        let size = ctx.window.outer_size().ok()?;
        Some(((pos.x, pos.y), (size.width as i32, size.height as i32)))
    }

    fn work_area(hwnd: HWND) -> Option<WorkArea> {
        let empty = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: empty,
            rcWork: empty,
            dwFlags: 0,
        };
        unsafe {
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            if monitor.is_null() || GetMonitorInfoW(monitor, &mut info) == 0 {
                return None;
            }
        }
        let rc = info.rcWork;
        Some(WorkArea { left: rc.left, top: rc.top, right: rc.right, bottom: rc.bottom })
    }
}
