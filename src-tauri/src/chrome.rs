//! 窗口形态（chrome）装配：widget / main 等是各自独立的 WebviewWindow，形态在创建后固定，
//! 不做 decorations 往返切换（该切换会丢 alpha）。
//!
//! WIDGET_W/H 默认值 = large 档（SIZE_PRESETS 单一源在前端 designPrefs.ts，此处为启动首帧
//! 同值副本；运行时档位切换走 set_widget_size，不回读 Rust 常量）。
//!
//! widget 的边缘处置是原子组合，默认态**勿改动**：
//! 系统全静默（DONOTROUND + BORDER NONE + NCRP_DISABLED + set_shadow（false)）
//! + CSS 全权（16px 圆角 + isolate 合成层）。材质态是唯一例外，且必须走
//! apply_widget_material_chrome 的**原子组合切换**（勿单独改其一）。

use tauri::WebviewWindow;

pub const WIDGET_W: f64 = 1041.0;
// large 档 155（月标签为浮层、上下边距 12/12 对称）；
// 单一源在 designPrefs.SIZE_PRESETS，此处为启动首帧同值副本。
pub const WIDGET_H: f64 = 155.0;
// main（expanded 形态）默认尺寸 1040×660 单一源在 tauri.conf.json——
// 双窗口架构下 main 形态创建后固定，Rust 侧不再有切尺寸的时机。

#[cfg(windows)]
mod dwm_attrs {
    pub const DWMWA_NCRENDERING_POLICY: u32 = 2;
    pub const DWMNCRP_DISABLED: u32 = 2;
    pub const DWMNCRP_USEWINDOWSTYLE: u32 = 0;
    pub const DWMWA_BORDER_COLOR: u32 = 34;
    pub const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;
}

/// DWM 窗口圆角（DWMWA_WINDOW_CORNER_PREFERENCE=33）。
/// widget 默认 DONOTROUND：圆角由 CSS 16px 独占（Chromium 单次 AA）——系统 ROUND 与
/// CSS 内容边界叠加会产生亚像素毛边，且 DWM 边框状态机不受属性层完全控制（激活时
/// 强调色边框回弹），属平台级限制。
/// 受控例外：
/// - main 常态 ROUND（呼吸位 8px 错开 CSS 卡片圆角，不叠加；无回弹）；
/// - timeline Shadow 风格 ROUND（同 main 组合）；
/// - widget 材质态 ROUND（CSS 圆角退出单源交 DWM，原子组合内切换）。
#[cfg(windows)]
pub fn set_corner_preference(window: &WebviewWindow, round: bool) {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND,
    };
    if let Ok(hwnd) = window.hwnd() {
        let pref = if round { DWMWCP_ROUND } else { DWMWCP_DONOTROUND } as u32;
        unsafe {
            DwmSetWindowAttribute(
                hwnd.0 as *mut core::ffi::c_void,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &pref as *const u32 as *const core::ffi::c_void,
                4,
            );
        }
    }
}

#[cfg(not(windows))]
pub fn set_corner_preference(_window: &WebviewWindow, _round: bool) {}

/// 移除 Win11 系统 1px 边框（DWMWA_BORDER_COLOR=34 → COLOR_NONE）。
#[cfg(windows)]
fn set_border_color_none(window: &WebviewWindow) {
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    if let Ok(hwnd) = window.hwnd() {
        let color: u32 = dwm_attrs::DWMWA_COLOR_NONE;
        unsafe {
            DwmSetWindowAttribute(
                hwnd.0 as *mut core::ffi::c_void,
                dwm_attrs::DWMWA_BORDER_COLOR,
                &color as *const u32 as *const core::ffi::c_void,
                4,
            );
        }
    }
}

#[cfg(not(windows))]
fn set_border_color_none(_window: &WebviewWindow) {}

/// 关闭 DWM 非客户区渲染（DWMNCRP_DISABLED）：阴影/边框/高光整套不画。
/// 边缘 100% 由 CSS 表达的前提；set_shadow（false) 双保险重申玻璃边距归零。
#[cfg(windows)]
fn set_ncr_disabled(window: &WebviewWindow) {
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    if let Ok(hwnd) = window.hwnd() {
        let policy: u32 = dwm_attrs::DWMNCRP_DISABLED;
        unsafe {
            DwmSetWindowAttribute(
                hwnd.0 as *mut core::ffi::c_void,
                dwm_attrs::DWMWA_NCRENDERING_POLICY,
                &policy as *const u32 as *const core::ffi::c_void,
                4,
            );
        }
    }
    let _ = window.set_shadow(false);
}

