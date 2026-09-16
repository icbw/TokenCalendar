//! 悬浮球贴边停靠——「拖到屏幕边缘松手自动贴边收起成竖条,从边缘
//! 拖出/双击展开回卡片」的 edge docking 范式。多显示器（含混合 DPI）下形态与
//! 位置必须稳定,几何模型因此立在四条硬约束上：
//!
//! 1. **参照物是显示器,不是窗口**。所有判定与归位都用「当前显示器枚举里那台
//!    目标屏的 rcWork + 该屏自己的 scale」。窗口自身的 scale_factor（tao 缓存）
//!    与 MonitorFromWindow（按窗口矩形中心归属）在多屏接缝上会互相打架：窗口
//!    中心一过缝归属就翻到隔壁,于是「光标在 A 屏、贴边意图是 A 屏右缘」被判成
//!    「触到 B 屏左缘」——竖条跳到接缝另一侧、卡片在错误的一屏展开。
//!
//! 2. **判定对象按语义分档**：
//!     **自由态表盘看光标**——用户的手感就是「把鼠标推到屏边」,光标即
//!    鼠标点击/拖动位置,**参照屏仍是内容所在屏**（光标只是测量点,不参与选屏）,
//!    几像素容差只是「检测屏缘的兼容范围」;用内容矩形判会明显提前（展开态内容
//!    矩形右侧还挂着 6px 间隙 + 16px 按钮列,表盘离缘还有二三十像素就收起——
//!    「圆盘还没接触屏幕就触发」）;
//!     **已停靠保持/拖离与钳制看可见内容矩形**——竖条贴缘时本体离缘 4px、光标
//!    停在主体中部,拿光标判会立刻误判离开;窗口矩形也不行（含透明边距/提示位
//!    画布,越界 ≠ 内容越界）;
//!    ′ **自由态竖条也看可见内容矩形**（㊶,与同尺同容差）——竖条的内容
//!    就是本体,不存在表盘那种「按钮列提前触发」的顾虑:手动折叠的竖条
//!    拖到离缝 7px 松手不吸附（光标在竖条中部、离缘 19px 不触缘,屏内拖动又触发
//!    不了的跨缝补判）,被钳到离缘 24px 处——不是「重定位吸附」的固定距离;
//!     **跨缝补判**——拖动中窗口中心换过屏
//!    （系统已按「窗口过半」把窗口判给新屏、DPI 也跟着切）时,光标必然已经随拖动
//!    深入新屏、离缝远超 eps ⇒ 整条失效（从接缝左侧拖到右侧,表盘停在缝上
//!    既不贴边也不弹开;反向因为「越过也算」的外侧无界而照常触发）。补判改用
//!    **主体是否完整显示**这条口径：主体离新屏近缘超过几像素 = 已整体进入新屏 ⇒
//!    不再贴边;仍压着缝（尚未完整显示）才算贴。见 `seam_landing_target`。
//!    屏归属 = **窗口中心**：两态容器都绕内容（视觉）中心对称 ⇒ 窗口中心 =
//!    视觉中心,与系统按窗口中心的归属判定同尺。
//!
//! 3. **参照屏唯一 + 判定对象按语义分档**。
//!    松手只在**内容当前所在屏**（自由态与已停靠态都是这一台,光标/历史停靠屏
//!    都不做候选）上解一个问题：本路的判定对象离该屏两条竖缘最近的一条是否触缘。
//!    旧版已停靠态盯「停靠状态记录的那台屏、那条缘」,于是竖条从别处拖到另一台屏
//!    的边缘时会因「离原停靠缘很远」被判拖离（弹出表盘）,跨接缝后还会按旧屏归位
//!    （贴边距离漂移、甚至侵入接缝）。
//!    判定对象：自由态表盘 = 光标（+ 跨缝补判）;自由态竖条与已停靠态 = 可见内容
//!    矩形（同尺同容差）。贴到某条缘 ⇒ 竖条（保持/进入,含换屏换边）;未贴且已停靠
//!    ⇒ 展开、未贴且自由 ⇒ 钳回（自由态竖条吸附后即成为已停靠,再拖离才展开）。
//!    判定对象与容差见硬约束 2 与两个常量的注释。
//!
//! 4. **一次归位一个执行者**。形态切换（尺寸）与位置补偿必须在同一次调用里原子
//!    完成：前端 set_orb_size（内容锚定）与 Rust 归位各自补偿一次位置的话,两个
//!    invoke 并发时补偿被算两遍,卡片会横向窜两百像素。
//!
//! 5. **画布不抢鼠标：主体之外动态让出**。窗口绕表盘对称
//!    展开（580×310),画布（内容之外的透明区)只承载 hover 提示,不该拦鼠标。静态
//!    `HTTRANSPARENT`（WM_NCHITTEST）只覆盖「光标从窗口外进入画布」的通路——
//!    从主体滑入画布时光标始终没离开本窗口矩形,系统不会重询命中测试,消息继续
//!    归本窗口（画布遮挡其他程序的 hover 与操作,右键也仍被响应）。
//!    补一条**动态让出**,两道机制一起上：
//!     `set_window_body_region`——**硬保证**：光标离开交互主体即把窗口区域
//!      （`SetWindowRgn`）收紧到「主体 + 阴影余量」,区域外的画布在几何上不属于
//!      窗口,命中测试绝不会命中（只设样式位不够——㊼ :`WS_EX_TRANSPARENT`
//!      已生效、画布照样遮挡）;
//!     `WS_EX_TRANSPARENT` 样式位 + `HTTRANSPARENT` 命中判定——系统级快路径。
//!    恢复（回主体）= 区域还原整窗 + 清样式位。穿透期间窗口收不到鼠标消息,
//!    让出/恢复全靠 `PASS_POLL_MS` **常驻轮询**（35ms;「光标从窗口外进入画布」
//!    也由它兜底——那条路径同样收不到任何消息）。命中口径与点击穿透同源
//!    （`body_rect` = 可见内容矩形 + 滞回缓冲）;拖动中不切换（移动循环持有鼠标）。
//!
//!    **⚠ 翻转的视觉副作用：闪出原生标题栏**。
//!    根子是窗口对系统的两句话自相矛盾：tao 的 `WM_NCCALCSIZE` 对未装饰窗口返回 0
//!    （**非客户区面积为零**）,可 `to_window_styles` 又给 `decorations:false` 的窗口
//!    照置 `WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX`（它只在自己的
//!    `AdjustWindowRect*` 里屏蔽 caption,样式位照留）,且窗口照常**接受非客户区
//!    绘制消息**。平时 DWM 替它合成框架、框架面积为零 ⇒ 看不;但带**窗口区域**
//!    的窗口 DWM 不合成框架,走 user32/uxtheme 的旧式非客户区绘制——那条路径按
//!    **样式**（不按 NCCALCSIZE 的结果）决定画什么,`WM_NCPAINT` / `WM_NCACTIVATE` /
//!    `WM_NCUAHDRAWCAPTION` 一到,就把「图标 + 标题 + 最小化/关闭」直接画进窗口
//!    矩形顶部,下一帧再被 WebView 盖掉 ⇒ 闪 2 帧。让出翻转 = 区域装/卸 + 光标换
//!    窗口 = 正好凑齐这些消息。法不在让出流程里,而是让窗口**言行一致**,三道
//!    互相独立的闸：
//!     `subclass_proc` 拒绝非客户区绘制消息（`WM_NCPAINT` / `WM_NCUAHDRAWCAPTION` /
//!      `WM_NCUAHDRAWFRAME` 直接返回,`WM_NCACTIVATE` 以 lParam = -1 转发——
//!      DefWindowProc 据此**不重绘**非客户区,tao 的焦点逻辑照常）——非客户区既然
//!      声明为零,就不该有任何路径能画它,与 tao 写回什么样式无关;
//!     `strip_frame_styles` 摘掉整组「带框窗口」特征位（`FRAME_STYLE_BITS` =
//!      CAPTION | SYSMENU | THICKFRAME | MINIMIZEBOX | MAXIMIZEBOX）——命中测试与
//!      主题引擎都不再把它当带按钮的框窗（tao 每次 flag 变更都会把整组样式写回 +
//!      `SWP_FRAMECHANGED`,故由让出轮询逐帧对账复摘）;
//!     `disable_nc_rendering`（DWMNCRP_DISABLED）把非客户区绘制**固定**在旧式路径
//!      上——区域装/卸时不再在「DWM 合成 ↔ 旧式绘制」之间来回切换（切换本身也会
//!      触发一次框架重绘）。三条都只关「框架能不能被画出来」,不碰让出语义。
//!
//! DPI 切换的时序：把窗口移到另一台
//! DPI 不同的显示器时,Windows 发 WM_DPICHANGED,tao 按「保持逻辑尺寸」重排窗口（尺寸与
//! 位置都会被系统改）。而拖动期间排队的那条消息**在 WM_EXITSIZEMOVE 之后**才到 ⇒ 松手瞬间
//! 写完的归位会被它整段抹掉（`settle reposition （2494,749) -> got （1977,603)`）。
//! 因此三条一起成立：
//!  **尺寸写物理值**（㊵ 推翻初版的「写逻辑值」）——`set_size（LogicalSize)` 的换算在
//!   tao 内部用**消息驱动**的 scale 缓存,跨屏松手时缓存还是旧屏值,56×116 逻辑会被写成
//!   112×232 物理（`write rect （2494,1070) 56x116 logical @1.5 （expect 84x174 phys)`
//!   紧跟 `rect resize 84x174 -> got 112x232`）;物理值不经缓存换算,写的就是目标矩形,
//!   「写错 ⇒ 尺寸越缝 ⇒ 系统重估 DPI ⇒ 再重排 ⇒ 再写错」的自激风暴因此断根;
//!  几何只有 `apply_desired` 一个出口,它把「意图」（`Desired`）记进 Ctx,并用**目标屏
//!   自己的 scale**把逻辑尺寸当场换成物理值（`desired_rect`）;
//!  子类化转发 WM_DPICHANGED 之后重放意图（`replay_desired`）,把矩形钉回目标值;
//!   写入幂等（`write_rect` 已达成即静默跳过）,重放不再喂养风暴;移动循环进行中不重放
//!   （那段时间用户的手是几何的主人）。
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

/// 写形态并**变化即落盘**（形态是启动恢复的输入——「恢复上次
/// 退出前的形态与位置」;低频离散事件（双击/拖动松手/贴边）,与 `set_dock_state`
/// 同一条落盘纪律。失败静默——下次显隐/退出兜底重写）。
pub fn set_expanded_state(app: &AppHandle, expanded: bool) {
    let state = app.state::<crate::AppState>();
    let prev = state.orb_expanded.swap(expanded, Ordering::SeqCst);
    if prev != expanded {
        crate::window_state::persist(app, &state);
    }
}

/// 只写内存、**不落盘**（启动恢复专用：`restore_orb` 执行时窗口几何还在归位途中,
/// 此刻落盘会把中间态写进文件;最终形态会随后续任意 persist 固化）。
pub fn store_expanded_state(app: &AppHandle, expanded: bool) {
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

/// 前端启动恢复查询（OrbWindow 挂载时）：Rust 侧权威的停靠态 + 形态。
/// 前端**不再按 window.innerWidth 猜形态**：页面可能在 restore 归位之前
/// 就已加载,那时窗口还是配置初始尺寸（56×116 竖条）——猜错后挂载对齐会把刚恢复的
/// 表盘缩回竖条。IPC 经启动闸门,此查询必在 restore 之后执行。
#[tauri::command]
pub fn get_orb_form(app: AppHandle) -> Result<OrbForm, String> {
    Ok(OrbForm { dock: dock_state_for(&app), expanded: expanded_state(&app) })
}

/// `get_orb_form` 载荷。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct OrbForm {
    pub dock: Option<OrbDockState>,
    pub expanded: bool,
}

/// dock/undock 广播载荷（前端消费:收起/展开形态切换）。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct OrbDockChanged {
    pub docked: bool,
    pub edge: Option<OrbDockEdge>,
}

/// 启动恢复入口（window_state:restore 调用;仅 Windows）：
/// 按上次退出形态恢复 orb 的尺寸与位置——贴边停靠按 anchor 重算（真正贴边）、
/// 自由态按记录形态与位置恢复、**首次启动默认展开（表盘）+ 居中**。
/// 返回最终形态（写进 AppState.orb_expanded,前端挂载据此对齐）。
/// 参数 `recorded` = 落盘窗口位置 + inner 尺寸（物理像素）;`recorded_expanded` =
/// 落盘形态（旧格式可能缺失,按尺寸反推）。
#[cfg(windows)]
pub fn restore_orb(
    window: &tauri::WebviewWindow,
    recorded: Option<(i32, i32, u32, u32)>,
    recorded_expanded: Option<bool>,
    dock: Option<OrbDockState>,
) -> bool {
    win::restore_orb(window, recorded, recorded_expanded, dock)
}

