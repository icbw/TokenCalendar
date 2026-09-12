//! 悬浮球贴边停靠——「拖到屏幕边缘松手自动贴边收起成竖条,从边缘
//! 拖出/双击展开回卡片」的 edge docking 范式。多显示器（含混合 DPI）下形态与
//! 位置必须稳定,几何模型因此立在三条硬约束上：
//!
//! 1. **参照物是显示器,不是窗口**。所有判定与归位都用「当前显示器枚举里那台
//!    目标屏的 rcWork + 该屏自己的 scale」。窗口自身的 scale_factor（tao 缓存）
//!    与 MonitorFromWindow（按窗口矩形中心归属）在多屏接缝上会互相打架：窗口
//!    中心一过缝归属就翻到隔壁,于是「光标在 A 屏、贴边意图是 A 屏右缘」被判成
//!    「触到 B 屏左缘」——竖条跳到接缝另一侧、卡片在错误的一屏展开。
//!
//! 2. **参照点是可见内容,不是窗口矩形,也不是光标**。窗口含透明边距/提示位画布
//!    （展开态 580 逻辑宽里内容只有 175）,窗口矩形越界 ≠ 用户看到的内容越界;
//!    光标又受抓取点偏移影响。两态容器都绕内容中心对称（窗口中心 = 内容中心 =
//!    视觉中心）,所以「内容矩形」既是最小歧义的判定对象,也与系统按窗口中心的
//!    归属判定天然一致。
//!
//! 3. **单一落点判据**。松手时只解一个问题：内容矩形离
//!    **内容所在屏**的哪条竖缘最近、间距是否 ≤ 容差——不分「自由态吸附」与
//!    「已停靠拖离」两套判据。旧版已停靠态盯「停靠状态记录的那台屏、那条缘」,
//!    于是竖条从别处拖到另一台屏的边缘时会因「离原停靠缘很远」被判拖离（弹出
//!    表盘）,跨接缝后还会按旧屏归位（贴边距离漂移、甚至侵入接缝）。统一后：
//!    贴到所在屏的某条缘 ⇒ 竖条（保持/进入,含换屏换边）;未贴且已停靠 ⇒ 展开、
//!    未贴且自由 ⇒ 钳回。判定输入只有「内容矩形 + 所在屏」,与从哪拖来无关。
//!
//! 4. **一次归位一个执行者**。形态切换（尺寸）与位置补偿必须在同一次调用里原子
//!    完成：前端 set_orb_size（内容锚定）与 Rust 归位各自补偿一次位置的话,两个
//!    invoke 并发时补偿被算两遍,卡片会横向窜两百像素。
//!
//! DPI 切换的时序：把窗口移到另一台 DPI 不同的显示器时,Windows 发 WM_DPICHANGED,
//! tao 按「保持逻辑尺寸」重排窗口（尺寸与位置都会被系统改）。归位统一走
//! `set_position → set_size → set_position` 三明治,再做一次读回校验,把系统重排
//! 的影响收敛掉。
//!
//! 其余机制（拖动通道 / 状态广播 / 形态执行者）保持原样：
//! - 拖动走 data-tauri-drag-region 的系统模态移动循环,「松手」时刻只有
//!   WM_EXITSIZEMOVE 可靠,comctl32 子类化 orb 窗口（与 snap.rs 同模式,互不干扰）;
//! - dock/undock 动作（几何归位 + 状态 + 广播）在 Rust;前端消费事件切 React 形态。
//!   收起态尺寸由 place_docked 设定,展开态尺寸由 orb_undock 设定——前端只在
//!   「手动折叠/自由态展开」路径上调 set_orb_size。

use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Manager};

/// 悬浮球当前形态（true = 展开卡片）。Rust 侧权威副本：几何判定与点击穿透命中
/// 都不再从窗口尺寸反推形态——跨屏拖动时系统会按 DPI 重排窗口物理尺寸,
/// 「物理尺寸 ÷ 当前屏 scale」推不出可靠的逻辑尺寸。写者 = set_orb_size 命令与
/// orb_dock 归位路径。
pub fn expanded_state(app: &AppHandle) -> bool {
    app.state::<crate::AppState>().orb_expanded.load(Ordering::SeqCst)
}

pub fn set_expanded_state(app: &AppHandle, expanded: bool) {
    app.state::<crate::AppState>().orb_expanded.store(expanded, Ordering::SeqCst);
}

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
    /// 停靠时工作区矩形（物理像素;重算与匹配显示器用）。
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

/// 前端发起的 undock 入口（双击展开/拖离边缘展开）：清停靠状态,并做**原子的**
/// 展开归位——设展开态尺寸 + 内容原地长大 + 钳进内容所在显示器工作区。
/// 尺寸与位置在同一调用里完成（前端不再另调 set_orb_size,避免两次位置补偿）。
///
/// `expand_w` / `expand_h`（逻辑像素,可选）：本次展开后的目标窗口尺寸;
/// 缺省回退到 Rust 侧的展开态常量。
///
/// `edge`：停靠侧,仅用于日志（归位不看方向——按内容当前所在显示器原地生长）。
///
/// 无事件——发起方（前端）已知新形态,状态清理保证落盘一致。
#[tauri::command]
pub fn orb_undock(
    app: AppHandle,
    edge: Option<String>,
    expand_w: Option<f64>,
    expand_h: Option<f64>,
) -> Result<(), String> {
    let window = app
        .get_webview_window(crate::visibility::ORB_LABEL)
        .ok_or("orb window not found")?;
    crate::dev_log!("[orb-dock] orb_undock edge={edge:?}");
    set_dock_state(&app, None);
    #[cfg(windows)]
    win::undock_ready(&window, expand_w, expand_h);
    #[cfg(not(windows))]
    let _ = (&window, expand_w, expand_h);
    Ok(())
}

/// 前端启动恢复查询（OrbWindow 挂载时):有 dock 态 → 回收起态+贴边位置。
#[tauri::command]
pub fn get_orb_dock(app: AppHandle) -> Result<Option<OrbDockState>, String> {
    Ok(dock_state_for(&app))
}

/// dock/undock 广播载荷（前端消费:收起/展开形态切换）。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct OrbDockChanged {
    pub docked: bool,
    pub edge: Option<OrbDockEdge>,
}