#[cfg(not(windows))]
fn set_ncr_disabled(_window: &WebviewWindow) {}

/// NCR 策略恢复系统默认（USEWINDOWSTYLE）：撤销 NCRP_DISABLED。
/// 用于挂件材质态与 timeline Shadow 风格——NCRP_DISABLED 的透明 NC 带会垫在内容与
/// ROUND 裁切弧之间造成 1px 毛边，DWM 单源圆角时必须撤销。
#[cfg(windows)]
fn set_ncr_default(window: &WebviewWindow) {
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    if let Ok(hwnd) = window.hwnd() {
        let policy: u32 = dwm_attrs::DWMNCRP_USEWINDOWSTYLE;
        unsafe {
            DwmSetWindowAttribute(
                hwnd.0 as *mut core::ffi::c_void,
                dwm_attrs::DWMWA_NCRENDERING_POLICY,
                &policy as *const u32 as *const core::ffi::c_void,
                4,
            );
        }
    }
}

#[cfg(not(windows))]
fn set_ncr_default(_window: &WebviewWindow) {}

/// 应用 widget 窗口形态（setup 启动时一次性调用）。边缘处置为
/// 「系统全静默 + CSS 全权」组合，固定不变（应用边缘的原子组合）。
pub fn apply_widget_chrome(window: &WebviewWindow) {
    let _ = window.set_decorations(false);
    let _ = window.set_size(tauri::LogicalSize::new(WIDGET_W, WIDGET_H));
    let _ = window.set_min_size(Some(tauri::LogicalSize::new(480.0, 80.0)));
    set_corner_preference(window, false);
    set_border_color_none(window);
    set_ncr_disabled(window);
}

/// 应用 main（expanded 形态）窗口形态，frameless：
/// - 保留 WS_THICKFRAME（可缩放）+ **DWM 系统阴影**：外缘透明呼吸位 + 系统阴影当悬浮投影；
///   THICKFRAME 的 DWM 阴影压不掉，所以不做 NCR 压制/set_shadow（false)，与 widget 相反；
/// - **ROUND（常态即开）**：窗口轮廓与阴影圆角化，与材质态轮廓一致（DWM ROUND 不引入
///   失焦退化——那是 host backdrop 材质的行为，与圆角属性无关）。与 CSS 卡片圆角不叠加：
///   DWM 裁窗口矩形四角（8px 呼吸位外侧），CSS 16px 裁内容卡片，二者被
///   8px 呼吸位错开不相交——呼吸位是双圆角共存的前提，勿取消；
/// - BORDER NONE：去掉系统 1px 边框（透明环上会显形；ROUND 下无激活强调色回弹）。
pub fn apply_main_chrome(window: &WebviewWindow) {
    let _ = window.set_decorations(false);
    let _ = window.set_min_size(Some(tauri::LogicalSize::new(720.0, 480.0)));
    let _ = window.set_shadow(true);
    set_corner_preference(window, true);
    set_border_color_none(window);
}

/// 应用 orb（悬浮球）窗口形态：复用挂件「系统全静默 + CSS 全权」
/// 原子组合（DONOTROUND + BORDER NONE + NCRP_DISABLED + shadow false），
/// 圆角由 CSS 独占。**与 widget 的差异**：
/// - **置顶**：set_always_on_top（true)——悬浮球/贴边条是 glanceable 常驻件，被普通应用
///   遮挡即失去存在意义（Windows topmost Z-order；其他 topmost 窗口/系统 UI 仍在其上，
///   独占全屏不保证，属正常降级）；
/// - orb 无材质档（不提供毛玻璃，故无 apply_widget_material_chrome 的切换路径，组合装配后
///   固定，勿在此窗口上调材质切换函数）；
/// - **无任何形变能力**：resizable=false（tauri.conf.json 声明侧）+ set_maximizable（false)
///   （此处运行时双保险）。tao 样式映射：WS_SIZEBOX（THICKFRAME) 只挂 RESIZABLE 位、
///   WS_MAXIMIZEBOX 只挂 MAXIMIZABLE 位——两锁落下后 Aero Snap 拖到屏幕边缘的全屏/半屏
///   布局在 Win32 样式层失去触发载体（否则拖 orb 到边缘会误触系统全屏），程序化
///   set_size（两态切换）不走这两条样式路径，不受影响。尺寸不在此设置
///   （tauri.conf.json 初始 56×116 竖条，两态切换走 set_orb_size）。
pub fn apply_orb_chrome(window: &WebviewWindow) {
    let _ = window.set_decorations(false);
    let _ = window.set_maximizable(false);
    let _ = window.set_always_on_top(true);
    set_corner_preference(window, false);
    set_border_color_none(window);
    set_ncr_disabled(window);
}