/// 前端驱动尺寸切换的**内容锚定**入口（手动折叠 / 自由态展开）：
/// 换尺寸的同时把窗口位置补回「内容原点不动」,并同步 Rust 侧形态状态。
/// 跨屏时用内容所在显示器的 scale 换算（不用 tao 缓存值,避免与目标屏不一致）。
#[cfg(windows)]
pub fn set_size_anchored(window: &tauri::WebviewWindow, width: f64, height: f64) {
    win::set_size_anchored(window, width, height);
}

/// 指针让出态的复位钩子（窗口显隐切换调用,见 `win:reset_pointer_pass`）：
/// 隐藏期间没有鼠标消息去复位让出态,不清掉的话下次显示时整窗都会被鼠标穿透;
/// 常驻轮询也随显隐起停（`active` = 窗口此后是否可）。
#[cfg(windows)]
pub fn reset_pointer_pass(window: &tauri::WebviewWindow, active: bool) {
    win::reset_pointer_pass(window, active);
}

#[cfg(not(windows))]
pub fn reset_pointer_pass(window: &tauri::WebviewWindow, active: bool) {
    let _ = (window, active);
}

// ---------- Windows 实现 ----------

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateRectRgn, DeleteObject, GetMonitorInfoW, GetRgnBox, GetWindowRgn, MonitorFromPoint,
        SetWindowRgn, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, SIMPLEREGION,
    };
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_NCRENDERING_POLICY, DWMNCRP_DISABLED,
    };
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetWindowLongPtrW, KillTimer, SetTimer, SetWindowLongPtrW, SetWindowPos,
        GWL_EXSTYLE, GWL_STYLE, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER,
    };

    use super::{OrbDockChanged, OrbDockEdge, OrbDockState};
    use tauri::{Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

    const SUBCLASS_ID: usize = 0x704F_5242;

    // 同 snap.rs：本地常量,不为此引 Win32_UI_WindowsAndMessaging
    const WM_EXITSIZEMOVE: u32 = 0x0232;
    /// WM_ENTERSIZEMOVE（移动循环开始）——与 EXITSIZEMOVE 配对,作为「这次按下
    /// 到底有没有把窗口拖走」的起点（判定见 DRAG_SHIFT_LOGICAL）。
    const WM_ENTERSIZEMOVE: u32 = 0x0231;
    /// WM_NCHITTEST（lParam = 光标屏幕坐标,低/高 16 位各为 x/y 的有符号短整型）。
    const WM_NCHITTEST: u32 = 0x0084;
    /// WM_DPICHANGED（窗口跨到 DPI 不同的显示器）——本模块在**转发给 tao 之后**
    /// 排定一次合并重放（见 `schedule_replay`）：tao 会按「保持逻辑尺寸」把窗口
    /// 重排一次,把我们在 EXITSIZEMOVE 里刚写完的矩形抹掉。
    const WM_DPICHANGED: u32 = 0x02E0;
    /// WM_TIMER（合并重放的定时器到点;定时器 id = `REPLAY_TIMER_ID`）。
    const WM_TIMER: u32 = 0x0113;
    /// 重放去抖定时器的 id（"ORP1"）与合并窗口长度——见 `schedule_replay`。
    const REPLAY_TIMER_ID: usize = 0x4F52_5031;
    const REPLAY_DEBOUNCE_MS: u32 = 40;
    /// WM_NCHITTEST 返回码：命中透明边距 → 忽略本窗口,点击交给下层
    /// （点击穿透——「容器不可交互」;HTTRANSPARENT = -1）。
    const HTTRANSPARENT: LRESULT = -1;
    /// WM_MOUSEMOVE——「指针让出」的一条低成本触发点：从主体滑入画布时光标没有
    /// 离开本窗口矩形,系统不会重询命中测试,只有本窗口自己的鼠标消息能捕获越界。
    const WM_MOUSEMOVE: u32 = 0x0200;
    /// WM_SETTINGCHANGE——系统设置广播（本模块只关心「辅助功能 → 文本大小」：
    /// WebView2 会随之改整体缩放,本模块的物理换算系数要跟着刷,见 `text_scale.rs`）。
    /// 任一顶层窗口都会收到,orb 常驻且已子类化,就借它接;处理后照常交给 tao。
    const WM_SETTINGCHANGE: u32 = 0x001A;
    /// 非客户区绘制消息——本窗口的非客户区
    /// 由 tao 的 WM_NCCALCSIZE 声明为零,这些消息一律不许画：
    /// - `WM_NCPAINT`：旧式框架绘制入口（DefWindowProc 按**样式**画标题栏/边框,
    ///   不看 NCCALCSIZE 的结果——正是「面积为零却画出一条标题栏」的来源）;
    /// - `WM_NCACTIVATE`：DefWindowProc 借它按激活态重绘标题栏;lParam = -1 是
    ///   文档化的「只改状态不重绘」约定;
    /// - `WM_NCUAHDRAWCAPTION` / `WM_NCUAHDRAWFRAME`：
    ///   主题引擎（uxtheme）绘制标题栏/按钮的请求——自绘框架应用（Chromium、Qt）
    ///   统一吞掉的一组。
    const WM_NCPAINT: u32 = 0x0085;
    const WM_NCACTIVATE: u32 = 0x0086;
    const WM_NCUAHDRAWCAPTION: u32 = 0x00AE;
    const WM_NCUAHDRAWFRAME: u32 = 0x00AF;
    /// 指针让出的轮询定时器 id（"ORP2"）与周期——**常驻**（窗口可见期间一直跑）：
    /// 穿透期间窗口收不到鼠标消息,让出/恢复只能靠轮询发现;「光标从窗口外进入
    /// 画布」这条通路也靠它兜底（那条路径下窗口同样收不到任何消息）。35ms 在滑行
    /// 途中约一两帧,进出主体无感;单轮成本 = 几个微秒级系统调用,常驻无压力。
    const PASS_TIMER_ID: usize = 0x4F52_5032;
    const PASS_POLL_MS: u32 = 35;
    /// 让出判定的滞回缓冲（逻辑像素）：光标离主体边缘超过它才算离开;恢复判定
    /// 不带缓冲（回到矩形内即恢复）——两档之间是一条「维持现状」带,边界不抖动。
    const PASS_HYSTERESIS_LOGICAL: f64 = 6.0;
    /// WS_EX_TRANSPARENT：整窗对鼠标透明的**样式位**（让出两件套之一）。
    /// ⚠ （㊼）：单独设这个位**不足以**让画布不再遮挡——日志
    /// `pointer pass ON` 已出现（样式设置成功）,画布照旧吃消息、挡住下层程序。
    /// 真兜底的是 `set_window_body_region`（`SetWindowRgn` 窗口区域收紧,几何级
    /// 硬保证）;本位置保留作系统级快路径。
    /// ⚠ 不仿 tao 的 IGNORE_CURSOR_EVENT 连带加 `WS_EX_LAYERED`（tao
    /// window_state.rs 的映射）：layered 窗口不能显示子窗口内容是经典限制,
    /// 本窗口是 WebView2 + DComp 透明窗,改绘制路径的风险大于收益。
    const WS_EX_TRANSPARENT_BIT: u32 = 0x0000_0020;

    /// 「带框窗口」特征位 = `WS_OVERLAPPEDWINDOW` 去掉 `WS_OVERLAPPED`（0)：
    /// `WS_CAPTION`（0x00C0_0000) | `WS_SYSMENU`（0x0008_0000) | `WS_THICKFRAME`
    /// （0x0004_0000) | `WS_MINIMIZEBOX`（0x0002_0000) | `WS_MAXIMIZEBOX`（0x0001_0000)。
    /// tao 对 `decorations:false` 的窗口**保留** CAPTION | SYSMENU（+ MINIMIZEBOX,
    /// 它只在自己的尺寸计算里屏蔽 caption）,于是窗口在系统眼里**仍是带标题栏、
    /// 带最小化/关闭按钮的窗体**——非客户区被 tao 的 WM_NCCALCSIZE 归零 ⇒ 平时
    /// 不可,但旧式非客户区绘制按**样式**画：㊾ 只摘 CAPTION 时闪出的框架里
    /// 恰好只剩「最小化 + 关闭」两个按钮（= 残留的 SYSMENU | MINIMIZEBOX）。
    /// 整组摘掉,命中测试/主题引擎才不再把它当框窗（`strip_frame_styles`）;
    /// 绘制消息本身另由 `subclass_proc` 拒绝。
    /// 不补 `WS_POPUP`：改窗口大类会牵动 z 序/属主语义,而框架能不能被画出来
    /// 与它无关。
    const FRAME_STYLE_BITS: u32 = 0x00CF_0000;

    /// **拖动位移判定阈值**（逻辑像素）——移动循环结束时窗口位移 ≥ 此值才算
    /// 「真拖动」,广播 `orb-dragged` 供前端抑制 hover 提示。手抖级位移/原地按下
    /// 放开不算（拖动区覆盖整块表盘,mousedown 即进移动循环,「按住没动」必须与
    /// 「拖走了一段」区分开——这是把判定放 Rust 的唯一理由）。
    const DRAG_SHIFT_LOGICAL: f64 = 2.0;

    /// **几像素手感档**（逻辑像素）——本模块两处「贴 / 不贴」的分界**共用同一个数**：
    ///  自由态**光标触缘**：拖动松手时
    ///    **以光标（鼠标点击/拖动位置）为起点**算到工作区左/右缘的距离,≤ 此值
    ///    （或已越过该缘）才算「拖到边上了」,松手收成竖条——用户原话「鼠标点击
    ///    位置作为起点计算屏幕边缘四个像素」;它只是**检测屏幕边缘的兼容范围**,
    ///    不是吸附范围;判据是光标不是内容矩形（展开态内容矩形右侧挂着 6px 间隙 +
    ///    16px 按钮列,拿内容判会让表盘离缘二三十像素就收起,否决）。
    ///  自由态**跨缝补判的主体完整显示**分界：
    ///    主体离新屏近缘 > 此值 ⇒ 已整体进入新屏（完整显示）⇒ 不再贴边。
    /// 取 4：Windows 光标最大坐标 = 屏宽 − 1（严格 0 判定在无侧边任务栏时永远差
    /// 1px 触不到）,2 在实际手感上要求「贴死」,高 DPI 下更紧;4 逻辑像素 ≈ 6 物理
    /// 像素 @1.5,「推到边上」即可触发。
    /// ⚠ 两处同值 = 同一个手感档,改一处即两处生效——**勿拆成两个常量**（拆开必然漂移）。
    const EDGE_EPS_LOGICAL: f64 = 4.0;

    /// **已停靠态保持容差**（逻辑像素）：贴稳的竖条（内容离缘 4px,PILL_EDGE_GAP）
    /// 拖离到 > 此值才判脱边（undock 展开）——取 16 ⇒ 本体要离缘约 12px 才算拖离,
    /// 手抖、沿边缘上下挪、换屏换边都仍算贴着。
    const DOCK_KEEP_TOLERANCE_LOGICAL: f64 = 16.0;

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
    /// 235 + 55 = 580/2、100 + 55 = 310/2（表盘 110×110,视觉中心在内容块左上
    /// （55,55) 处）——容器因此绕视觉中心对称。与前端 CSS 同源。
    /// ⚠ 盘径三轮 150 → 130 → 118 → 110,偏移随之重算（与前端 EXPANDED_PAD 同步改）。
    const EXPANDED_PAD_L_LOGICAL: f64 = 235.0;
    const EXPANDED_PAD_T_LOGICAL: f64 = 100.0;

    /// 表盘半径（逻辑像素;110px 正圆的一半）。两处消费： 校验「窗口中心 =
    /// 表盘中心」这条对称硬约束（见单测）——画布若只往右/下扩,窗口中心会偏离
    /// 视觉中心,系统就会在视觉主体还没过缝时先把窗口判给隔壁屏; `visual_span`
    /// 用它算出跨缝补判要用的**主体跨度**（内容盒右侧要裁掉间隙 + 按钮列）。
    const DIAL_RADIUS_LOGICAL: f64 = 55.0;

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
    /// - 展开态 = 表盘 110 + 间隙 6 + 按钮列 16 = 132 宽,高 110。
    const PILL_INNER_W_LOGICAL: f64 = 24.0;
    const PILL_INNER_H_LOGICAL: f64 = 84.0;
    const EXPANDED_INNER_W_LOGICAL: f64 = 132.0;
    const EXPANDED_INNER_H_LOGICAL: f64 = 110.0;

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

    /// 移动循环的起点（WM_ENTERSIZEMOVE 记,EXIT 取用后即清）。
    #[derive(Clone, Copy)]
    struct DragOrigin {
        /// 起点窗口位置（物理像素）——只用于判定「窗口真被拖走」,不参与 dock 几何。
        pos: (i32, i32),
        /// 起点时内容所在屏的工作区（物理像素）——跨缝补判用（与出口时刻的屏比对,
        /// 换过屏 ⇒ 本次拖动跨过接缝）。用工作区矩形而不是屏序号：显示器热插拔/重排
        /// 会让序号漂移,工作区矩形是稳定标识。
        work: [i32; 4],
    }

    /// 归位意图：**窗口矩形的唯一权威**。每次归位/形态切换都会
    /// 记下「想要的结果」而不是「算好的坐标」——系统因 DPI 变更重排窗口后,由
    /// `replay_desired` 按**新 DPI** 重算并重新施加,跨屏归位因此不依赖写入时序。
    #[derive(Clone, Copy, Debug)]
    enum Desired {
        /// 贴边：竖条贴 `work` 那台屏的 `edge` 缘,中心 Y 按 `ratio` 落。
        Docked { edge: OrbDockEdge, ratio: f64, work: [i32; 4] },
        /// 内容锚定：**可见内容**左上角钉在物理点 `origin`,`expanded` 决定偏移与尺寸。
        Anchored { origin: (i32, i32), expanded: bool },
    }

    struct Ctx {
        window: WebviewWindow,
        /// 拖动位移判定阈值（物理像素;见 DRAG_SHIFT_LOGICAL）。
        drag_threshold: i32,
        /// 移动循环的起点（见 `DragOrigin`）。
        drag_origin: Mutex<Option<DragOrigin>>,
        /// 最后一次归位意图（见 `Desired`）。
        desired: Mutex<Option<Desired>>,
        /// 移动循环进行中（WM_ENTERSIZEMOVE..EXITSIZEMOVE）——拖动期间不重放意图,
        /// 用户的手才是几何的主人（重放会把窗口从光标下拽走）。
        in_move_loop: AtomicBool,
        /// **指针让出态**：true = 窗口当前带 `WS_EX_TRANSPARENT`
        /// （整窗对鼠标透明,系统路由跳过本窗口）。由 `sync_pointer_pass` 单点翻转,
        /// 只在变化时做样式调用 / 事件 / 定时器 / 日志。
        pointer_pass: AtomicBool,
    }

    static CTX: OnceLock<Ctx> = OnceLock::new();

    pub fn install(window: &WebviewWindow) {
        let Ok(hwnd) = window.hwnd() else {
            crate::dev_log!("[orb-dock] install failed: no hwnd");
            return;
        };
        let scale = window_dpi_scale(window);
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
            desired: Mutex::new(None),
            in_move_loop: AtomicBool::new(false),
            pointer_pass: AtomicBool::new(false),
        };
        match CTX.set(ctx) {
            Ok(()) => {
                crate::dev_log!("[orb-dock] installed, drag_threshold = {drag_threshold}px");
                // 指针让出的常驻轮询：穿透后的恢复、以及「光标从窗口外进入画布」
                // 两条通路都收不到鼠标消息,全靠它扫描光标（35ms,成本可忽略）;
                // 显隐切换会重设（隐藏停表 / 显示起表,见 reset_pointer_pass）。
                unsafe { SetTimer(hwnd, PASS_TIMER_ID, PASS_POLL_MS, None) };
                // 基础样式（诊断用）：记下窗口出厂 STYLE / EXSTYLE——让出机制只在
                // EXSTYLE 上加 WS_EX_TRANSPARENT 位,样式闸只从 STYLE 上摘带框位,
                // 日志里两组值前后对照即可核对透明窗的组合与摘位是否生效。
                let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 };
                let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 };
                // 「闪出原生标题栏」的样式闸与 DWM 闸（㊾/㊿,见 FRAME_STYLE_BITS /
                // disable_nc_rendering;绘制消息闸在 subclass_proc 里常驻）
                let stripped = strip_frame_styles(hwnd);
                disable_nc_rendering(hwnd);
                crate::dev_log!(
                    "[orb-dock] style = 0x{style:08X} -> 0x{:08X} (frame styles stripped = {stripped}) exstyle = 0x{ex:08X}",
                    style & !FRAME_STYLE_BITS
                );
            }
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
        if msg == WM_NCPAINT || msg == WM_NCUAHDRAWCAPTION || msg == WM_NCUAHDRAWFRAME {
            // 非客户区声明为零（tao WM_NCCALCSIZE → 0）,就不许任何路径画它：
            // DefWindowProc / uxtheme 是按**样式**画标题栏的,不看非客户区面积
            // ——这正是让出翻转时「闪出原生标题栏」的绘制入口。
            return 0;
        } else if msg == WM_NCACTIVATE {
            // 激活态变化照常交给 tao（它据 wParam 维护焦点事件）,但 lParam 改成
            // -1：DefWindowProc 据此**只改状态、不重绘**非客户区。
            return DefSubclassProc(hwnd, msg, wparam, -1);
        } else if msg == WM_SETTINGCHANGE {
            // 文本大小等系统设置变更：刷新缓存系数;真变了就按新系数重放归位意图
            // （窗口尺寸 = CSS × DPI × 文本缩放要重写;让出区域由轮询对账自愈）。
            // 非文本缩放的设置变更也会走到,代价只是一次注册表读。
            if crate::text_scale::refresh() {
                schedule_replay(hwnd);
            }
        } else if msg == WM_ENTERSIZEMOVE {
            set_in_move_loop(true);
            remember_drag_origin();
        } else if msg == WM_EXITSIZEMOVE {
            set_in_move_loop(false);
            on_exit();
        } else if msg == WM_DPICHANGED {
            // 先让 tao 按它的语义重排（它要更新 scale 缓存并从 lParam 取新尺寸）,
            // 再**排定一次合并重放**（去抖,见 `schedule_replay`）——拖动期间排队的
            // 这条消息会在 WM_EXITSIZEMOVE 之后才到,不重放就等于放弃归位
            // （settle reposition （2494,749) -> got （1977,603)）。
            // 拖动中不重放（用户的手是几何的主人）。
            let r = DefSubclassProc(hwnd, msg, wparam, lparam);
            schedule_replay(hwnd);
            return r;
        } else if msg == WM_TIMER && wparam == REPLAY_TIMER_ID {
            // 合并重放到点：清掉定时器,按当前意图把矩形钉回目标值
            // （拖动中 `replay_desired` 自会跳过;此刻的几何以用户的手为准）。
            unsafe { KillTimer(hwnd, REPLAY_TIMER_ID) };
            replay_desired();
            return 0;
        } else if msg == WM_TIMER && wparam == PASS_TIMER_ID {
            // 指针让出的恢复轮询（穿透期间收不到鼠标消息,只能这样问光标在哪）。
            if let Some(ctx) = CTX.get() {
                let window = ctx.window.clone();
                sync_pointer_pass(&window);
            }
            return 0;
        } else if msg == WM_MOUSEMOVE {
            // 从主体滑入画布：命中测试不会重询（光标没离开本窗口矩形）,只有这条
            // 消息能捕获越界 → 主动让出。不拦截,继续转发。
            if let Some(ctx) = CTX.get() {
                let window = ctx.window.clone();
                sync_pointer_pass(&window);
            }
        } else if msg == WM_NCHITTEST {
            // 点击穿透：命中透明呼吸位 → HTTRANSPARENT（不进 tao 的默认 hit test;
            // 主体区/拖动中的判定交回默认处理）。顺带同步让出态——这条通路覆盖
            // 「光标从窗口外进入画布」,让出态与命中判定保持同一口径。
            if let Some(hit) = transparent_margin_hit(lparam) {
                if let Some(ctx) = CTX.get() {
                    let window = ctx.window.clone();
                    sync_pointer_pass(&window);
                }
                return hit;
            }
        }
        // 无条件转发子类化链（tao wndproc 行为不受影响;同 snap.rs 零迟滞约束）
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }

    /// 移动循环标记（拖动期间不重放归位意图,见 `Ctx.in_move_loop`）。
    fn set_in_move_loop(on: bool) {
        if let Some(ctx) = CTX.get() {
            ctx.in_move_loop.store(on, Ordering::SeqCst);
        }
    }

    fn in_move_loop() -> bool {
        CTX.get().is_some_and(|ctx| ctx.in_move_loop.load(Ordering::SeqCst))
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
            // 文本缩放乘进屏 scale（text_scale.rs）：WebView 内容按 DPI × 文本缩放渲染,
            // 本模块的「逻辑像素」= CSS 像素,物理换算必须带上它,否则主体/区域错位。
            out.push(Screen { rect, work, scale: m.scale_factor() * crate::text_scale::factor() });
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
    /// 展开态 = 表盘中心（235 + 55 = 580/2、100 + 55 = 310/2）——所以「窗口矩形
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
    /// ⚠ 只服务**已停靠一路**（自由态改看光标,见硬约束 2;跨缝补判）。
    fn edge_gap(content: (i32, i32, i32, i32), work: &WorkArea, edge: OrbDockEdge) -> i32 {
        let (l, r) = (content.0, content.0 + content.2);
        match edge {
            // 屏在左缘右侧：内容左缘仍在缘右 ⇒ 距离 = 左缘 − 缘;跨线/越过 ⇒ 0
            OrbDockEdge::Left => (l - work.left).max(0),
            // 屏在右缘左侧：内容右缘仍在缘左 ⇒ 距离 = 缘 − 右缘;跨线/越过 ⇒ 0
            OrbDockEdge::Right => (work.right - r).max(0),
        }
    }

    /// **已停靠**松手落点 → 贴边目标：在**内容所在屏**的两条竖缘里取最近的一条,
    /// 间距 ≤ tol 才算贴上（None = 自由落点）。判定输入只有「内容矩形 + 内容所在
    /// 屏」——此前贴在哪台屏哪条缘不是输入,沿边缘上下挪/换屏换边因此都只按本屏
    /// 重算（同一条接缝属于两台屏,归属由内容中心定;自由态的跨缝情形另由
    /// `seam_landing_target` 补判）。
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

    /// 光标屏幕坐标（物理像素,取整）——自由态贴边判定的起点（= 鼠标点击/拖动位置）。
    fn cursor_pos(window: &WebviewWindow) -> Option<(i32, i32)> {
        window.cursor_position().ok().map(|p| (p.x.round() as i32, p.y.round() as i32))
    }

    /// 自由态贴边判定：**光标**（= 鼠标点击/
    /// 拖动位置）到**内容所在屏**工作区左/右缘的距离 ≤ `EDGE_EPS_LOGICAL`
    /// （几像素）**或已越过该缘** ⇒ 判贴上该缘。
    /// - 「越过」也算触缘（往外推过头是「更贴」的意图）——所以「把鼠标推到边上」
    ///   这种手势一定成立,不受抓取点偏移影响;光标推过接缝后仍算越过本屏的缘。
    /// - ⚠ **参照屏恒为内容所在屏**（光标只是测量点,不参与选屏）。这是
    ///   二轮订定下的「判定与归位同屏」约束：曾把「光标所在屏」也当
    ///   候选来源,光标一过接缝就判成隔壁屏左缘,归位按隔壁 scale 换算 ⇒ 窗口在
    ///   自身 DPI 下 set_position/set_size 全部不生效（dev 日志
    ///   `settle reposition （2536,756) -> got （2069,639)`、`settle resize 112x232
    ///   -> got 870x465`）,竖条留在原地不贴边——「贴边弹出很远距离,
    ///   不是贴靠边沿」。**候选边缘只取本屏两缘,勿再加别的来源。**
    fn cursor_dock_target(screen: &Screen, cursor: (i32, i32)) -> Option<(i32, OrbDockEdge)> {
        let eps = (EDGE_EPS_LOGICAL * screen.scale).round() as i32;
        let mut best: Option<(i32, OrbDockEdge)> = None;
        for (edge, hit, dist) in [
            (OrbDockEdge::Left, cursor.0 <= screen.work.left + eps, cursor.0 - screen.work.left),
            (OrbDockEdge::Right, cursor.0 >= screen.work.right - eps, screen.work.right - cursor.0),
        ] {
            if !hit {
                continue;
            }
            let dist = dist.abs();
            if best.map_or(true, |(d, _)| dist < d) {
                best = Some((dist, edge));
            }
        }
        best
    }

    /// 自由态**视觉主体**的水平跨度（物理像素,左缘..右缘）——过半判定与命中区
    /// 都要用的「表盘本体」而不是内容盒：展开态内容盒 = 表盘 110 + 间隙 6 +
    /// 按钮列 16,右缘因此比表盘本体多 22 逻辑像素（拿内容盒判会提前一个按钮列
    /// 的距离触发）;收起态内容盒就是竖条本体,原样返回。
    fn visual_span(expanded: bool, content: (i32, i32, i32, i32), scale: f64) -> (i32, i32) {
        let w = if expanded {
            (DIAL_RADIUS_LOGICAL * 2.0 * scale).round() as i32
        } else {
            content.2
        };
        (content.0, content.0 + w)
    }

    /// 自由态**跨缝补判**：拖动中窗口中心换过屏
    /// （= 系统已按「窗口过半」把窗口判给新屏、DPI 也跟着切）时,**光标必然已经
    /// 随拖动深入新屏**,离缝远超几像素的 eps ⇒ 光标口径整条失效。症状：
    /// 从接缝左侧拖到右侧,表盘停在缝上,既不贴边也不弹开
    /// （`release … cursor=（2629,982) … target=None`）;反向却因「越过也算」的外侧
    /// 无界而照常触发——于是同一个手势只有一半方向成立。
    ///
    /// 判据 = **主体是否完整显示**（口径:与「拖入边缘时主体被截断才触发
    /// 贴边」是同一条逻辑的镜像）：主体跨过新屏近缘之后,只要它离新屏近缘仍
    /// ≤ 几像素（含还压着缝、尚未整体进入）就算贴住那条近缘;离缝超过几像素 ⇒ 主体
    /// 已经**完整显示出来**（不再被屏缘截断）,就不必再贴边,该自由摆放。
    ///
    /// ⚠ 只在**本次拖动换过屏**（`crossed`）时生效：单屏内拖动仍要求光标真推到边
    /// （否则又回到用户否决过的「圆盘还没接触屏幕就收起」）。
    fn seam_landing_target(
        screen: &Screen,
        span: (i32, i32),
        crossed: bool,
    ) -> Option<(i32, OrbDockEdge)> {
        if !crossed {
            return None;
        }
        let eps = (EDGE_EPS_LOGICAL * screen.scale).round() as i32;
        let mut best: Option<(i32, OrbDockEdge)> = None;
        for (edge, gap) in [
            (OrbDockEdge::Left, (span.0 - screen.work.left).max(0)),
            (OrbDockEdge::Right, (screen.work.right - span.1).max(0)),
        ] {
            if gap <= eps && best.map_or(true, |(g, _)| gap < g) {
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

    /// 拖动松手：只跑一次落点判定（自由态 `cursor_dock_target`、已停靠 `dock_target`）,
    /// 结果分三种走向——
    /// - 贴到内容所在屏的某条竖缘 ⇒ **dock**：写停靠状态（边 + 锚点 + 那台屏的
    ///   工作区）并按该屏归位;此前是自由态（或形态失配）就广播,前端切竖条;
    ///   此前已停靠（沿边缘上下挪 / 换屏换边）则只归位,不广播（形态没变）;
    /// - 未贴边且此前已停靠 ⇒ **undock**：清状态 + 广播,展开归位交 `orb_undock`
    ///   （前端发起,尺寸与位置原子完成）;
    /// - 未贴边且自由态 ⇒ **钳回**内容所在屏工作区完整显示（不裁切、不压任务栏）。
    ///
    /// 参照屏永远是**内容当前所在屏**（不是历史停靠屏,也不是光标所在屏——光标只是
    /// 自由态判定的测量点）——判定与归位同屏,跨屏拖动后不会再出现「按旧屏归位」的
    /// 贴边距离漂移或侵入接缝;拿别的屏归位时窗口的 set_position/set_size 会在自身
    /// DPI 下失效（竖条留在原地不贴边）。
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
        let drag_origin = take_drag_origin();
        if let Some(o) = drag_origin {
            if (pos.0 - o.pos.0).abs() >= ctx.drag_threshold
                || (pos.1 - o.pos.1).abs() >= ctx.drag_threshold
            {
                use tauri::Emitter;
                crate::dev_log!(
                    "[orb-dock] dragged ({},{}) -> ({},{})",
                    o.pos.0,
                    o.pos.1,
                    pos.0,
                    pos.1
                );
                let _ = app.emit("orb-dragged", true);
            }
        }

        let expanded = super::expanded_state(&app);
        let content = content_rect(pos, screen.scale, expanded);
        let was_docked = super::dock_state_for(&app).is_some();
        // 判定对象分档：
        // - 自由态**表盘** = **光标**触缘（鼠标推到屏边才收;用内容矩形判会提前
        //   二三十像素——展开态内容右侧挂着 6px 间隙 + 16px 按钮列）;
        // - 自由态**竖条** = 可见内容矩形 ≤ 16（与已停靠同尺）——竖条的内容就是
        //   本体,不存在「提前触发」问题：手动折叠的竖条
        //   拖到离缝 7px 松手不吸附（光标在竖条中部、离缘 19px 不触缘;屏内拖动又
        //   不满足跨缝补判的前置 crossed）⇒ 被钳到离缘 24px 处,不是「重定位吸附」
        //   的固定距离（dev 日志 release content=[2517,843,36,126] cursor=（2541,926)
        //   was_docked=false crossed=false target=None）;
        // - 已停靠 = 可见内容矩形 ≤ 16（滞回：贴稳后要拖离约 12px 才脱边）。
        let cursor = cursor_pos(&ctx.window);
        let keep_tol = (DOCK_KEEP_TOLERANCE_LOGICAL * screen.scale).round() as i32;
        // 本次拖动是否跨过接缝（起点屏 ≠ 出口屏）——只喂给自由态表盘的跨缝补判。
        let crossed_seam = drag_origin.is_some_and(|o| {
            o.work != [screen.work.left, screen.work.top, screen.work.right, screen.work.bottom]
        });
        let target = if was_docked || !expanded {
            // 已停靠竖条（保持/拖离/换屏换边）与自由态竖条（手动折叠后拖到缘边）
            // 同尺：本体离缘 ≤ keep_tol ⇒ 贴上（越过/压着缘一律算 0,见 edge_gap）。
            dock_target(&screen, content, keep_tol)
        } else {
            //  光标触缘;光标取不到（几乎不可能）⇒ 不触发、按未贴边钳回——不拿
            // 内容矩形兜底,否则又回到「圆盘还没到边就收起」的旧口径。
            //  跨缝补判：光标随拖动深入新屏后必然失效,
            // 用「主体完整显示」补上,手势因此双向一致。
            cursor
                .and_then(|c| cursor_dock_target(&screen, c))
                .or_else(|| {
                    seam_landing_target(
                        &screen,
                        visual_span(expanded, content, screen.scale),
                        crossed_seam,
                    )
                })
        };
        crate::dev_log!(
            "[orb-dock] release content=[{},{},{},{}] cursor={cursor:?} keep_tol={keep_tol} scale={} was_docked={was_docked} expanded={expanded} crossed={crossed_seam} target={target:?}",
            content.0,
            content.1,
            content.2,
            content.3,
            screen.scale
        );

        match target {
            Some((_, edge)) => {
                // 落点屏 = **内容所在屏**（判定与归位同屏;勿改成光标所在屏——
                // 跨屏归位会按隔壁 scale 算,窗口尺寸/位置都改不动）
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
                crate::dev_log!("[orb-dock] undock (no edge within {keep_tol} on screen {si})");
                super::set_dock_state(&app, None);
                use tauri::Emitter;
                let _ = app.emit("orb-dock-changed", OrbDockChanged { docked: false, edge: None });
            }
            None => {
                // 自由态没贴到缘：内容钳回所在显示器工作区完整显示（不裁切、
                // 不压任务栏）。越界量小时视觉上就是「被轻轻推回」。
                clamp_into_work(&ctx.window, &screen, content, expanded);
            }
        }
    }

    /// dock 几何归位：竖条贴缘（**本体**距缘 4px;窗口透明边距出屏）,锚 Y 按比例
    /// 回放并钳在工作区内。矩形由 `docked_rect` 按**目标屏自己的 scale** 算
    /// （窗口此刻还在另一台 DPI 不同的屏上,跨屏写入由 `apply_desired` 统一处理）;
    /// 这里只写形态状态 + 记下意图,不直接碰窗口。
    fn place_docked(window: &WebviewWindow, edge: OrbDockEdge, ratio: f64, screen: &Screen) {
        let work = [screen.work.left, screen.work.top, screen.work.right, screen.work.bottom];
        crate::dev_log!(
            "[orb-dock] place_docked {edge:?} ratio={ratio:.3} scale={} work={work:?}",
            screen.scale
        );
        apply_desired(window, Desired::Docked { edge, ratio, work }, true);
        // 形态落盘放在几何之后（persist 会读窗口几何——顺序反了会把旧矩形写进文件）
        super::set_expanded_state(window.app_handle(), false);
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

        // 展开尺寸以**常量**为准（前端传入值只做漂移告警：两处必须由
        // scripts/check-orb-geometry.mjs 保持同源,这里不设兜底特例）
        let (ew, eh) = form_size_logical(true);
        for (got, want, axis) in [(expand_w, ew, "width"), (expand_h, eh, "height")] {
            if let Some(v) = got.filter(|v| *v > 1.0) {
                if (v - want).abs() > 0.5 {
                    crate::dev_log!(
                        "[orb-dock] undock_ready {axis} drift: frontend {v} vs const {want}（以常量为准）"
                    );
                }
            }
        }

        // 内容（含阴影余量）钳进工作区 → 得到最终内容原点;窗口位置由 anchored_rect 反推
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
        let origin = (rx + m, ry + m);

        crate::dev_log!(
            "[orb-dock] undock_ready content_origin=({ox},{oy}) -> clamped ({},{}) scale={scale} size={ew}x{eh} logical",
            origin.0,
            origin.1
        );
        apply_desired(window, Desired::Anchored { origin, expanded: true }, true);
        // 形态落盘放在几何之后（persist 会读窗口几何——顺序反了会把旧矩形写进文件）
        super::set_expanded_state(&app, true);
    }

    /// 尺寸切换 + 内容锚定（手动折叠 / 自由态展开走这里）：换尺寸后把窗口位置
    /// 补齐,保证**内容原点在屏上不动**,并同步 Rust 侧形态状态。
    pub fn set_size_anchored(window: &WebviewWindow, width: f64, height: f64) {
        let Ok(pos) = window.outer_position() else {
            forget_desired(); // 几何读不到 ⇒ 没有可重放的目标,别留旧意图
            set_size_by_current_dpi(window, width, height);
            return;
        };
        let Ok(size) = window.outer_size() else {
            forget_desired();
            set_size_by_current_dpi(window, width, height);
            return;
        };
        let screens = screens(window);
        if screens.is_empty() {
            forget_desired();
            set_size_by_current_dpi(window, width, height);
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
        let (w_log, h_log) = form_size_logical(new_expanded);
        if (width - w_log).abs() > 0.5 || (height - h_log).abs() > 0.5 {
            crate::dev_log!(
                "[orb-dock] set_orb_size drift: {width}x{height} vs form {w_log}x{h_log}（以常量为准）"
            );
        }
        apply_desired(window, Desired::Anchored { origin: (ox, oy), expanded: new_expanded }, true);
        // 形态落盘放在几何之后（persist 会读窗口几何——顺序反了会把旧矩形写进文件）
        super::set_expanded_state(&app, new_expanded);
    }

    /// 窗口当前 DPI 缩放（`GetDpiForWindow` ÷ 96;取不到时退 1.0）。
    /// **本模块只从这里取窗口 DPI**——tao 的 `scale_factor` 是缓存、随 WM_DPICHANGED
    /// 更新,而那条消息在跨屏拖动时往往还排在队列里 ⇒ 读它就会拿到旧值。这与硬约束 1
    /// 是同一条理由：几何只认「显式显示器枚举 + 现取的 DPI」。
    fn window_dpi_scale(window: &WebviewWindow) -> f64 {
        let Ok(hwnd) = window.hwnd() else { return 1.0 };
        let dpi = unsafe { GetDpiForWindow(hwnd.0 as HWND) };
        // 同 `screens`：乘上文本缩放（CSS 像素 = 逻辑像素 × 文本缩放）。
        let text = crate::text_scale::factor();
        if dpi == 0 {
            text
        } else {
            f64::from(dpi) / 96.0 * text
        }
    }

    /// 逻辑尺寸 → **物理写入**（换算用**现取** DPI,不经过 tao 缓存）——兜底路径专用
    /// （拿不到屏模型/窗口几何的极端分支）。与 `write_rect` 同一条理由:逻辑写入会被
    /// 滞后的缓存按旧屏尺换算,物理写入不会（见 `write_rect` 注释）。
    fn set_size_by_current_dpi(window: &WebviewWindow, w_log: f64, h_log: f64) {
        let scale = window_dpi_scale(window);
        let _ = window.set_size(PhysicalSize::new(
            (w_log * scale).round().max(1.0) as i32,
            (h_log * scale).round().max(1.0) as i32,
        ));
    }

    /// 形态的窗口**逻辑**尺寸（与前端 `COLLAPSED_SIZE` / `EXPANDED_SIZE` 同源,
    /// 由 `scripts/check-orb-geometry.mjs` 机械校验）。
    fn form_size_logical(expanded: bool) -> (f64, f64) {
        if expanded {
            (EXPANDED_W_LOGICAL, EXPANDED_H_LOGICAL)
        } else {
            (PILL_W_LOGICAL, PILL_H_LOGICAL)
        }
    }

    /// 贴边矩形（纯函数,单测覆盖）：竖条贴 `work` 那台屏的 `edge` 缘（**本体**距缘
    /// `PILL_EDGE_GAP`,窗口透明边距允许出屏——屏外无像素）,中心 Y 按 `ratio` 落并
    /// 钳在工作区内。返回 （x, y, 逻辑宽, 逻辑高)。
    fn docked_rect(
        edge: OrbDockEdge,
        ratio: f64,
        scale: f64,
        work: &WorkArea,
    ) -> (i32, i32, f64, f64) {
        let w = (PILL_W_LOGICAL * scale).round() as i32;
        let h = (PILL_H_LOGICAL * scale).round() as i32;
        // 竖条本体距工作区缘 4px ⇒ 窗口缘 = 缘 ∓ （边距 − 间隙),透明边距出屏
        let shift = ((PILL_INSET_LOGICAL - PILL_EDGE_GAP_LOGICAL) * scale).round() as i32;
        let x = match edge {
            OrbDockEdge::Left => work.left - shift,
            OrbDockEdge::Right => work.right - w + shift,
        };
        let center_y = work.top as f64 + ratio * (work.bottom - work.top) as f64;
        let y = (center_y.round() as i32 - h / 2).clamp(work.top, (work.bottom - h).max(work.top));
        (x, y, PILL_W_LOGICAL, PILL_H_LOGICAL)
    }

    /// 内容锚定矩形（纯函数,单测覆盖）：**可见内容**左上角钉在物理点 `origin`,
    /// `expanded` 决定它在窗口内的偏移与窗口尺寸 ⇒ 内容在屏上不动。手动折叠/展开、
    /// undock 展开、自由态钳回三条路径都归到这一条语义（无特例分支）。
    fn anchored_rect(origin: (i32, i32), expanded: bool, scale: f64) -> (i32, i32, f64, f64) {
        let (pad_l, pad_t) = content_pad(expanded);
        let (w_log, h_log) = form_size_logical(expanded);
        (
            origin.0 - (pad_l * scale).round() as i32,
            origin.1 - (pad_t * scale).round() as i32,
            w_log,
            h_log,
        )
    }

    /// 意图 → **物理矩形**。归位屏一律从「内容当前
    /// 所在屏」起算;贴边意图再按记录的工作区找回目标屏（显示器拔插/重排后自适应,找不到
    /// 退回当前屏）。逻辑 → 物理的换算取**目标屏自己的 scale**（贴边 = work 匹配到的
    /// 那台屏;锚定 = 内容所在屏）,不用 tao 缓存——与 `write_rect` 写物理值配套,
    /// tao 缓存滞后期间跨屏归位也写得对（详见 `write_rect`）。
    fn desired_rect(desired: Desired, screen: &Screen, all: &[Screen]) -> (i32, i32, i32, i32) {
        let (x, y, w_log, h_log, scale) = match desired {
            Desired::Docked { edge, ratio, work } => {
                let sc = all.iter().find(|s| same_work(&s.work, &work)).unwrap_or(screen);
                let (x, y, w, h) = docked_rect(edge, ratio, sc.scale, &sc.work);
                (x, y, w, h, sc.scale)
            }
            Desired::Anchored { origin, expanded } => {
                let (x, y, w, h) = anchored_rect(origin, expanded, screen.scale);
                (x, y, w, h, screen.scale)
            }
        };
        (
            x,
            y,
            (w_log * scale).round() as i32,
            (h_log * scale).round() as i32,
        )
    }

    /// **几何写入口（全模块唯一）**：记下意图（供 DPI 重排后重放）→ 按内容所在屏算矩形
    /// → 统一写入。place_docked / undock_ready / set_size_anchored / 自由态钳回都收敛到
    /// 这里,「跨屏归位」因此不再各写各的。
    /// `force` = 本次是**用户动作**（true：总是精确写,见 `write_rect`）还是 DPI 重排后的
    /// 重放（false：已达成即跳过,防风暴）。
    fn apply_desired(window: &WebviewWindow, desired: Desired, force: bool) {
        if let Some(ctx) = CTX.get() {
            *ctx.desired.lock().unwrap() = Some(desired);
        }
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
        let (x, y, w_phys, h_phys) = desired_rect(desired, &screens[si], &screens);
        write_rect(window, x, y, w_phys, h_phys, force);
    }

    /// DPI 变更后重放归位意图（WM_DPICHANGED;返回是否重放了）。
    /// 拖动期间排队的这条消息在 WM_EXITSIZEMOVE **之后**才到,tao 会按「保持逻辑尺寸」
    /// 把窗口的尺寸与位置一起重排 ⇒ 不重放就等于放弃归位（「竖条留在原地不贴边」的真因）。
    /// 移动循环进行中不重放：那段时间用户的手才是几何的主人。
    fn replay_desired() -> bool {
        let Some(ctx) = CTX.get() else { return false };
        if in_move_loop() {
            return false;
        }
        let Some(desired) = *ctx.desired.lock().unwrap() else {
            return false;
        };
        crate::dev_log!("[orb-dock] replay on DPI change: {desired:?}");
        apply_desired(&ctx.window, desired, false);
        true
    }

    /// 排定一次**合并重放**（去抖）：对同一窗口重复 `SetTimer` 会重置计时,于是
    /// 密集的 WM_DPICHANGED（窗口骑在接缝上时,系统会在两台显示器的归属之间摇摆,
    ///  `dev-20260913.log` 19:15/19:27 两段各 15-30 条）只换来静默
    /// `REPLAY_DEBOUNCE_MS` 之后的**一次**写入——「写 → 系统重估 → 再发消息 →
    /// 再写」的自激回路因此被时间隔断（㊺;单发消息只是晚 40ms
    /// 重放,用户无感）。
    fn schedule_replay(hwnd: HWND) {
        unsafe { SetTimer(hwnd, REPLAY_TIMER_ID, REPLAY_DEBOUNCE_MS, None) };
    }

    /// 清掉归位意图（几何读不到、给不出可重放的目标时用——宁可不重放,也不能让
    /// 旧意图在 DPI 变更后把窗口拽回原处）。
    fn forget_desired() {
        if let Some(ctx) = CTX.get() {
            *ctx.desired.lock().unwrap() = None;
        }
    }

    /// **原子写窗口矩形**（一次 `SetWindowPos` 同时设置位置与尺寸,物理像素）——
    /// 写入路径唯一的落盘原语（为什么必须原子,见 `write_rect` 的说明）。
    /// `SWP_NOZORDER` 保持既有 Z 序（悬浮球是置顶窗）,`SWP_NOACTIVATE` 不抢焦点
    /// （拖动/点击穿透依赖窗口不因写入而激活）。
    fn apply_rect_atomic(window: &WebviewWindow, x: i32, y: i32, w_phys: i32, h_phys: i32) -> bool {
        let Ok(hwnd) = window.hwnd() else { return false };
        unsafe {
            SetWindowPos(
                hwnd.0 as HWND,
                std::ptr::null_mut(),
                x,
                y,
                w_phys,
                h_phys,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            ) != 0
        }
    }

    /// 统一写入出口：**位置与尺寸都写物理值**,
    /// **原子写入**（一次 `SetWindowPos` 同时设置位置与尺寸）+ 读回校验。
    ///
    /// 尺寸为什么写物理值（推翻㊴ 的「写逻辑值」）：`set_size（LogicalSize)` 的换算在
    /// tao 内部用 `window_state.scale_factor` —— **消息驱动的缓存**。跨屏拖动松手时窗口
    /// 已归新屏（`GetDpiForWindow` 已报新值）,但 WM_DPICHANGED 还排在队列里 ⇒ 缓存仍是
    /// 旧屏 scale,56×116 逻辑被写成 112×232 物理（`write rect （2494,1070) 56x116
    /// logical @1.5 （expect 84x174 phys)` 紧跟 `rect resize 84x174 -> got 112x232`）。
    /// 窗口因此比目标宽 28px、内容压在接缝上,且「写错 ⇒ 尺寸越缝 ⇒ 系统重估 DPI ⇒
    /// 再重排 ⇒ 再写错」自激成几百毫秒的消息风暴（贴片嵌入接缝 + 表盘闪烁）。
    /// 物理值不经缓存换算,写的就是目标矩形;系统随后的 DPI 重排由 `replay_desired`
    /// 按同一物理值再钉一次。
    ///
    /// 为什么必须**原子**：
    /// 分步写入必然产生中间态——「展开态大窗口先被放到贴边窄位」「小窗口先跳到接缝」
    /// 「目标尺寸先落在旧位置」——**每个中间态都是一次独立的窗口矩形变更**,系统会按它
    /// 重估窗口的显示器归属与 DPI,在接缝附近就演变成 WM_DPICHANGED 连发 + tao 重排 +
    /// 我们重放的拉锯（同一接缝两侧,先缩后移闪一侧、改先移后缩又闪另一侧
    /// ——顺序启发式按不住,根因在此）。原子写入把一次归位压缩成**一次**窗口矩形变更:
    /// 系统只评估最终矩形（跨缝 ≤ 21%,归属稳定）,没有中间态可误判。
    ///
    /// **幂等早退分档**：
    /// - `force = true`（用户动作：贴边归位 / 展开归位 / 尺寸锚定 / 钳回）——**总是精确写**。
    ///   吸附位是「本体距缘固定 4 逻辑」的确定值,±`SETTLE_TOL_PHYS` 的容差会把「松手处
    ///   离目标 ≤4px」的情形整段放过,于是吸附**看起来没发生**、离缘距离随拖动量漂移。
    ///   原话：「如果放下的距离接缝像素小于阈值,则不会触发重定位吸附,而是直接
    ///   停在原位,距离接缝几个像素」——「离得越近越像没吸附」正是早退的容差效应
    ///   （dev 日志 `target=Some（（4,Left))` / `Some（（3,Right))` 紧跟 `place_docked`
    ///   之后没有 write）。
    /// - `force = false`（DPI 重排后的 `replay_desired`）——已达成即静默,防「重放 →
    ///   重排 → 再重放」自激风暴;系统重排改出的尺寸（如 63×131）差 > 容差,仍会写。
    fn write_rect(window: &WebviewWindow, x: i32, y: i32, w_phys: i32, h_phys: i32, force: bool) {
        let (w_phys, h_phys) = (w_phys.max(1), h_phys.max(1));
        if !force {
            if let (Ok(p), Ok(sz)) = (window.outer_position(), window.outer_size()) {
                if (p.x - x).abs() <= SETTLE_TOL_PHYS
                    && (p.y - y).abs() <= SETTLE_TOL_PHYS
                    && (sz.width as i32 - w_phys).abs() <= SETTLE_TOL_PHYS
                    && (sz.height as i32 - h_phys).abs() <= SETTLE_TOL_PHYS
                {
                    return; // 已达成:静默（幂等,防「重放 → 重排 → 再重放」自激）
                }
            }
        }
        // 一次到位（原子）:没有「大窗口停在贴边窄位」「小窗口先跳接缝」这类中间态,
        // 系统只会为**最终矩形**做一次归属评估。
        if !apply_rect_atomic(window, x, y, w_phys, h_phys) {
            // SetWindowPos 失败（理论上仅句柄异常）⇒ 退回 tao 两条调用,
            // 至少保证一只脚落地;下面的读回校验会再纠一次。
            let _ = window.set_position(PhysicalPosition::new(x, y));
            let _ = window.set_size(PhysicalSize::new(w_phys, h_phys));
        }
        crate::dev_log!("[orb-dock] write rect ({x},{y}) {w_phys}x{h_phys} phys");
        // 读回校验容差:用户动作（force）收紧到 2——吸附距离对用户是可感知的（本体
        // 距缘固定 4 逻辑）,系统级 1px 取整放过、更大偏移再钉一次;重放（非 force）
        // 保持 SETTLE_TOL_PHYS,避免与系统重排互相追打。
        let tol = if force { 2 } else { SETTLE_TOL_PHYS };
        if let Ok(p) = window.outer_position() {
            if (p.x - x).abs() > tol || (p.y - y).abs() > tol {
                crate::dev_log!(
                    "[orb-dock] rect reposition ({x},{y}) -> got ({},{})",
                    p.x,
                    p.y
                );
                apply_rect_atomic(window, x, y, w_phys, h_phys);
            }
        }
        if let Ok(sz) = window.outer_size() {
            if (sz.width as i32 - w_phys).abs() > tol || (sz.height as i32 - h_phys).abs() > tol {
                crate::dev_log!(
                    "[orb-dock] rect resize {w_phys}x{h_phys} -> got {}x{}",
                    sz.width,
                    sz.height
                );
                apply_rect_atomic(window, x, y, w_phys, h_phys);
            }
        }
        // 几何变了 → 「光标相对主体」的关系也变了（归位可能把主体送到光标下,
        // 或从光标下挪走）——重设让出区域（尺寸变化可能让系统重置/错位它）,
        // 再重估一次让出状态（拖动中内部会早退;幂等,静默）。
        refresh_pass_region(window);
        sync_pointer_pass(window);
    }

    /// 自由松手/恢复的边界钳制：**内容矩形 + 阴影余量**钳回所在显示器工作区
    /// （物理像素）。窗口比工作区还大（极端小屏）时钳到左上角。返回是否挪位。
    /// 钳完仍走 `apply_desired`（内容锚定语义）——**没越界也要记意图**,DPI 变更后的
    /// 重放才钉得住「用户松手的这一处」。
    fn clamp_into_work(
        window: &WebviewWindow,
        screen: &Screen,
        content: (i32, i32, i32, i32),
        expanded: bool,
    ) -> bool {
        let m = (CONTENT_MARGIN_LOGICAL * screen.scale).round() as i32;
        let (cx, cy, cw, ch) = (content.0 - m, content.1 - m, content.2 + 2 * m, content.3 + 2 * m);
        let max_x = (screen.work.right - cw).max(screen.work.left);
        let max_y = (screen.work.bottom - ch).max(screen.work.top);
        let tx = cx.clamp(screen.work.left, max_x);
        let ty = cy.clamp(screen.work.top, max_y);
        let (dx, dy) = (tx - cx, ty - cy);
        if dx != 0 || dy != 0 {
            crate::dev_log!(
                "[orb-dock] clamp content ({},{}) -> ({},{})",
                content.0,
                content.1,
                content.0 + dx,
                content.1 + dy
            );
        }
        apply_desired(
            window,
            Desired::Anchored { origin: (content.0 + dx, content.1 + dy), expanded },
            true,
        );
        dx != 0 || dy != 0
    }

    /// 启动形态决策（纯函数,单测覆盖;「启动恢复上次退出前的
    /// 形态与位置,首次默认表盘」;订：竖条只属于贴边,未贴边一律表盘）：
    /// **记录窗口当时的形态**（只用于从记录窗口位置换算内容原点,不决定启动形态）——
    /// 显式字段（`orb_expanded`）优先,旧格式（无该字段）按落盘尺寸反推。
    fn recorded_form(recorded_expanded: Option<bool>, recorded_size: (u32, u32), scale: f64) -> bool {
        match recorded_expanded {
            Some(e) => e,
            None => looks_expanded(recorded_size.0 as f64 / scale, recorded_size.1 as f64 / scale),
        }
    }

    /// 未贴边启动的表盘内容矩形（物理像素）：内容原点 = 记录窗口位置 + **记录形态**的
    /// 内容偏移（与手动展开的内容锚定同一口径——自由竖条原地长成表盘,不按表盘偏移
    /// 硬套竖条窗口位置,否则表盘会向左上窜出 219/84 逻辑像素）。
    fn startup_dial_content(pos: (i32, i32), scale: f64, recorded_expanded: bool) -> (i32, i32, i32, i32) {
        let (rec_l, rec_t) = content_pad(recorded_expanded);
        let (exp_l, exp_t) = content_pad(true);
        let dial_pos = (
            pos.0 + (rec_l * scale).round() as i32 - (exp_l * scale).round() as i32,
            pos.1 + (rec_t * scale).round() as i32 - (exp_t * scale).round() as i32,
        );
        content_rect(dial_pos, scale, true)
    }

    /// 启动恢复（`window_state:restore` 调用;无子类化上下文也安全）：按**上次退出
    /// 形态**恢复尺寸与位置,返回最终形态（写进 AppState.orb_expanded）。
    ///
    /// 三种情形：
    /// - **贴边停靠**（`dock` 有值）⇒ 按 anchor 重算贴边位置（出屏补偿是 place_docked
    ///   的专属语义,通用 clamp 会把窗口拉回屏内、贴边观感丢失）;目标屏优先按记录的
    ///   工作区匹配当前显示器（显示器拔插/DPI 变更后自适应）;
    /// - **未贴边**（有几何记录）⇒ **一律表盘**（竖条形态只属于
    ///   贴边;上次退出是自由竖条也回表盘,在竖条原位长大）,内容 + 阴影余量钳进工作区
    ///   后精确写入;
    /// - **首次启动**（无记录）⇒ **展开态（表盘）** + 居中。
    pub fn restore_orb(
        window: &WebviewWindow,
        recorded: Option<(i32, i32, u32, u32)>,
        recorded_expanded: Option<bool>,
        dock: Option<OrbDockState>,
    ) -> bool {
        let screens = screens(window);
        // 记录位置所在屏的 scale（窗口此刻还在默认位置,不能按窗口当前位置选屏）
        let scale_of = |x: i32, y: i32| -> f64 {
            screens
                .get(screen_index_at(&screens, x, y))
                .map_or_else(|| window_dpi_scale(window), |s| s.scale)
        };

        //  贴边停靠态：形态必为收起,位置由 anchor 重算（不用记录窗口位置）
        if let Some(dock) = dock {
            if screens.is_empty() {
                return false;
            }
            let idx = screens
                .iter()
                .position(|s| same_work(&s.work, &dock.work))
                .or_else(|| recorded.map(|(x, y, _, _)| screen_index_at(&screens, x, y)))
                .unwrap_or(0);
            crate::dev_log!(
                "[orb-dock] restore_orb docked {:?} ratio={:.3} screen={idx}",
                dock.edge,
                dock.anchor_y_ratio
            );
            place_docked(window, dock.edge, dock.anchor_y_ratio, &screens[idx]);
            return false;
        }

        //  未贴边 / 首次：一律表盘（竖条只属于贴边——自由竖条不作为启动形态）
        let scale = recorded.map_or_else(|| window_dpi_scale(window), |(x, y, _, _)| scale_of(x, y));
        let expanded = true;
        crate::dev_log!(
            "[orb-dock] restore_orb free expanded={expanded} recorded={recorded:?} recorded_expanded={recorded_expanded:?} scale={scale}"
        );
        // 只写内存不落盘（几何归位途中,落盘会记中间态——见 store_expanded_state）
        super::store_expanded_state(window.app_handle(), expanded);

        match recorded {
            Some((x, y, w, h)) => {
                // 内容原点 = 记录窗口位置 + 记录形态的内容偏移（记录是表盘 ⇒ 原位;记录是
                // 自由竖条 ⇒ 在竖条原位长成表盘）;钳进工作区后精确写入（clamp_into_work
                // 内含 apply_desired（force) 与意图记账,DPI 变更后可重放）。
                if screens.is_empty() {
                    return expanded;
                }
                let screen = screens[screen_index_at(&screens, x, y)];
                let was_expanded = recorded_form(recorded_expanded, (w, h), screen.scale);
                let content = startup_dial_content((x, y), screen.scale, was_expanded);
                clamp_into_work(window, &screen, content, expanded);
            }
            None => {
                // 首次启动：**展开态（表盘）** + 主屏工作区居中。
                // 窗口尚未显示,tauri center 的显示器判定不可靠——用显式显示器模型
                // 自己算（主屏 = 覆盖原点的显示器;工作区居中避开任务栏）。
                let (w_log, h_log) = form_size_logical(expanded);
                let (pad_l, pad_t) = content_pad(expanded);
                let main = screens.iter().find(|s| {
                    s.work.left <= 0 && s.work.top <= 0 && s.work.right > 0 && s.work.bottom > 0
                });
                let Some(main) = main else {
                    // 显示器枚举异常（极罕）：退回窗口 DPI + tauri 居中
                    let s = window_dpi_scale(window);
                    let _ = window.set_size(PhysicalSize::new(
                        (w_log * s).round() as i32,
                        (h_log * s).round() as i32,
                    ));
                    let _ = window.center();
                    if let Ok(p) = window.outer_position() {
                        let origin = (
                            p.x + (pad_l * s).round() as i32,
                            p.y + (pad_t * s).round() as i32,
                        );
                        apply_desired(window, Desired::Anchored { origin, expanded }, true);
                    }
                    return expanded;
                };
                let w_phys = (w_log * main.scale).round() as i32;
                let h_phys = (h_log * main.scale).round() as i32;
                let x = main.work.left + ((main.work.right - main.work.left) - w_phys) / 2;
                let y = main.work.top + ((main.work.bottom - main.work.top) - h_phys) / 2;
                // 先落位再记账（apply_desired 按窗口当前位置选屏,落位后才能选对主屏）
                let _ = window.set_position(PhysicalPosition::new(x, y));
                let origin = (
                    x + (pad_l * main.scale).round() as i32,
                    y + (pad_t * main.scale).round() as i32,
                );
                apply_desired(window, Desired::Anchored { origin, expanded }, true);
            }
        }
        expanded
    }

    /// 交互主体矩形（物理像素）——「点击穿透命中」与「指针让出」共用的唯一口径：
    /// 形态取权威状态,偏移/尺寸取逻辑常量 × 该屏 scale（现取 DPI,不用 tao 缓存）。
    fn body_rect(window: &WebviewWindow) -> Option<(i32, i32, i32, i32)> {
        let pos = window.outer_position().ok()?;
        let scale = window_dpi_scale(window);
        let expanded = super::expanded_state(window.app_handle());
        Some(content_rect((pos.x, pos.y), scale, expanded))
    }

    /// 点击穿透命中判定：光标在两态各自的「交互主体」矩形内 → None（走默认处理）;
    /// 落在透明呼吸位 → Some（HTTRANSPARENT)。
    /// lParam 低/高 16 位 = 光标屏幕坐标（各按 i16 符号扩展——多显示器负坐标在
    /// i16 范围内;>32767 的超大拼接桌面会溢出,已知边界不覆盖）。
    fn transparent_margin_hit(lparam: LPARAM) -> Option<LRESULT> {
        let ctx = CTX.get()?;
        let x = (lparam & 0xFFFF) as u16 as i16 as i32;
        let y = ((lparam >> 16) & 0xFFFF) as u16 as i16 as i32;
        let (left, top, w, h) = body_rect(&ctx.window)?;
        let inside = x >= left && x < left + w && y >= top && y < top + h;
        if inside {
            None
        } else {
            Some(HTTRANSPARENT)
        }
    }

    // ---------- 指针让出 ----------

    /// 现取光标屏幕坐标（物理像素）——穿透期间窗口收不到鼠标消息,只有这个 API
    /// 能问出光标在哪（恢复轮询的输入）。
    fn cursor_screen_pos() -> Option<(i32, i32)> {
        let mut p = POINT { x: 0, y: 0 };
        (unsafe { GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
    }

    /// 光标是否在主体矩形外（buf = 滞回缓冲,物理像素;0 即严格在矩形内）。
    /// 纯函数：让出判定带缓冲 / 恢复判定不带,两档之间是「维持现状」带,防边界抖动。
    fn pointer_outside(cursor: (i32, i32), body: (i32, i32, i32, i32), buf: i32) -> bool {
        let (l, t, w, h) = body;
        cursor.0 < l - buf
            || cursor.0 >= l + w + buf
            || cursor.1 < t - buf
            || cursor.1 >= t + h + buf
    }

    /// 设/清 `WS_EX_TRANSPARENT`（整窗对鼠标透明）——直接改 EXSTYLE,不走 tao 的
    /// `set_ignore_cursor_events`（那会连带加 `WS_EX_LAYERED`,见常量注释）。
    /// **只改样式位,不做 `SetWindowPos（FRAMECHANGED)`**（㊽）：框架重算会让
    /// 系统把窗口框架整块重绘一遍——「划过容器时瞬间闪出原生窗体/标题栏」
    /// 的头号来源（每次让出/恢复各一次）;这个样式位的生效本不依赖框架重算,
    /// 若个别场景不生效,还有 `set_window_body_region` 的区域收紧兜底。
    /// 返回是否真的改了样式（幂等调用静默返回 false）。
    fn set_mouse_transparent(hwnd: HWND, on: bool) -> bool {
        unsafe {
            let cur = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            let next =
                if on { cur | WS_EX_TRANSPARENT_BIT } else { cur & !WS_EX_TRANSPARENT_BIT };
            if next == cur {
                return false;
            }
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next as isize);
            true
        }
    }

    /// 摘掉窗口样式里的整组「带框窗口」特征位（`FRAME_STYLE_BITS`;幂等,返回是否
    /// 真的改了）——「闪出原生标题栏」的样式闸。只改样式位,不做
    /// `SWP_FRAMECHANGED`：绘制方读的是当前样式,框架重算反而是一次整块重绘。
    /// ⚠ 这不是一次性的活：tao 每次 flag 变更（show/hide、置顶、可缩放…）的
    /// `apply_diff` 都会 `SetWindowLongW（GWL_STYLE, to_window_styles)` 把整组
    /// 样式**写回**（连带 `SWP_FRAMECHANGED` 框架重算,见 tao window_state.rs）,
    /// 所以由让出轮询（`sync_pointer_pass`）逐帧对账 + 显隐钩子（`reset_pointer_pass`）
    /// 一起保证它长期不在。
    /// 副作用核对：拖动走 `WM_NCLBUTTONDOWN（HTCAPTION)` → `SC_MOVE` 模态移动循环,
    /// 不要求窗口真有 caption/系统菜单（无框 popup 惯用同一招）;最小化/关闭由
    /// tao 直接 `ShowWindow` / `WM_CLOSE`,不经系统菜单。
    fn strip_frame_styles(hwnd: HWND) -> bool {
        unsafe {
            let cur = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
            if cur & FRAME_STYLE_BITS == 0 {
                return false;
            }
            SetWindowLongPtrW(hwnd, GWL_STYLE, (cur & !FRAME_STYLE_BITS) as isize);
            true
        }
    }

    /// 关掉 DWM 对该窗口的**非客户区渲染**（「闪出原生标题栏」的 DWM 闸）：把非
    /// 客户区绘制**固定**在旧式路径上——带窗口区域的窗口 DWM 本就不合成框架,不固定
    /// 的话每次区域装/卸都在「DWM 合成 ↔ 旧式绘制」之间切一次,切换本身就是一次
    /// 框架重绘（㊾ 之前的「偶发闪」）;固定后剩下的旧式绘制入口由 `subclass_proc`
    /// 一律拒绝。一次 DWM 调用、幂等;tao 不碰这个属性（它只写暗色模式那一项）,
    /// 所以 `install` 与显隐钩子各调一次足够。
    fn disable_nc_rendering(hwnd: HWND) {
        let policy: i32 = DWMNCRP_DISABLED;
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_NCRENDERING_POLICY as u32,
                &policy as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            );
        }
    }

    /// 主体 + 阴影余量在**窗口坐标**中的矩形（纯计算,物理像素）——`SetWindowRgn`
    /// 的输入：主体取 `content_rect` 零原点版（与屏幕坐标口径同源）,外扩一圈
    /// `CONTENT_MARGIN_LOGICAL`（表盘投影/描边发光不落在区域外,让出瞬间阴影不被裁）。
    fn body_window_rect_at(scale: f64, expanded: bool) -> (i32, i32, i32, i32) {
        let (l, t, w, h) = content_rect((0, 0), scale, expanded);
        let m = (CONTENT_MARGIN_LOGICAL * scale).round() as i32;
        (l - m, t - m, w + 2 * m, h + 2 * m)
    }

    /// 现取窗口形态/DPI 的让出区域（窗口坐标,物理像素;见 `body_window_rect_at`）。
    fn body_window_rect(window: &WebviewWindow) -> (i32, i32, i32, i32) {
        body_window_rect_at(window_dpi_scale(window), super::expanded_state(window.app_handle()))
    }

    /// 让出的**硬保证**：把窗口区域收紧到「主体 + 阴影余量」（`SetWindowRgn`）——
    /// 区域外的画布**在几何上不属于窗口**,命中测试绝不会命中,不依赖「系统是否
    /// 尊重 WS_EX_TRANSPARENT / HTTRANSPARENT」这类跨进程语义（
    /// 只设样式位仍被遮挡——画布继续吃消息、挡住下层程序的悬停与点击）。恢复用
    /// `NULL` 区域（整窗）。
    /// ⚠ `SetWindowRgn` 成功后**系统接管 region 所有权**,不可再删;失败才自己删。
    /// `bredraw = 0`：不让系统**立即**重绘（㊽——立即重绘是「划过一次闪一下」的
    /// 来源之一）;区域变化由合成器在下一帧按新形状重画,无中间帧。
    /// ⚠ 区域对「框架重算」敏感：tao 若因 show/hide 等操作重排框架会把它重置回
    /// 整窗——靠 `window_region_matches` 对账自愈（见 `sync_pointer_pass`）。
    fn set_window_body_region(window: &WebviewWindow, hwnd: HWND, on: bool) {
        unsafe {
            if !on {
                SetWindowRgn(hwnd, std::ptr::null_mut(), 0);
                return;
            }
            let (l, t, w, h) = body_window_rect(window);
            let rgn = CreateRectRgn(l, t, l + w, t + h);
            if rgn.is_null() {
                return;
            }
            if SetWindowRgn(hwnd, rgn, 0) == 0 {
                DeleteObject(rgn);
            }
        }
    }

    /// 窗口当前区域是否已等于目标矩形（读回校验;无区域/非矩形区域一律不匹配）。
    /// 对账用：tao 的 `SetWindowPos（FRAMECHANGED)` 等会把窗口区域重置回整窗。
    fn window_region_matches(hwnd: HWND, target: (i32, i32, i32, i32)) -> bool {
        unsafe {
            let probe = CreateRectRgn(0, 0, 1, 1);
            if probe.is_null() {
                return false;
            }
            let kind = GetWindowRgn(hwnd, probe);
            let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            let hit = kind == SIMPLEREGION
                && GetRgnBox(probe, &mut r) != 0
                && r.left == target.0
                && r.top == target.1
                && r.right == target.2
                && r.bottom == target.3;
            DeleteObject(probe);
            hit
        }
    }

    /// **指针让出状态机**：光标离开交互主体 ⇒ 让出鼠标
    /// （样式位 + 区域收紧两件套）;回主体 ⇒ 恢复。触发点 = WM_NCHITTEST /
    /// WM_MOUSEMOVE（即时）、常驻轮询（`PASS_TIMER_ID`,35ms——穿透期间收不到
    /// 鼠标消息,只有轮询能发现「光标回到主体」;「光标从窗口外进入画布」也靠它
    /// 兜底）、几何写入口（窗口移动改变光标与主体的相对关系）。只有状态翻转才
    /// 动作;拖动中早退（移动循环持有鼠标,重设样式会打断拖动）。
    fn sync_pointer_pass(window: &WebviewWindow) {
        let Some(ctx) = CTX.get() else { return };
        if in_move_loop() {
            return;
        }
        let Ok(hwnd) = window.hwnd() else { return };
        let hwnd = hwnd.0 as HWND;
        let passed = ctx.pointer_pass.load(Ordering::SeqCst);
        // 带框样式对账（见 FRAME_STYLE_BITS）：tao 的 flag 变更会把整组样式写回、
        // CAPTION | SYSMENU | MINIMIZEBOX 复活——每轮一次便宜的读回,失配即摘。
        // 复摘本身不触发框架重算（只改样式位、不带 SWP_FRAMECHANGED）,所以它是
        // 样式闸的长期保证;`install` 与显隐钩子各摘一次负责「重启/显隐后立刻到位」。
        if strip_frame_styles(hwnd) {
            crate::dev_log!("[orb-dock] frame styles re-stripped (tao style rebuild)");
        }
        // 样式位校验/自愈：tao 的 show/hide 会用缓存 flags 全量重建样式、抹掉我们
        // 的位（它不知道我们直接改了 EXSTYLE）——每轮一次便宜的读回,失配即补两件套。
        let actual =
            unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 } & WS_EX_TRANSPARENT_BIT != 0;
        if actual != passed {
            set_mouse_transparent(hwnd, passed);
            set_window_body_region(window, hwnd, passed);
        }
        // 区域对账：tao 的 SetWindowPos（FRAMECHANGED) 等操作会把窗口区域重置回
        // 整窗（它不知道我们设过区域）——每轮读回保底（微秒级）,失配即重设。
        if passed {
            let (l, t, w, h) = body_window_rect(window);
            if !window_region_matches(hwnd, (l, t, l + w, t + h)) {
                set_window_body_region(window, hwnd, true);
            }
        }
        let Some(cursor) = cursor_screen_pos() else { return };
        let Some(body) = body_rect(window) else { return };
        let buf = (PASS_HYSTERESIS_LOGICAL * window_dpi_scale(window)).round() as i32;
        // 让出：离主体 > buf 才算离开;恢复：回到矩形内即算——两档之间保持现状。
        let target =
            if passed { pointer_outside(cursor, body, 0) } else { pointer_outside(cursor, body, buf) };
        if target == passed {
            return;
        }
        set_mouse_transparent(hwnd, target);
        set_window_body_region(window, hwnd, target);
        ctx.pointer_pass.store(target, Ordering::SeqCst);
        if target {
            crate::dev_log!(
                "[orb-dock] pointer pass ON cursor=({},{}) body=({},{},{},{})",
                cursor.0,
                cursor.1,
                body.0,
                body.1,
                body.2,
                body.3
            );
        } else {
            crate::dev_log!("[orb-dock] pointer pass OFF cursor=({},{})", cursor.0, cursor.1);
        }
        use tauri::Emitter;
        let _ = window.app_handle().emit("orb-pointer-pass", target);
    }

    /// 几何变化后重设让出区域（尺寸/位置/DPI 变化可能让系统重置或错位窗口区域）;
    /// 只在让出态有意义,幂等静默。
    fn refresh_pass_region(window: &WebviewWindow) {
        let Some(ctx) = CTX.get() else { return };
        if !ctx.pointer_pass.load(Ordering::SeqCst) {
            return;
        }
        let Ok(hwnd) = window.hwnd() else { return };
        set_window_body_region(window, hwnd.0 as HWND, true);
    }

    /// 复位让出态（显隐切换调用;`active` = 窗口此后是否可）：清两件套 + 复位
    /// 标记;可见才启动常驻轮询（隐藏期间没必要扫光标）。隐藏期间没有鼠标消息,
    /// 残留的让出态会在下次显示时把整个窗口从鼠标里吃掉（主体都点不动）——所以
    /// **无条件清样式与区域**（不依赖状态标记:tao 重建样式可能已把位抹掉,
    /// 标记与真实样式可能不同步）。
    pub fn reset_pointer_pass(window: &WebviewWindow, active: bool) {
        let Some(ctx) = CTX.get() else { return };
        let was = ctx.pointer_pass.swap(false, Ordering::SeqCst);
        if let Ok(hwnd) = window.hwnd() {
            let hwnd = hwnd.0 as HWND;
            let changed = set_mouse_transparent(hwnd, false);
            set_window_body_region(window, hwnd, false);
            // 显隐钩子顺带把「原生标题栏」的样式闸与 DWM 闸再按一遍：tao 的
            // show/hide 走 `apply_diff`,会把整组带框样式连同 `SWP_FRAMECHANGED`
            // 一起写回（见 strip_frame_styles 注释）——这里摘掉才能让窗口一露面
            // 就是无框的。
            let restripped = strip_frame_styles(hwnd);
            disable_nc_rendering(hwnd);
            if restripped {
                crate::dev_log!("[orb-dock] frame styles re-stripped (visibility)");
            }
            if active {
                unsafe { SetTimer(hwnd, PASS_TIMER_ID, PASS_POLL_MS, None) };
            } else {
                unsafe { KillTimer(hwnd, PASS_TIMER_ID) };
            }
            if was || changed {
                crate::dev_log!("[orb-dock] pointer pass reset (visibility, active={active})");
            }
        }
    }

    /// 记下移动循环起点（WM_ENTERSIZEMOVE;早于循环内任何位移）——位置用于
    /// 「真被拖走」判定,同时屏工作区用于跨缝补判。
    fn remember_drag_origin() {
        let Some(ctx) = CTX.get() else { return };
        let Some((pos, size)) = window_geom(ctx) else { return };
        let screens = screens(&ctx.window);
        let work = if screens.is_empty() {
            [0, 0, 0, 0]
        } else {
            let w = screens[content_screen_index(&screens, pos, size)].work;
            [w.left, w.top, w.right, w.bottom]
        };
        *ctx.drag_origin.lock().unwrap() = Some(DragOrigin { pos, work });
    }

    /// 取出移动循环起点（一次性;EXIT 判定后即清,不残留到下一轮）。
    fn take_drag_origin() -> Option<DragOrigin> {
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
            let tol = (DOCK_KEEP_TOLERANCE_LOGICAL * right.scale).round() as i32; // 24
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

        /// 自由态触发以**光标**（鼠标点击/拖动位置）为起点（修㊲ 二次修订,2026-09-13
        /// 用户实测「圆盘还没接触屏幕就触发」的回归护栏）：光标到**内容所在屏**竖缘
        /// ≤ 几像素才触发,内容矩形不参与;越过该缘（含推过接缝）同样算触缘。
        /// ⚠ 参照屏只有这一台（内容所在屏）——候选里再塞别的屏会让归位跨屏失败
        /// （2026-09-13 实测「贴边弹出很远距离,不是贴靠边沿」）。
        #[test]
        fn cursor_touch_uses_few_pixel_eps() {
            let screen = Screen {
                rect: wa(2560, 0, 5656, 1968),
                work: wa(2560, 0, 5656, 1968),
                scale: 1.5,
            };
            // 离缘 50（@1.5,远大于几像素档）→ 不触发
            assert_eq!(cursor_dock_target(&screen, (2610, 500)), None);
            // 左缘：距缘 4 → 触发;越过该缘（光标留在缘外）→ 也算触缘
            assert_eq!(
                cursor_dock_target(&screen, (2564, 500)),
                Some((4, OrbDockEdge::Left))
            );
            assert_eq!(
                cursor_dock_target(&screen, (2500, 500)),
                Some((60, OrbDockEdge::Left))
            );
            // 右缘：距缘 2 → 触发;推过接缝（已越到隔壁屏）→ 仍算触缘本屏右缘
            assert_eq!(
                cursor_dock_target(&screen, (5654, 500)),
                Some((2, OrbDockEdge::Right))
            );
            assert_eq!(
                cursor_dock_target(&screen, (5700, 500)),
                Some((44, OrbDockEdge::Right))
            );
            // 屏中间：不触发
            assert_eq!(cursor_dock_target(&screen, (4000, 500)), None);
        }

        /// 自由态**竖条**的落点口径 = 本体（与已停靠同尺;修㊶,2026-09-13 用户实测
        /// 回归）：手动折叠的竖条拖到离左屏右缘 7px 松手——光标在竖条中部（离缘
        /// 19px > eps 6 不触缘）、屏内拖动又不满足跨缝补判的前置 ⇒ 旧口径两档全落空,
        /// 竖条被钳到离缘 24px 处,不是「重定位吸附」的固定距离。新口径:竖条形态走
        /// 本体判定,7 ≤ keep_tol(24) ⇒ 吸附贴缘。
        #[test]
        fn pill_shape_uses_body_not_cursor() {
            let left = Screen {
                rect: wa(0, 0, 2560, 1440),
                work: wa(0, 0, 2560, 1368),
                scale: 1.5,
            };
            let tol = (DOCK_KEEP_TOLERANCE_LOGICAL * left.scale).round() as i32; // 24
            // 实测落点:内容 (2517,843,36,126) → 右缘距缘 7
            assert_eq!(
                dock_target(&left, rect(2517, 843, 36, 126), tol),
                Some((7, OrbDockEdge::Right))
            );
            // 同一次拖动里光标档确实不触发（19px > eps 6）——旧口径的漏网实证
            assert_eq!(cursor_dock_target(&left, (2541, 926)), None);
        }

        /// 视觉主体跨度：展开态要裁掉内容盒右侧的「间隙 6 + 按钮列 16 = 22 逻辑
        /// 像素 × scale」（表盘才是主体）;收起态内容盒 = 竖条本体,原样返回。
        #[test]
        fn visual_span_drops_button_column_when_expanded() {
            // 展开态 @scale 2：内容盒 264 = 表盘 220 + 44
            assert_eq!(visual_span(true, rect(1000, 500, 264, 220), 2.0), (1000, 1220));
            // 收起态 @scale 2：内容盒 48 就是本体
            assert_eq!(visual_span(false, rect(1000, 500, 48, 168), 2.0), (1000, 1048));
        }

        /// 跨缝补判（修㊳,同日二次修订）：窗口中心换过屏 ⇒ 光标必然已深入新屏、
        /// 光标口径失效,此时按「**主体是否完整显示**」补判——主体还压着缝（尚未
        /// 整体进入新屏）⇒ 贴;离缝超过几像素 = 已完整显示 ⇒ 不贴（用户定案：
        /// 与「拖入边缘时主体被截断才触发贴边」是同一条逻辑的镜像）。
        /// 护栏是 2026-09-13 实测的「从接缝左侧拖到右侧,表盘停在缝上,既不贴边也
        /// 不弹开」（dev 日志 `release … cursor=(2629,982) … scale=2 … target=None`）。
        /// 接缝 = 2560（左屏 1.5 收在 2560,右屏 2.0 从 2560 起）。
        #[test]
        fn seam_landing_docks_until_body_fully_visible() {
            let right = Screen {
                rect: wa(2560, 0, 6400, 2160),
                work: wa(2560, 0, 6400, 2160),
                scale: 2.0,
            };
            let eps = (EDGE_EPS_LOGICAL * right.scale).round() as i32; // 8
            let radius = (DIAL_RADIUS_LOGICAL * right.scale).round() as i32; // 110
            let expanded = |l: i32| {
                visual_span(
                    true,
                    rect(l, 900, (EXPANDED_INNER_W_LOGICAL * right.scale).round() as i32, 220),
                    right.scale,
                )
            };
            // 表盘中心刚过缝（左缘还在缝左侧一个半径处 = 压着缝）⇒ 贴右屏左缘
            assert_eq!(
                seam_landing_target(&right, expanded(2560 - radius), true),
                Some((0, OrbDockEdge::Left))
            );
            // 左缘刚好压线 / 刚整体进入几像素（≤ eps）⇒ 仍算贴,间距按屏内一侧算
            assert_eq!(
                seam_landing_target(&right, expanded(2560), true),
                Some((0, OrbDockEdge::Left))
            );
            assert_eq!(
                seam_landing_target(&right, expanded(2560 + eps - 1), true),
                Some((eps - 1, OrbDockEdge::Left))
            );
            assert_eq!(
                seam_landing_target(&right, expanded(2560 + eps), true),
                Some((eps, OrbDockEdge::Left))
            );
            // 离缝超过几像素 ⇒ 主体已完整显示 ⇒ 不贴（含实测那次 41px 的落点）
            assert_eq!(seam_landing_target(&right, expanded(2560 + eps + 1), true), None);
            assert_eq!(seam_landing_target(&right, expanded(2601), true), None);
            // 没换过屏 ⇒ 补判不开口（屏内拖动仍要求光标真推到边）
            assert_eq!(seam_landing_target(&right, expanded(2560), false), None);

            // 反向（右→左）：贴**左屏右缘**（接缝另一侧的同一物理位置,
            // 但几何按左屏算——归位屏 = 内容所在屏）。
            let left = Screen {
                rect: wa(0, 0, 2560, 1440),
                work: wa(0, 0, 2560, 1440),
                scale: 1.5,
            };
            let body_l = (DIAL_RADIUS_LOGICAL * 2.0 * left.scale).round() as i32; // 165
            // 表盘右缘离缝 2px（尚未完整进入左屏）⇒ 贴左屏右缘
            assert_eq!(
                seam_landing_target(&left, (2558 - body_l, 2558), true),
                Some((2, OrbDockEdge::Right))
            );
            // 已整体进入左屏（离缝 10px > eps 6）⇒ 不贴
            assert_eq!(seam_landing_target(&left, (2550 - body_l, 2550), true), None);
        }

        /// 贴边矩形（纯函数,修㊴ 起 dock 归位的唯一算法）：竖条**本体**距工作区缘
        /// 4 逻辑像素（窗口透明边距允许出屏）,中心 Y 按 ratio 落并钳在工作区内。
        #[test]
        fn docked_rect_puts_body_four_px_from_edge() {
            let work = wa(2560, 0, 6400, 2160);
            let (x, y, w_log, h_log) = docked_rect(OrbDockEdge::Right, 0.5, 2.0, &work);
            assert_eq!((w_log, h_log), (PILL_W_LOGICAL, PILL_H_LOGICAL));
            // shift = (16 − 4) × 2 = 24 ⇒ 窗口右缘出屏 24px
            assert_eq!(x, 6400 - 112 + 24);
            // 竖条本体（左 16 + 宽 24 逻辑 = 80 物理）右缘 = 6400 − 8 ⇒ 距缘 8 物理 = 4 逻辑
            assert_eq!(x + 80, 6400 - 8);
            // 左缘镜像：窗口左缘出屏
            assert_eq!(docked_rect(OrbDockEdge::Left, 0.5, 2.0, &work).0, 2560 - 24);
            // 锚 Y：ratio 0.5 ⇒ 窗口中心落在工作区中线 1080,再上移半个**物理**窗口高
            // （116 逻辑 × 2 = 232 ⇒ 半高 116）
            assert_eq!(y, 1080 - 116);
        }

        /// 内容锚定矩形（纯函数）：**可见内容左上角不动** ⇒ 换形态只换偏移与窗口尺寸
        /// （窗口位置 = 内容原点 − 该形态偏移;尺寸 = 该形态逻辑尺寸）。
        #[test]
        fn anchored_rect_keeps_content_origin() {
            let (x_exp, y_exp, ew, eh) = anchored_rect((1000, 800), true, 1.5);
            assert_eq!((ew, eh), (EXPANDED_W_LOGICAL, EXPANDED_H_LOGICAL));
            // 235×1.5 = 352.5 → 353;100×1.5 = 150
            assert_eq!((x_exp, y_exp), (647, 650));
            let (x_col, y_col, cw, ch) = anchored_rect((1000, 800), false, 1.5);
            assert_eq!((cw, ch), (PILL_W_LOGICAL, PILL_H_LOGICAL));
            // 16×1.5 = 24
            assert_eq!((x_col, y_col), (976, 776));
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

        /// 记录形态判定（只用于换算内容原点）：显式字段优先;旧格式（无字段）按落盘尺寸反推。
        #[test]
        fn recorded_form_prefers_field_then_size() {
            // 显式字段优先（即使与尺寸矛盾——字段是形态的权威记录）
            assert!(recorded_form(Some(true), (112, 232), 2.0));
            assert!(!recorded_form(Some(false), (1160, 620), 2.0));
            // 旧格式（无字段）按记录尺寸反推 @scale 2.0（112×232 = 竖条;1160×620 = 表盘）
            assert!(!recorded_form(None, (112, 232), 2.0));
            assert!(recorded_form(None, (1160, 620), 2.0));
        }

        /// 未贴边启动一律表盘（2026-09-17 用户定案）：记录是表盘 ⇒ 内容原位;记录是自由
        /// 竖条 ⇒ 表盘内容原点落在竖条内容原点（原地长大,不按表盘偏移硬套竖条窗口位置）。
        #[test]
        fn startup_dial_grows_from_recorded_content_origin() {
            let scale = 1.5;
            let pos = (1000, 600);
            assert_eq!(startup_dial_content(pos, scale, true), content_rect(pos, scale, true));
            let pill = content_rect(pos, scale, false);
            let dial = startup_dial_content(pos, scale, false);
            assert!((dial.0 - pill.0).abs() <= 1 && (dial.1 - pill.1).abs() <= 1, "{dial:?} vs {pill:?}");
            assert_eq!((dial.2, dial.3), (content_rect(pos, scale, true).2, content_rect(pos, scale, true).3));
        }

        /// 形态定性：外部传入的逻辑尺寸按最近的常量归属（收起 ↔ 展开）。
        #[test]
        fn looks_expanded_by_nearest_size() {
            assert!(!looks_expanded(PILL_W_LOGICAL, PILL_H_LOGICAL));
            assert!(looks_expanded(EXPANDED_W_LOGICAL, EXPANDED_H_LOGICAL));
        }

        /// 指针让出的滞回两档（修㊻）：让出判定带缓冲（离主体 > buf 才算离开）、
        /// 恢复判定不带（回到矩形内即恢复）——两档之间是「维持现状」带,边界不抖动。
        #[test]
        fn pointer_pass_hysteresis_bands() {
            // 主体 (100,50)-(232,160)（物理视角;右/下为半开区间）
            let body = rect(100, 50, 132, 110);
            // 矩形内：不让出
            assert!(!pointer_outside((150, 100), body, 6));
            // 左边外 2px：在 6px 缓冲带内 → 维持现状（不算离开）
            assert!(!pointer_outside((98, 100), body, 6));
            // 左边外 8px：超出缓冲 → 让出
            assert!(pointer_outside((92, 100), body, 6));
            // 恢复档（buf=0）：矩形边界内即算「回主体」,刚好压边（半开区间）算外
            assert!(!pointer_outside((100, 50), body, 0));
            assert!(!pointer_outside((231, 159), body, 0));
            assert!(pointer_outside((99, 50), body, 0));
            assert!(pointer_outside((232, 160), body, 0));
            // 缓冲同时作用于右/下两侧
            assert!(!pointer_outside((236, 164), body, 6));
            assert!(pointer_outside((239, 167), body, 6));
        }

        /// 让出区域（修㊼）= 主体 + 阴影余量（窗口坐标,物理像素）——展开态 @1.5:
        /// 主体 (353,150,198,165) 各边外扩 24 → (329,126,246,213);
        /// 收起态 @2.0:主体 (32,32,48,168) 外扩 32 → (0,0,112,232)。
        #[test]
        fn pass_region_wraps_body_with_margin() {
            assert_eq!(body_window_rect_at(1.5, true), (329, 126, 246, 213));
            assert_eq!(body_window_rect_at(2.0, false), (0, 0, 112, 232));
        }

        /// 意图 → **物理**矩形（修㊵）：逻辑 → 物理的换算取**目标屏自己的 scale**——
        /// 贴边按 work 找回的那台屏（窗口此刻可能还挂在另一台 DPI 不同的屏上）、
        /// 锚定按内容所在屏。写入据此走物理值,不受 tao 缓存滞后影响（回归:缓存滞后
        /// 期间把 56×116 写成 112×232,曾造成「贴片嵌入接缝 + 表盘闪烁」的风暴）。
        #[test]
        fn desired_rect_scales_by_target_screen() {
            let left = Screen { rect: wa(0, 0, 2560, 1440), work: wa(0, 0, 2560, 1368), scale: 1.5 };
            let right =
                Screen { rect: wa(2560, 0, 5656, 2160), work: wa(2560, 0, 5656, 1968), scale: 2.0 };
            let all = [left, right];
            // 贴左屏右缘（窗口此刻还挂在右屏,scale 2.0）：尺寸必须按左屏 1.5 → 84×174;
            // x = 2560 − 84 + (16−4)×1.5
            let (x, _, w, h) = desired_rect(
                Desired::Docked { edge: OrbDockEdge::Right, ratio: 0.5, work: [0, 0, 2560, 1368] },
                &right,
                &all,
            );
            assert_eq!((w, h), (84, 174));
            assert_eq!(x, 2560 - 84 + 18);
            // 镜像（贴右屏左缘,窗口挂在左屏）：按右屏 2.0 → 112×232;x = 2560 − (16−4)×2
            let (x, _, w, h) = desired_rect(
                Desired::Docked { edge: OrbDockEdge::Left, ratio: 0.5, work: [2560, 0, 5656, 1968] },
                &left,
                &all,
            );
            assert_eq!((w, h), (112, 232));
            assert_eq!(x, 2560 - 24);
            // 锚定：按内容所在屏（左屏 1.5 → 870×465;右屏 2.0 → 1160×620）
            let (_, _, w, h) = desired_rect(
                Desired::Anchored { origin: (100, 100), expanded: true },
                &left,
                &all,
            );
            assert_eq!((w, h), (870, 465));
            let (_, _, w, h) = desired_rect(
                Desired::Anchored { origin: (3000, 100), expanded: true },
                &right,
                &all,
            );
            assert_eq!((w, h), (1160, 620));
        }
    }
}
