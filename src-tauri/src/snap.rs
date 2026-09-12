//! 挂件网格吸附——检测 Rust：拖动走
//! data-tauri-drag-region 的系统模态移动循环，前端无法在拖动中插手，吸附时机
//! 只能从窗口消息拿（设计文档选型 B；tauri:Monitor 无工作区，
//! 格网铺在 rcWork 内，Win32 取）。
//!
//! 机制：comctl32 子类化 widget 窗口，拦 WM_ENTERSIZEMOVE / WM_EXITSIZEMOVE /
//! WM_MOVING（子类化 proc 先于 tao wndproc 执行，末尾无条件 DefSubclassProc
//! 转发，tao/系统行为不受影响）：
//! - ENTER 快照几何；EXIT 无条件量化：**窗口右上角吸附到工作区格网最近顶点**
//!   （pitch 10 逻辑像素 × scale_factor；原点 = 工作区右上角向左/下延展；
//!   移动过程零迟滞——WM_MOVING 只读，动作仅 EXIT 后一次 set_position）；
//! - **贴边语义已删除（-3）**：边缘即顶点子集，不再重复定义；移动/拉伸/
//!   混合变化统一右上角网格对齐；比例锁程序化回写不产生 ENTER/EXIT
//!   （WM_MOVING 仅模态拖动发送），无自触发回路；
//! - `widget_snap_enabled`（serde default **false**，-4）P3 设置页接线前
//!   的开启杠杆：手改 window-state.json `"widget_snap_enabled": true`；
//! - 状态单侧写：子类化线程是唯一写者（AppState Mutex 单一源），不另设锁。
//! WM_* 不可达的回退（Moved 去抖）见设计文档 /

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

// ---------- SnapState（window-state.json 字段类型，跨平台编译；P2 顶点锚点消费） ----------

/// 格网顶点索引：相对工作区右上角原点的偏移格数（向左/向下为正；P1-R，-2）。
/// 绝对坐标不落盘——由 SnapState + 当前工作区矩形 + pitch 重算，显示器拔插/
/// DPI 变化时按当前工作区重对齐即自然适配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapVertex {
    pub col: u32,
    pub row: u32,
}

/// 吸附状态（`widget_snap` 字段，纯顶点模型——edge/corner 随 P1-R 删除）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapState {
    /// 停靠顶点（窗口右上角所在格网位置）。
    pub vertex: SnapVertex,
    /// 停靠时工作区矩形（物理像素；顶点绝对坐标重算依据）。
    pub work: [i32; 4],
    /// 停靠时格网间距（物理像素；顶点重算依据）。
    pub pitch: i32,
}

// ---------- 装配 ----------

/// 子类化 widget 窗口（setup 中调用一次；窗口缺失仅告警不阻断）。
pub fn install(app: &AppHandle) {
    let Some(window) = app.get_webview_window(crate::visibility::WIDGET_LABEL) else {
        crate::dev_log!("[snap] install skipped: widget window not found");
        return;
    };
    #[cfg(windows)]
    win::install(&window);
    #[cfg(not(windows))]
    let _ = window;
}