/// 应用 timeline（项目推进时间轴）窗口装配期形态：边缘栈 = 挂件默认态原子组合
/// （DONOTROUND + BORDER NONE + NCRP_DISABLED + shadow false，圆角由 CSS 独占）。
/// 这只是装配初值：前端挂载即经 set_timeline_style 施加 Shadow / Flat
/// （见 apply_timeline_style_chrome）。
/// 与 widget 的差异：
/// - **不置顶**（看板挂第二屏不需要，主屏会挡编辑器；条态置顶由
///   `set_timeline_form` 切换，不在装配期定死）；
/// - 可缩放（resizable=true 声明侧），min 480×200；初始 900×360 走 tauri.conf.json，
///   此处不 set_size（几何由 window_state restore 接管）；
/// - 无材质档（不装配材质 hook，透明度 CSS 化）。
pub fn apply_timeline_chrome(window: &WebviewWindow) {
    let _ = window.set_decorations(false);
    let _ = window.set_min_size(Some(tauri::LogicalSize::new(480.0, 200.0)));
    set_corner_preference(window, false);
    set_border_color_none(window);
    set_ncr_disabled(window);
}

/// timeline 窗口风格原子组合切换（设置·Appearance 里 Timeline appearance 的 Window style 两档）：
/// - `shadow = true`（Shadow,默认）= 主窗口组合：NCRP 恢复系统默认 + DWM 阴影 + ROUND + BORDER NONE;
///   CSS 侧外缘 8px 全透明呼吸位（错开 DWM 圆角与 CSS 卡片圆角,同 main,勿取消）;
/// - `shadow = false`（Flat）= 装配期组合（DONOTROUND + BORDER NONE + NCRP_DISABLED + shadow false）。
/// 条态一律 Flat（贴顶细条不要阴影 / 呼吸位）;执行者 = timeline_form（形态切换与风格切换都经它）。
/// NCR 相关属性必须整组切换,勿单独改其一。
pub fn apply_timeline_style_chrome(window: &WebviewWindow, shadow: bool) {
    if shadow {
        set_ncr_default(window);
        set_corner_preference(window, true);
        set_border_color_none(window);
        let _ = window.set_shadow(true);
    } else {
        set_corner_preference(window, false);
        set_border_color_none(window);
        set_ncr_disabled(window);
    }
}

/// 挂件材质态边缘组合切换（NCR 四属性必须作为原子组合切换，勿单独改其一）：
/// - 默认（Off）= DONOTROUND + BORDER NONE + NCRP_DISABLED + shadow（false)
///   （apply_widget_chrome 装配的「系统全静默 + CSS 全权」组合）
/// - 材质态 = ROUND + BORDER NONE + NCRP 恢复系统默认 + shadow（false)
///   挂件窗口即卡片（全出血），材质背板填满方角窗口矩形，若保留 CSS 16px 圆角
///   则外四角露背板——所以圆角换 DWM 单源，CSS 圆角/描边同步退出
///   （yearMatrix.css html.material-on 规则）。
///   BORDER NONE + ROUND 无毛边/无激活强调色回弹（主窗口同为此组合）。
///   shadow（false) 维持不变：系统阴影仍是压制态，材质态无阴影。
pub fn apply_widget_material_chrome(window: &WebviewWindow, material_on: bool) {
    set_corner_preference(window, material_on);
    set_border_color_none(window);
    if material_on {
        set_ncr_default(window);
    } else {
        set_ncr_disabled(window);
    }
}