/// restore 后的边界钳制入口（window_state:restore 调用;仅 orb）：
/// 取内容所在显示器工作区把恢复位置钳回屏内。dock 态重启由 place_docked 精确
/// 归位,此处只管自由态的「不裁切」底线。
#[cfg(windows)]
pub fn clamp_restored(window: &tauri::WebviewWindow) {
    win::clamp_restored(window);
}

/// 前端驱动尺寸切换的**内容锚定**入口（手动折叠 / 自由态展开）：
/// 换尺寸的同时把窗口位置补回「内容原点不动」,并同步 Rust 侧形态状态。
/// 跨屏时用内容所在显示器的 scale 换算（不用 tao 缓存值,避免与目标屏不一致）。
#[cfg(windows)]
pub fn set_size_anchored(window: &tauri::WebviewWindow, width: f64, height: f64) {
    win::set_size_anchored(window, width, height);
}

/// 恢复期位置换算（orb 专用;window_state:restore 调用）：落盘几何记的是**窗口**
/// 左上,而两态的内容偏移不同——按记录尺寸判断当时属于哪一态,把位置换成当前
/// （收起态）的窗口位置。否则「展开态退出 → 重启」时,竖条会出现在那张大画布的
/// 角落里（离卡片原来的视觉位置几百像素）。
#[cfg(windows)]
pub fn restore_pos_offset(
    window: &tauri::WebviewWindow,
    recorded_w: f64,
    recorded_h: f64,
) -> (i32, i32) {
    win::content_anchor_offset(window, recorded_w, recorded_h)
}

/// restore 时的 dock 态归位入口（window_state:restore 调用;仅 orb）：
/// dock 态按 edge + anchor_y_ratio 重算贴边位置（含透明边距出屏补偿——
/// 不能用 clamp_restored,它会把窗口拉回屏内,贴边观感与运行时停靠不一致）。
#[cfg(windows)]
pub fn restore_docked(window: &tauri::WebviewWindow, dock: &OrbDockState) {
    win::restore_docked(window, dock);
}

// ---------- Windows 实现 ----------