// ---------- Windows 实现 ----------

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};

    use super::{SnapState, SnapVertex};
    use tauri::{Manager, PhysicalPosition, WebviewWindow};

    /// 子类化槽位 id（同窗口多子类化按 id 区分，本应用仅此一处，取任意非零值）。
    const SUBCLASS_ID: usize = 0x7048_5350;

    // WM_* 本地常量（不为此引 Win32_UI_WindowsAndMessaging，见设计文档）：
    // 进入/退出系统移动尺寸模态循环（拖动与拉伸共用同一循环）、移动中的位置通知。
    const WM_ENTERSIZEMOVE: u32 = 0x0231;
    const WM_EXITSIZEMOVE: u32 = 0x0232;
    const WM_MOVING: u32 = 0x0216;

    /// 格网间距。
    const PITCH_LOGICAL: f64 = 10.0;

    /// 子类化回调上下文（install 时填充一次；回调与 tao wndproc 同为主线程）。
    struct Ctx {
        window: WebviewWindow,
        pitch: i32,
    }

    static CTX: OnceLock<Ctx> = OnceLock::new();
    /// 拖动轮次起点快照（ENTER 填 / EXIT 取；配对出现，Mutex 仅作防御）。
    static SNAP_START: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);
    /// MOVING 每次拖动轮次只记一次日志（逐像素触发，全量打印刷屏）。
    static LOGGED_MOVING: AtomicBool = AtomicBool::new(false);

    /// 工作区（物理像素，rcWork 已扣任务栏）。
    struct WorkArea {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    pub fn install(window: &WebviewWindow) {
        let Ok(hwnd) = window.hwnd() else {
            crate::dev_log!("[snap] install failed: no hwnd");
            return;
        };
        let scale = window.scale_factor().unwrap_or(1.0);
        let pitch = (PITCH_LOGICAL * scale).round() as i32;
        // tauri 的 HWND.0 数值类型跨版本漂移，as 转换对 isize/*mut c_void 均成立
        // （同 chrome.rs 惯例）
        let hwnd = hwnd.0 as HWND;
        let ok = unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0) };
        if ok == 0 {
            crate::dev_log!("[snap] SetWindowSubclass failed");
            return;
        }
        match CTX.set(Ctx { window: window.clone(), pitch }) {
            Ok(()) => crate::dev_log!("[snap] grid snap installed, pitch = {pitch}px"),
            Err(_) => crate::dev_log!("[snap] ctx already set (double install?)"),
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
        match msg {
            WM_ENTERSIZEMOVE => on_enter(),
            WM_MOVING => {
                if !LOGGED_MOVING.swap(true, Ordering::Relaxed) {
                    // S1 spike 结论通道：本行出现在 dev 控制台 = WM_MOVING 可达
                    crate::dev_log!("[snap] MOVING reachable (lparam={lparam:#x})");
                }
            }
            WM_EXITSIZEMOVE => on_exit(hwnd),
            _ => {}
        }
        // 无条件转发子类化链（最终到 tao 的 wndproc）——零迟滞约束（-1）：
        // 拖动过程零干预，量化动作只在 EXIT 后一次 set_position
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }

    fn on_enter() {
        LOGGED_MOVING.store(false, Ordering::Relaxed);
        let snapshot = window_geom().map(|(o, s)| (o.0, o.1, s.0, s.1));
        if let Ok(mut slot) = SNAP_START.lock() {
            *slot = snapshot;
        }
        crate::dev_log!("[snap] ENTER sizemove");
    }

    /// 拖动/拉伸结束：**无条件网格量化**（-3 贴边语义已删除，移动/拉伸同路径）。
    fn on_exit(hwnd: HWND) {
        crate::dev_log!("[snap] EXIT sizemove");
        let Some(ctx) = CTX.get() else { return };
        let app = ctx.window.app_handle();
        if !crate::window_state::widget_snap_enabled(app) {
            return;
        }
        let _ = SNAP_START.lock().ok().and_then(|mut slot| slot.take()); // 快照仅供日志对账
        let Some((outer, size)) = window_geom() else { return };
        let Some(work) = work_area(hwnd) else {
            crate::dev_log!("[snap] no work area");
            return;
        };
        // 右上角参考点：quantize（right_x, top_y) → 最近顶点 → 反推落点。
        // 出屏防护：顶点候选天然受格网铺在 rcWork 内约束，右上角不越工作区；
        // 窗口向左/下延伸可能越工作区左/上缘——右/下缘参考则钳回工作区内。
        let right_x = outer.0 + size.0;
        let vertex = nearest_vertex(&work, ctx.pitch, right_x, outer.1);
        let mut target = vertex_target(&work, ctx.pitch, vertex, size);
        target.0 = target.0.max(work.left);
        target.1 = target.1.min(work.bottom - size.1);
        if target != outer {
            crate::dev_log!(
                "[snap] top-right ({right_x},{}) → vertex ({},{}) at ({},{})",
                outer.1, vertex.col, vertex.row, target.0, target.1
            );
            let _ = ctx.window.set_position(PhysicalPosition::new(target.0, target.1));
            // P3 落定动效事件：仅位置真变时发（量化未位移不发,避免每次松手都闪）
            use tauri::Emitter;
            let _ = app.emit("widget-snap-landed", vertex);
        }
        crate::window_state::set_snap_state(
            app,
            crate::visibility::WIDGET_LABEL,
            Some(SnapState {
                vertex,
                work: [work.left, work.top, work.right, work.bottom],
                pitch: ctx.pitch,
            }),
        );
    }

    /// 最近顶点：顶点绝对坐标 =（right − k·pitch, top + m·pitch），k/m ≥ 0；
    /// 对参考点坐标按 pitch 取整即得最近顶点索引。
    fn nearest_vertex(work: &WorkArea, pitch: i32, right_x: i32, top_y: i32) -> SnapVertex {
        let dx = (work.right - right_x).max(0);
        let dy = (top_y - work.top).max(0);
        SnapVertex {
            col: ((dx as f64 / pitch as f64).round() as u32),
            row: ((dy as f64 / pitch as f64).round() as u32),
        }
    }

    /// 顶点反推窗口落点（右上角对齐顶点；窗口向左/下延伸）。
    fn vertex_target(work: &WorkArea, pitch: i32, v: SnapVertex, size: (i32, i32)) -> (i32, i32) {
        let vx = work.right - (v.col as i32) * pitch;
        let vy = work.top + (v.row as i32) * pitch;
        (vx - size.0, vy)
    }

    /// 当前窗口矩形（物理像素，outer 口径；与判定/落点全程同口径）。
    fn window_geom() -> Option<((i32, i32), (i32, i32))> {
        let ctx = CTX.get()?;
        let pos = ctx.window.outer_position().ok()?;
        let size = ctx.window.outer_size().ok()?;
        Some(((pos.x, pos.y), (size.width as i32, size.height as i32)))
    }

    /// 窗口所在显示器的工作区（MONITOR_DEFAULTTONEAREST + rcWork，扣任务栏）。
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