#[cfg(windows)]
mod win {
    use std::sync::Mutex;
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};

    use super::{OrbDockChanged, OrbDockEdge, OrbDockState};
    use tauri::{LogicalSize, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

    const SUBCLASS_ID: usize = 0x704F_5242;

    // 同 snap.rs：本地常量,不为此引 Win32_UI_WindowsAndMessaging
    const WM_EXITSIZEMOVE: u32 = 0x0232;
    /// WM_ENTERSIZEMOVE（移动循环开始）——与 EXITSIZEMOVE 配对,作为「这次按下
    /// 到底有没有把窗口拖走」的起点（判定见 DRAG_SHIFT_LOGICAL）。
    const WM_ENTERSIZEMOVE: u32 = 0x0231;
    /// WM_NCHITTEST（lParam = 光标屏幕坐标,低/高 16 位各为 x/y 的有符号短整型）。
    const WM_NCHITTEST: u32 = 0x0084;
    /// WM_NCHITTEST 返回码：命中透明边距 → 忽略本窗口,点击交给下层
    /// （点击穿透——「容器不可交互」;HTTRANSPARENT = -1）。
    const HTTRANSPARENT: LRESULT = -1;

    /// **拖动位移判定阈值**（逻辑像素）——移动循环结束时窗口位移 ≥ 此值才算
    /// 「真拖动」,广播 `orb-dragged` 供前端抑制 hover 提示。手抖级位移/原地按下
    /// 放开不算（拖动区覆盖整块表盘,mousedown 即进移动循环,「按住没动」必须与
    /// 「拖走了一段」区分开——这是把判定放 Rust 的唯一理由）。
    const DRAG_SHIFT_LOGICAL: f64 = 2.0;

    /// **贴边判据的统一容差**（逻辑像素）：内容矩形与**内容所在屏**某条竖缘的
    /// 贴合间距 ≤ 此值 ⇒ 判「贴在这条缘上」。全场只有这一个容差、一套判据——
    /// 自由态吸附、已停靠保持/拖离、跨屏换边全走它（全局一致性）。
    /// 判据对象是**内容**而非光标：光标受抓取点偏移影响（抓卡片中部拖动时窗口
    /// 先行越界）,内容矩形才是「用户看到的主体贴到边了没有」。
    /// 取 16：约为表盘直径的 1/9,「看起来贴上去了」即可触发,又不会把停在屏边
    /// 附近的卡片无故吸走;竖条贴边时内容离缘 4px,故拖离约 12px 才脱离
    /// （手抖/微调,含沿边缘上下挪,都还算贴着）。
    const DOCK_TOLERANCE_LOGICAL: f64 = 16.0;

    /// 收起态窗口尺寸（逻辑像素,与前端 COLLAPSED_SIZE 同源——Rust 侧几何归位用）。
    /// 窗口 = 竖条本体 24×84 + **每边 16px 透明呼吸位**：竖条投影（10px 模糊）与
    /// 描边发光需要窗口边界之外的空间,否则被硬切出直边。
    const PILL_W_LOGICAL: f64 = 56.0;
    const PILL_H_LOGICAL: f64 = 116.0;

    /// 展开态窗口尺寸（逻辑像素,与前端 EXPANDED_SIZE 同源——缺省归位尺寸）。
    /// 窗口 = 可见内容 + 透明画布（hover 提示的落点空间）,且**绕表盘视觉中心
    /// 对称**：Windows 按窗口矩形中心判定窗口属于哪台显示器,对称后视觉中心过缝
    /// 的那一刻才等于窗口中心过缝,系统判定与人的感知一致。
    /// ⚠ 与前端 EXPANDED_SIZE 同源,改一处要改两处。
    const EXPANDED_W_LOGICAL: f64 = 580.0;
    const EXPANDED_H_LOGICAL: f64 = 310.0;

    /// 展开态**内容**（可见卡片：表盘 + 按钮列）在窗口内的左上偏移（逻辑像素）：
    /// 215 + 75 = 580/2、80 + 75 = 310/2（表盘 150×150,视觉中心在内容块左上
    /// （75,75) 处）——容器因此绕视觉中心对称。与前端 CSS 同源。
    const EXPANDED_PAD_L_LOGICAL: f64 = 215.0;
    const EXPANDED_PAD_T_LOGICAL: f64 = 80.0;

    /// 表盘半径（逻辑像素;150px 正圆的一半）。只用于校验「窗口中心 = 表盘中心」
    /// 这条对称硬约束（见单测）——画布若只往右/下扩,窗口中心会偏离视觉中心,
    /// 系统就会在视觉主体还没过缝时先把窗口判给隔壁屏。
    #[allow(dead_code)]
    const DIAL_RADIUS_LOGICAL: f64 = 75.0;

    /// 竖条本体在窗口内的边距（逻辑像素;窗口 = 本体 + 2×此值,居中）。
    const PILL_INSET_LOGICAL: f64 = 16.0;

    /// 可见内容的边界余量（逻辑像素）——投影/描边发光所需的最小空位。钳制按
    /// 「内容矩形 + 此余量」算：卡片可以贴屏边,但阴影不能被屏边裁掉。
    const CONTENT_MARGIN_LOGICAL: f64 = 16.0;

    /// 贴边时竖条**本体**距工作区缘的间距（逻辑像素）——归位时窗口的透明边距
    /// 允许落在屏外（屏外无像素,不影响视觉）。
    const PILL_EDGE_GAP_LOGICAL: f64 = 4.0;

    /// 展开归位时内容距工作区缘的最小呼吸位（逻辑像素）。
    const EXPAND_GAP_LOGICAL: f64 = 8.0;

    /// 交互主体尺寸（逻辑像素,点击穿透的命中区）——两态各一组,与前端内容区
    /// 尺寸同源：
    /// - 收起态 = 竖条本体 24×84;
    /// - 展开态 = 表盘 150 + 间隙 6 + 按钮列 19 = 175 宽,高 150。
    const PILL_INNER_W_LOGICAL: f64 = 24.0;
    const PILL_INNER_H_LOGICAL: f64 = 84.0;
    const EXPANDED_INNER_W_LOGICAL: f64 = 175.0;
    const EXPANDED_INNER_H_LOGICAL: f64 = 150.0;

    /// 归位读回校验容差（物理像素）——超过才追加一次纠正调用。
    const SETTLE_TOL_PHYS: i32 = 4;

    /// 工作区（物理像素,rcWork 已扣任务栏）。Copy：多条判定路径各取一份。
    #[derive(Clone, Copy)]
    struct WorkArea {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    /// 显示器（物理像素）+ 该屏 DPI 缩放。判定与归位的**唯一参照**：
    /// work/rect 来自 Win32（rcWork / rcMonitor）,scale 来自 tauri 显示器枚举
    /// （tao 用 GetDpiForMonitor 取,准确且不依赖窗口当前 DPI 缓存）。
    #[derive(Clone, Copy)]
    struct Screen {
        /// rcMonitor（显示器完整矩形;点的归属判定用）。
        rect: WorkArea,
        /// rcWork（扣任务栏;贴边判定与归位用）。
        work: WorkArea,
        /// 该显示器 DPI 缩放（逻辑 ↔ 物理换算）。
        scale: f64,
    }

    struct Ctx {
        window: WebviewWindow,
        /// 拖动位移判定阈值（物理像素;见 DRAG_SHIFT_LOGICAL）。
        drag_threshold: i32,
        /// 移动循环的起点窗口位置（物理像素;WM_ENTERSIZEMOVE 记,EXIT 取用
        /// 后即清）——只用于判定「窗口真被拖走」,不参与 dock 几何。
        drag_origin: Mutex<Option<(i32, i32)>>,
    }

    static CTX: OnceLock<Ctx> = OnceLock::new();

    pub fn install(window: &WebviewWindow) {
        let Ok(hwnd) = window.hwnd() else {
            crate::dev_log!("[orb-dock] install failed: no hwnd");
            return;
        };
        let scale = window.scale_factor().unwrap_or(1.0);
        let drag_threshold = (DRAG_SHIFT_LOGICAL * scale).round() as i32;
        let hwnd = hwnd.0 as HWND;
        let ok = unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0) };
        if ok == 0 {
            crate::dev_log!("[orb-dock] SetWindowSubclass failed");
            return;
        }
        let ctx = Ctx {
            window: window.clone(),
            drag_threshold,
            drag_origin: Mutex::new(None),
        };
        match CTX.set(ctx) {
            Ok(()) => crate::dev_log!("[orb-dock] installed, drag_threshold = {drag_threshold}px"),
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
        if msg == WM_ENTERSIZEMOVE {
            remember_drag_origin();
        } else if msg == WM_EXITSIZEMOVE {
            on_exit();
        } else if msg == WM_NCHITTEST {
            // 点击穿透：命中透明呼吸位 → HTTRANSPARENT（不进 tao 的默认 hit test;
            // 主体区/拖动中的判定交回默认处理）
            if let Some(hit) = transparent_margin_hit(lparam) {
                return hit;
            }
        }
        // 无条件转发子类化链（tao wndproc 行为不受影响;同 snap.rs 零迟滞约束）
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }

    // ---------- 显示器模型 ----------

    /// 当前所有显示器（含各自工作区与 scale）。
    /// 低频调用（拖动松手 / 归位命令）,一次枚举后在内存里做全部判定。
    fn screens(window: &WebviewWindow) -> Vec<Screen> {
        let Ok(monitors) = window.available_monitors() else {
            crate::dev_log!("[orb-dock] available_monitors failed");
            return Vec::new();
        };
        let mut out = Vec::with_capacity(monitors.len());
        for m in &monitors {
            let p = *m.position();
            let s = *m.size();
            let (w, h) = (s.width as i32, s.height as i32);
            let rect = WorkArea {
                left: p.x,
                top: p.y,
                right: p.x + w,
                bottom: p.y + h,
            };
            // rcWork 走 Win32（tauri 的 Monitor 只有显示器矩形,没有工作区）
            let work = monitor_work_at(p.x + w / 2, p.y + h / 2).unwrap_or(rect);
            out.push(Screen { rect, work, scale: m.scale_factor() });
        }
        out
    }

    /// 点（物理像素）落在哪台显示器：矩形命中优先,全不命中取中心最近者
    /// （拖到屏外/两屏缝隙时的兜底）。
    fn screen_index_at(screens: &[Screen], x: i32, y: i32) -> usize {
        for (i, s) in screens.iter().enumerate() {
            if x >= s.rect.left && x < s.rect.right && y >= s.rect.top && y < s.rect.bottom {
                return i;
            }
        }
        let mut best = 0usize;
        let mut best_d = i64::MAX;
        for (i, s) in screens.iter().enumerate() {
            let cx = (s.rect.left as i64 + s.rect.right as i64) / 2;
            let cy = (s.rect.top as i64 + s.rect.bottom as i64) / 2;
            let d = (cx - x as i64).pow(2) + (cy - y as i64).pow(2);
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best
    }

    /// 内容所在显示器：两态容器都绕**视觉中心**对称——收起态 = 竖条本体中心,
    /// 展开态 = 表盘中心（215 + 75 = 580/2、80 + 75 = 310/2）——所以「窗口矩形
    /// 中心」恒等于视觉中心。用窗口中心定归属有两重好处：它是系统判窗口属于哪台
    /// 显示器（DPI 切换、跨屏语义）的同一把尺,而且视觉主体的位置才是用户感知的
    /// 「卡片在哪台屏上」。⚠ 展开态内容矩形（含右侧按钮列）的中心不在此列。
    fn content_screen_index(screens: &[Screen], pos: (i32, i32), size: (i32, i32)) -> usize {
        screen_index_at(screens, pos.0 + size.0 / 2, pos.1 + size.1 / 2)
    }

    /// 停靠状态里记录的工作区是否就是这台显示器（显示器拔插/重排后失配）。
    fn same_work(work: &WorkArea, recorded: &[i32; 4]) -> bool {
        work.left == recorded[0]
            && work.top == recorded[1]
            && work.right == recorded[2]
            && work.bottom == recorded[3]
    }

    /// 某点（物理像素）所在显示器的工作区。
    fn monitor_work_at(x: i32, y: i32) -> Option<WorkArea> {
        monitor_work(unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) })
    }

    fn monitor_work(monitor: HMONITOR) -> Option<WorkArea> {
        let empty = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: empty,
            rcWork: empty,
            dwFlags: 0,
        };
        unsafe {
            if monitor.is_null() || GetMonitorInfoW(monitor, &mut info) == 0 {
                return None;
            }
        }
        let rc = info.rcWork;
        Some(WorkArea { left: rc.left, top: rc.top, right: rc.right, bottom: rc.bottom })
    }

    // ---------- 内容几何 ----------

    /// 形态 → 内容在窗口内的左上偏移（逻辑像素）。
    fn content_pad(expanded: bool) -> (f64, f64) {
        if expanded {
            (EXPANDED_PAD_L_LOGICAL, EXPANDED_PAD_T_LOGICAL)
        } else {
            (PILL_INSET_LOGICAL, PILL_INSET_LOGICAL)
        }
    }

    /// 是否像展开态（按逻辑尺寸与两态常量的距离判定;仅用于外部传入尺寸的定性,
    /// 运行时形态以 Rust 权威状态为准）。
    fn looks_expanded(w: f64, h: f64) -> bool {
        let d_pill = (w - PILL_W_LOGICAL).abs() + (h - PILL_H_LOGICAL).abs();
        let d_exp = (w - EXPANDED_W_LOGICAL).abs() + (h - EXPANDED_H_LOGICAL).abs();
        d_exp < d_pill
    }

    /// 可见内容矩形（物理像素）：形态取权威状态,偏移/尺寸取逻辑常量 × 该屏
    /// scale——不读窗口 size 反推（跨屏 DPI 重排会让物理尺寸 ≠ 逻辑 × 本屏 scale）。
    fn content_rect(pos: (i32, i32), scale: f64, expanded: bool) -> (i32, i32, i32, i32) {
        let (w, h) = if expanded {
            (EXPANDED_INNER_W_LOGICAL, EXPANDED_INNER_H_LOGICAL)
        } else {
            (PILL_INNER_W_LOGICAL, PILL_INNER_H_LOGICAL)
        };
        let (pad_l, pad_t) = content_pad(expanded);
        (
            pos.0 + (pad_l * scale).round() as i32,
            pos.1 + (pad_t * scale).round() as i32,
            (w * scale).round() as i32,
            (h * scale).round() as i32,
        )
    }

    /// 内容矩形到「某条竖缘」的贴合间距（物理像素）：取**屏内一侧**的距离,
    /// 跨线或越到屏外一律 0——「贴着这条缘」（含用力往外推过了头）是一种意图,
    /// 不该因为推得太远反而被判成「离线很远」。
    /// 全场唯一判据函数,不再区分「自由态吸附」与「已停靠拖离」。
    fn edge_gap(content: (i32, i32, i32, i32), work: &WorkArea, edge: OrbDockEdge) -> i32 {
        let (l, r) = (content.0, content.0 + content.2);
        match edge {
            // 屏在左缘右侧：内容左缘仍在缘右 ⇒ 距离 = 左缘 − 缘;跨线/越过 ⇒ 0
            OrbDockEdge::Left => (l - work.left).max(0),
            // 屏在右缘左侧：内容右缘仍在缘左 ⇒ 距离 = 缘 − 右缘;跨线/越过 ⇒ 0
            OrbDockEdge::Right => (work.right - r).max(0),
        }
    }

    /// 松手落点 → 贴边目标：在**内容所在屏**的两条竖缘里取最近的一条,间距
    /// ≤ tol 才算贴上（None = 自由落点）。判定输入只有「内容矩形 + 内容所在
    /// 屏」——从哪里拖来、此前贴在哪台屏哪条缘、此前什么形态全都不是输入,
    /// 跨屏/跨接缝行为因此全局一致（同一条接缝属于两台屏,归属由内容中心定）。
    fn dock_target(
        screen: &Screen,
        content: (i32, i32, i32, i32),
        tol: i32,
    ) -> Option<(i32, OrbDockEdge)> {
        let mut best: Option<(i32, OrbDockEdge)> = None;
        for edge in [OrbDockEdge::Left, OrbDockEdge::Right] {
            let gap = edge_gap(content, &screen.work, edge);
            if gap <= tol && best.map_or(true, |(g, _)| gap < g) {
                best = Some((gap, edge));
            }
        }
        best
    }

    /// 停靠锚点：内容中心 Y 相对工作区顶部的比例（0..1;工作区高度变化时按
    /// 比例重放,避免绝对像素越界）。
    fn anchor_ratio(content: (i32, i32, i32, i32), work: &WorkArea) -> f64 {
        let center_y = content.1 as f64 + content.3 as f64 * 0.5;
        ((center_y - work.top as f64) / (work.bottom - work.top).max(1) as f64).clamp(0.0, 1.0)
    }

    // ---------- 拖动松手：统一的落点判定与归位 ----------

    /// 拖动松手：只跑一次落点判定（`dock_target`）,结果分三种走向——
    /// - 贴到内容所在屏的某条竖缘 ⇒ **dock**：写停靠状态（边 + 锚点 + 那台屏的
    ///   工作区）并按该屏归位;此前是自由态（或形态失配）就广播,前端切竖条;
    ///   此前已停靠（沿边缘上下挪 / 换屏换边）则只归位,不广播（形态没变）;
    /// - 未贴边且此前已停靠 ⇒ **undock**：清状态 + 广播,展开归位交 `orb_undock`
    ///   （前端发起,尺寸与位置原子完成）;
    /// - 未贴边且自由态 ⇒ **钳回**内容所在屏工作区完整显示（不裁切、不压任务栏）。
    ///
    /// 参照屏永远是**内容当前所在屏**（不是历史停靠屏）——判定与归位同屏,
    /// 跨屏拖动后不会再出现「按旧屏归位」的贴边距离漂移或侵入接缝。
    fn on_exit() {
        let Some(ctx) = CTX.get() else { return };
        let app = ctx.window.app_handle();
        let Some((pos, size)) = window_geom(ctx) else { return };
        let screens = screens(&ctx.window);
        if screens.is_empty() {
            return;
        }
        let si = content_screen_index(&screens, pos, size);
        let screen = screens[si];

        // 拖动判定：本次移动循环里窗口确实被拖走过 → 广播,前端据此抑制 hover
        // 提示。必须在任何归位动作之前判定——归位会改窗口位置,那之后的差值不再
        // 是用户拖动量。
        if let Some((ox, oy)) = take_drag_origin() {
            if (pos.0 - ox).abs() >= ctx.drag_threshold || (pos.1 - oy).abs() >= ctx.drag_threshold {
                use tauri::Emitter;
                crate::dev_log!("[orb-dock] dragged ({ox},{oy}) -> ({},{})", pos.0, pos.1);
                let _ = app.emit("orb-dragged", true);
            }
        }

        let expanded = super::expanded_state(&app);
        let content = content_rect(pos, screen.scale, expanded);
        let tol = (DOCK_TOLERANCE_LOGICAL * screen.scale).round() as i32;
        let target = dock_target(&screen, content, tol);
        let was_docked = super::dock_state_for(&app).is_some();
        crate::dev_log!(
            "[orb-dock] release content=[{},{},{},{}] scale={} tol={tol} was_docked={was_docked} target={target:?}",
            content.0,
            content.1,
            content.2,
            content.3,
            screen.scale
        );

        match target {
            Some((_, edge)) => {
                let ratio = anchor_ratio(content, &screen.work);
                let state = OrbDockState {
                    edge,
                    anchor_y_ratio: ratio,
                    work: [screen.work.left, screen.work.top, screen.work.right, screen.work.bottom],
                };
                super::set_dock_state(&app, Some(state));
                place_docked(&ctx.window, edge, ratio, &screen);
                // 只在「前端需要把形态切成竖条」时广播：原本已停靠（含沿边缘
                // 微调、换屏换边）时不发——形态没变,刷一串无意义广播没好处;
                // `expanded` 兜住「状态说已停靠、形态却是表盘」的失配自愈。
                if !was_docked || expanded {
                    use tauri::Emitter;
                    let _ = app.emit(
                        "orb-dock-changed",
                        OrbDockChanged { docked: true, edge: Some(edge) },
                    );
                }
            }
            None if was_docked => {
                crate::dev_log!("[orb-dock] undock (no edge within {tol} on screen {si})");
                super::set_dock_state(&app, None);
                use tauri::Emitter;
                let _ = app.emit("orb-dock-changed", OrbDockChanged { docked: false, edge: None });
            }
            None => {
                // 自由态没贴到缘：内容钳回所在显示器工作区完整显示（不裁切、
                // 不压任务栏）。越界量小时视觉上就是「被轻轻推回」。
                clamp_into_work(&ctx.window, &screen, content);
            }
        }
    }

    /// dock 几何归位：竖条贴缘（**本体**距缘 4px;窗口透明边距出屏）,锚 Y 按比例
    /// 回放并钳在工作区内。全程用**目标屏自己的 scale** 换算物理尺寸与位置——
    /// 窗口此刻可能在另一台 DPI 不同的屏上（tao 缓存值不可信）,混用会让竖条
    /// 尺寸错、离缘间距错。
    fn place_docked(window: &WebviewWindow, edge: OrbDockEdge, ratio: f64, screen: &Screen) {
        let scale = screen.scale;
        let w = (PILL_W_LOGICAL * scale).round() as i32;
        let h = (PILL_H_LOGICAL * scale).round() as i32;
        // 竖条本体距工作区缘 4px ⇒ 窗口缘 = 缘 ∓ （边距 − 间隙),透明边距出屏
        let shift = ((PILL_INSET_LOGICAL - PILL_EDGE_GAP_LOGICAL) * scale).round() as i32;
        let x = match edge {
            OrbDockEdge::Left => screen.work.left - shift,
            OrbDockEdge::Right => screen.work.right - w + shift,
        };
        let center_y = screen.work.top as f64 + ratio * (screen.work.bottom - screen.work.top) as f64;
        let y = (center_y.round() as i32 - h / 2)
            .clamp(screen.work.top, (screen.work.bottom - h).max(screen.work.top));
        super::set_expanded_state(window.app_handle(), false);
        crate::dev_log!(
            "[orb-dock] place_docked {edge:?} ratio={ratio:.3} scale={scale} -> ({x},{y}) {w}x{h}"
        );
        settle(window, x, y, w, h);
    }

    /// 展开归位（orb_undock 调用）：**原子**完成「设展开尺寸 + 内容原地长大 +
    /// 钳进内容所在显示器工作区」。
    ///
    /// 尺寸与位置必须一次做完：前端若另外再调一次 set_orb_size（内容锚定）,两次
    /// 位置补偿会叠加成 199px 级的横窜。钳制对象是**展开后的内容矩形 + 阴影余量**
    /// （窗口右侧/底部是 hover 提示位画布,按窗口矩形钳会把卡片推离屏边一两百像素）。
    pub fn undock_ready(window: &WebviewWindow, expand_w: Option<f64>, expand_h: Option<f64>) {
        let Ok(pos) = window.outer_position() else { return };
        let Ok(size) = window.outer_size() else { return };
        let screens = screens(window);
        if screens.is_empty() {
            return;
        }
        let si = content_screen_index(&screens, (pos.x, pos.y), (size.width as i32, size.height as i32));
        let screen = screens[si];
        let scale = screen.scale;

        // 原内容原点（展开前的形态 = dock 态的收起竖条）——内容原地长大,这个点不动
        let app = window.app_handle();
        let cur_expanded = super::expanded_state(&app);
        let (pad_l_c, pad_t_c) = content_pad(cur_expanded);
        let ox = pos.x + (pad_l_c * scale).round() as i32;
        let oy = pos.y + (pad_t_c * scale).round() as i32;

        // 展开后窗口逻辑尺寸（前端传入为准,缺省回退常量）
        let ew = expand_w.filter(|v| *v > 1.0).unwrap_or(EXPANDED_W_LOGICAL);
        let eh = expand_h.filter(|v| *v > 1.0).unwrap_or(EXPANDED_H_LOGICAL);
        let win_w = (ew * scale).round() as i32;
        let win_h = (eh * scale).round() as i32;

        // 内容（含阴影余量）钳进工作区 → 由「内容原点 − 展开态偏移」反推窗口位置
        let m = (CONTENT_MARGIN_LOGICAL * scale).round() as i32;
        let gap = (EXPAND_GAP_LOGICAL * scale).round() as i32;
        let cw = ((EXPANDED_INNER_W_LOGICAL + 2.0 * CONTENT_MARGIN_LOGICAL) * scale).round() as i32;
        let ch = ((EXPANDED_INNER_H_LOGICAL + 2.0 * CONTENT_MARGIN_LOGICAL) * scale).round() as i32;
        let min_x = screen.work.left + gap;
        let min_y = screen.work.top + gap;
        let max_x = (screen.work.right - cw - gap).max(min_x);
        let max_y = (screen.work.bottom - ch - gap).max(min_y);
        let rx = (ox - m).clamp(min_x, max_x);
        let ry = (oy - m).clamp(min_y, max_y);
        let tx = rx + m - (EXPANDED_PAD_L_LOGICAL * scale).round() as i32;
        let ty = ry + m - (EXPANDED_PAD_T_LOGICAL * scale).round() as i32;

        super::set_expanded_state(&app, true);
        crate::dev_log!(
            "[orb-dock] undock_ready content_origin=({ox},{oy}) scale={scale} -> win ({tx},{ty}) {win_w}x{win_h}"
        );
        settle(window, tx, ty, win_w, win_h);
    }

    /// 尺寸切换 + 内容锚定（手动折叠 / 自由态展开走这里）：换尺寸后把窗口位置
    /// 补齐,保证**内容原点在屏上不动**,并同步 Rust 侧形态状态。
    pub fn set_size_anchored(window: &WebviewWindow, width: f64, height: f64) {
        let Ok(pos) = window.outer_position() else {
            let _ = window.set_size(LogicalSize::new(width, height));
            return;
        };
        let Ok(size) = window.outer_size() else {
            let _ = window.set_size(LogicalSize::new(width, height));
            return;
        };
        let screens = screens(window);
        if screens.is_empty() {
            let _ = window.set_size(LogicalSize::new(width, height));
            return;
        }
        let si = content_screen_index(
            &screens,
            (pos.x, pos.y),
            (size.width as i32, size.height as i32),
        );
        let scale = screens[si].scale;
        let app = window.app_handle();
        let cur_expanded = super::expanded_state(&app);
        let (pad_l_c, pad_t_c) = content_pad(cur_expanded);
        let ox = pos.x + (pad_l_c * scale).round() as i32;
        let oy = pos.y + (pad_t_c * scale).round() as i32;
        let new_expanded = looks_expanded(width, height);
        let (pad_l_n, pad_t_n) = content_pad(new_expanded);
        let tx = ox - (pad_l_n * scale).round() as i32;
        let ty = oy - (pad_t_n * scale).round() as i32;
        super::set_expanded_state(&app, new_expanded);
        settle(
            window,
            tx,
            ty,
            (width * scale).round() as i32,
            (height * scale).round() as i32,
        );
    }

    /// 几何归位的统一出口：**先定位（跨屏时触发 DPI 切换）→ 再定尺寸 → 再定位**,
    /// 最后读回校验一次。DPI 切换会让 tao 按「保持逻辑尺寸」重排窗口（尺寸与位置
    /// 都被系统改）,三明治把系统重排的影响收敛掉;读回校验兜住异步重排。
    fn settle(window: &WebviewWindow, x: i32, y: i32, w: i32, h: i32) {
        let (w, h) = (w.max(1), h.max(1));
        let _ = window.set_position(PhysicalPosition::new(x, y));
        let _ = window.set_size(PhysicalSize::new(w as u32, h as u32));
        let _ = window.set_position(PhysicalPosition::new(x, y));
        if let Ok(p) = window.outer_position() {
            if (p.x - x).abs() > SETTLE_TOL_PHYS || (p.y - y).abs() > SETTLE_TOL_PHYS {
                crate::dev_log!(
                    "[orb-dock] settle reposition ({x},{y}) -> got ({},{})",
                    p.x,
                    p.y
                );
                let _ = window.set_position(PhysicalPosition::new(x, y));
            }
        }
        if let Ok(s) = window.outer_size() {
            if (s.width as i32 - w).abs() > SETTLE_TOL_PHYS
                || (s.height as i32 - h).abs() > SETTLE_TOL_PHYS
            {
                crate::dev_log!(
                    "[orb-dock] settle resize {w}x{h} -> got {}x{}",
                    s.width,
                    s.height
                );
                let _ = window.set_size(PhysicalSize::new(w as u32, h as u32));
            }
        }
    }

    /// 自由松手/恢复的边界钳制：**内容矩形 + 阴影余量**钳回所在显示器工作区
    /// （物理像素）。窗口比工作区还大（极端小屏）时钳到左上角。返回是否挪位。
    fn clamp_into_work(window: &WebviewWindow, screen: &Screen, content: (i32, i32, i32, i32)) -> bool {
        let m = (CONTENT_MARGIN_LOGICAL * screen.scale).round() as i32;
        let (cx, cy, cw, ch) = (content.0 - m, content.1 - m, content.2 + 2 * m, content.3 + 2 * m);
        let max_x = (screen.work.right - cw).max(screen.work.left);
        let max_y = (screen.work.bottom - ch).max(screen.work.top);
        let tx = cx.clamp(screen.work.left, max_x);
        let ty = cy.clamp(screen.work.top, max_y);
        let (dx, dy) = (tx - cx, ty - cy);
        if dx == 0 && dy == 0 {
            return false;
        }
        let Ok(pos) = window.outer_position() else { return false };
        // 内容随窗口同向平移 ⇒ 目标差值直接加到窗口位置上,窗口内偏移自动抵消
        crate::dev_log!(
            "[orb-dock] clamp ({},{}) -> ({},{})",
            pos.x,
            pos.y,
            pos.x + dx,
            pos.y + dy
        );
        let _ = window.set_position(PhysicalPosition::new(pos.x + dx, pos.y + dy));
        true
    }

    /// dock 态重启恢复（模块级入口;无子类化上下文也安全）：按 edge +
    /// anchor_y_ratio 重算贴边位置——出屏补偿是 place_docked 的专属语义,
    /// 通用 clamp 会把窗口拉回屏内（竖条离缘变 16px,与运行时停客观感不一致）。
    /// 目标屏优先按记录的工作区匹配当前显示器（显示器拔插/DPI 变更后自适应）。
    pub fn restore_docked(window: &WebviewWindow, dock: &OrbDockState) {
        let screens = screens(window);
        if screens.is_empty() {
            return;
        }
        let idx = match screens.iter().position(|s| same_work(&s.work, &dock.work)) {
            Some(i) => i,
            None => {
                let Ok(pos) = window.outer_position() else { return };
                let Ok(size) = window.outer_size() else { return };
                content_screen_index(
                    &screens,
                    (pos.x, pos.y),
                    (size.width as i32, size.height as i32),
                )
            }
        };
        place_docked(window, dock.edge, dock.anchor_y_ratio, &screens[idx]);
    }

    /// restore 后钳制（模块级入口;无子类化上下文也安全）。
    pub fn clamp_restored(window: &WebviewWindow) {
        let Ok(pos) = window.outer_position() else { return };
        let Ok(size) = window.outer_size() else { return };
        let screens = screens(window);
        if screens.is_empty() {
            return;
        }
        let si = content_screen_index(
            &screens,
            (pos.x, pos.y),
            (size.width as i32, size.height as i32),
        );
        let screen = screens[si];
        let expanded = super::expanded_state(window.app_handle());
        let content = content_rect((pos.x, pos.y), screen.scale, expanded);
        clamp_into_work(window, &screen, content);
    }

    /// 记录尺寸（物理像素,来自落盘几何）→ 当前尺寸下的窗口位置正量
    /// （= 当时形态的内容偏移 − 当前（收起）形态的内容偏移）。
    /// 启动期近似实现：用窗口当前 scale 定性记录形态即可——随后 restore_docked /
    /// clamp_restored 会用显式显示器模型做精确归位。
    pub fn content_anchor_offset(window: &WebviewWindow, rec_w: f64, rec_h: f64) -> (i32, i32) {
        let scale = window.scale_factor().unwrap_or(1.0);
        let rec_expanded = looks_expanded(rec_w / scale, rec_h / scale);
        let (rec_l, rec_t) = content_pad(rec_expanded);
        let (cur_l, cur_t) = content_pad(false);
        (
            ((rec_l - cur_l) * scale).round() as i32,
            ((rec_t - cur_t) * scale).round() as i32,
        )
    }

    /// 点击穿透命中判定：光标在两态各自的「交互主体」矩形内 → None（走默认处理）;
    /// 落在透明呼吸位 → Some（HTTRANSPARENT)。
    /// lParam 低/高 16 位 = 光标屏幕坐标（各按 i16 符号扩展——多显示器负坐标在
    /// i16 范围内;>32767 的超大拼接桌面会溢出,已知边界不覆盖）。
    fn transparent_margin_hit(lparam: LPARAM) -> Option<LRESULT> {
        let ctx = CTX.get()?;
        let x = (lparam & 0xFFFF) as u16 as i16 as i32;
        let y = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;
        let pos = ctx.window.outer_position().ok()?;
        let scale = ctx.window.scale_factor().unwrap_or(1.0);
        let expanded = super::expanded_state(ctx.window.app_handle());
        let (left, top, w, h) = content_rect((pos.x, pos.y), scale, expanded);
        let inside = x >= left && x < left + w && y >= top && y < top + h;
        if inside {
            None
        } else {
            Some(HTTRANSPARENT)
        }
    }

    /// 记下移动循环起点（WM_ENTERSIZEMOVE;早于循环内任何位移）。
    fn remember_drag_origin() {
        let Some(ctx) = CTX.get() else { return };
        let Ok(pos) = ctx.window.outer_position() else { return };
        *ctx.drag_origin.lock().unwrap() = Some((pos.x, pos.y));
    }

    /// 取出移动循环起点（一次性;EXIT 判定后即清,不残留到下一轮）。
    fn take_drag_origin() -> Option<(i32, i32)> {
        CTX.get().and_then(|ctx| ctx.drag_origin.lock().unwrap().take())
    }

    fn window_geom(ctx: &Ctx) -> Option<((i32, i32), (i32, i32))> {
        let pos = ctx.window.outer_position().ok()?;
        let size = ctx.window.outer_size().ok()?;
        Some(((pos.x, pos.y), (size.width as i32, size.height as i32)))
    }

    // ---------- 纯几何单测（多屏判定的回归护栏;不触窗口/系统 API） ----------

    #[cfg(test)]
    mod tests {
        use super::*;

        fn wa(l: i32, t: i32, r: i32, b: i32) -> WorkArea {
            WorkArea { left: l, top: t, right: r, bottom: b }
        }

        /// 内容矩形 = (左, 上, 宽, 高)
        fn rect(l: i32, t: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
            (l, t, w, h)
        }

        /// 统一贴边判据：内容到缘的贴合间距取**屏内一侧**——跨线或越到屏外
        /// 一律 0（往外推是「更贴」的意图,不该被判成「离线很远」）。
        #[test]
        fn edge_gap_zero_when_touching_or_outside() {
            let work = wa(0, 0, 2560, 1368);
            // 右缘：屏内 10 / 跨线 / 越到屏外
            assert_eq!(edge_gap(rect(2400, 100, 150, 150), &work, OrbDockEdge::Right), 10);
            assert_eq!(edge_gap(rect(2500, 100, 150, 150), &work, OrbDockEdge::Right), 0);
            assert_eq!(edge_gap(rect(2600, 100, 150, 150), &work, OrbDockEdge::Right), 0);
            // 左缘镜像：屏内 10 / 跨线 / 越到屏外
            assert_eq!(edge_gap(rect(10, 100, 150, 150), &work, OrbDockEdge::Left), 10);
            assert_eq!(edge_gap(rect(-100, 100, 150, 150), &work, OrbDockEdge::Left), 0);
            assert_eq!(edge_gap(rect(-200, 100, 150, 150), &work, OrbDockEdge::Left), 0);
        }

        /// 判据的另一半：内容留在屏内、离缘多远就是多远——「拖离边缘展开」
        /// 靠它成立（否则竖条永远算贴边,永远弹不出去）。
        #[test]
        fn edge_gap_counts_distance_inside() {
            let work = wa(2560, 0, 5656, 1968);
            // 贴左缘（竖条本体距缘 4 逻辑像素 @1.5 → 6 物理）
            assert_eq!(edge_gap(rect(2566, 100, 36, 126), &work, OrbDockEdge::Left), 6);
            // 移到屏内 560 物理像素处
            assert_eq!(edge_gap(rect(3120, 100, 36, 126), &work, OrbDockEdge::Left), 560);
            // 右缘镜像：内容右缘距缘 20
            assert_eq!(edge_gap(rect(5600, 100, 36, 126), &work, OrbDockEdge::Right), 20);
        }

        /// 落点判定的输入只有「内容所在屏 + 内容矩形」：竖条从别处拖到这台屏的
        /// 任一缘都判贴上——历史停靠屏/停靠边不参与（回归：从别的屏拖到本屏右缘
        /// 曾被判「离原停靠缘太远」而误弹表盘;跨接缝后曾被按旧屏归位）。
        #[test]
        fn dock_target_reads_only_own_screen_edges() {
            let right = Screen {
                rect: wa(2560, 0, 5656, 1968),
                work: wa(2560, 0, 5656, 1968),
                scale: 1.5,
            };
            let tol = (DOCK_TOLERANCE_LOGICAL * right.scale).round() as i32; // 24
            // 贴本屏左缘（从接缝另一侧拖来）：内容左缘距缘 6 物理像素
            assert_eq!(
                dock_target(&right, rect(2566, 100, 36, 126), tol),
                Some((6, OrbDockEdge::Left))
            );
            // 贴本屏右缘（从别处拖到最右缘）：内容右缘距缘 16 物理像素
            assert_eq!(
                dock_target(&right, rect(5604, 100, 36, 126), tol),
                Some((16, OrbDockEdge::Right))
            );
            // 越到本屏左缘之外（跨缝/屏外）仍判贴上,且贴左缘
            assert_eq!(
                dock_target(&right, rect(2520, 100, 36, 126), tol),
                Some((0, OrbDockEdge::Left))
            );
            // 屏中间：两缘都够不着 → 自由落点
            assert_eq!(dock_target(&right, rect(4000, 100, 36, 126), tol), None);
        }

        /// 屏归属：矩形命中优先,全不命中取最近（拖出屏外的兜底）。
        #[test]
        fn screen_pick_hits_rect_then_nearest() {
            let screens = vec![
                Screen { rect: wa(0, 0, 2560, 1368), work: wa(0, 0, 2560, 1368), scale: 2.0 },
                Screen { rect: wa(2560, 0, 5656, 1968), work: wa(2560, 0, 5656, 1968), scale: 1.5 },
            ];
            assert_eq!(screen_index_at(&screens, 100, 100), 0);
            assert_eq!(screen_index_at(&screens, 2559, 100), 0);
            assert_eq!(screen_index_at(&screens, 2560, 100), 1);
            assert_eq!(screen_index_at(&screens, -5000, 100), 0);
            assert_eq!(screen_index_at(&screens, 9000, 100), 1);
        }

        /// 两态容器都绕**视觉中心**对称 ⇒ 窗口中心 = 视觉中心（归属判定与
        /// 系统按窗口中心的判定、与人的感知三者一致的前提）：
        /// - 收起态视觉中心 = 竖条本体中心（本体居中）;
        /// - 展开态视觉中心 = 表盘中心（内容矩形还带右侧按钮列,故不是矩形中心）。
        #[test]
        fn window_center_matches_visual_center() {
            let scale = 1.5;
            let pos = (1000, 800);
            // 收起态：竖条本体中心 = 窗口中心
            let (cl, ct, cw, ch) = content_rect(pos, scale, false);
            let wcx = pos.0 as f64 + PILL_W_LOGICAL * scale / 2.0;
            let wcy = pos.1 as f64 + PILL_H_LOGICAL * scale / 2.0;
            assert!((cl as f64 + cw as f64 / 2.0 - wcx).abs() <= 1.0);
            assert!((ct as f64 + ch as f64 / 2.0 - wcy).abs() <= 1.0);
            // 展开态：表盘中心 = 窗口中心
            let dial_cx = pos.0 as f64 + (EXPANDED_PAD_L_LOGICAL + DIAL_RADIUS_LOGICAL) * scale;
            let dial_cy = pos.1 as f64 + (EXPANDED_PAD_T_LOGICAL + DIAL_RADIUS_LOGICAL) * scale;
            let ewcx = pos.0 as f64 + EXPANDED_W_LOGICAL * scale / 2.0;
            let ewcy = pos.1 as f64 + EXPANDED_H_LOGICAL * scale / 2.0;
            assert!((dial_cx - ewcx).abs() <= 1.0, "expanded x: {dial_cx} vs {ewcx}");
            assert!((dial_cy - ewcy).abs() <= 1.0, "expanded y: {dial_cy} vs {ewcy}");
        }

        /// 形态定性：外部传入的逻辑尺寸按最近的常量归属（收起 ↔ 展开）。
        #[test]
        fn looks_expanded_by_nearest_size() {
            assert!(!looks_expanded(PILL_W_LOGICAL, PILL_H_LOGICAL));
            assert!(looks_expanded(EXPANDED_W_LOGICAL, EXPANDED_H_LOGICAL));
        }
    }
}
