//! 毛玻璃材质（spike）：DWM 路线桌面级背板（CSS backdrop-filter
//! 摸不到桌面）。直绑 window-vibrancy 0.6.0，Win32 路径与版本门槛
//! （SWCA=17763 / 未公开 Mica=22000 / host backdrop=22523）已按源码核实
//!。
//!
//! 档位：none / mica / acrylic，两窗口（widget/main）各自独立生效；
//! apply 前先 clear 两种机制——DWMSBT（Mica / host acrylic）与 SWCA
//! （旧 acrylic）是两个独立槽位，跨档切换不 clear 会叠加。
//!
//! 失败静默哲学：window-vibrancy 对平台/版本不支持返回 Err，
//! 前端收到 Err 后回退关闭档；host backdrop 路径内 DwmSetWindowAttribute
//! 返回值被库忽略（不报 Err），版本支持性以 Err 分支为准。
//!
//! 主窗口圆角恒为 DWM ROUND，材质
//! 开关只管背板；挂件材质态需要 DWM 圆角，
//! 但挂件默认组合含 NCRP_DISABLED（与 ROUND 互斥），走
//! chrome.rs 的原子组合切换 apply_widget_material_chrome。
//! 已知：挂件材质态阴影回归不可去（NCRP 是阴影/边框/圆角的总开关，
//! 与 ROUND 裁切互斥；tao set_shadow（false) 只是尺寸 insets 计算而非
//! DWM 阴影开关）——此伴随物。

use tauri::{AppHandle, Manager};

#[tauri::command]
pub fn set_window_material(
    app: AppHandle,
    label: String,
    effect: String,
    dark: bool,
) -> Result<(), String> {
    set_window_material_impl(&app, &label, &effect, dark)
}

#[cfg(windows)]
fn set_window_material_impl(
    app: &AppHandle,
    label: &str,
    effect: &str,
    dark: bool,
) -> Result<(), String> {
    let window = app
        .get_webview_window(label)
        .ok_or_else(|| format!("window not found: {}", label))?;

    // 清两机制再上目标档（幂等；<22000 平台上 clear_mica 返回 Err 属预期，容忍）。
    let _ = window_vibrancy::clear_mica(&window);
    let _ = window_vibrancy::clear_acrylic(&window);

    match effect {
        "mica" => window_vibrancy::apply_mica(&window, Some(dark)).map_err(|e| e.to_string())?,
        "acrylic" => window_vibrancy::apply_acrylic(&window, None).map_err(|e| e.to_string())?,
        "none" => {}
        other => return Err(format!("unknown effect: {}", other)),
    }

    // 主窗口圆角恒为 DWM ROUND（apply_main_chrome 启动
    // 装配），材质开关不再翻转圆角；挂件材质态走 NCR 四属性原子组合切换
    // （ROUND + 撤销 NCRP_DISABLED，教训），Off 档完整还原。
    if label == "widget" {
        crate::chrome::apply_widget_material_chrome(&window, effect != "none");
    }
    Ok(())
}

#[cfg(not(windows))]
fn set_window_material_impl(
    _app: &AppHandle,
    label: &str,
    _effect: &str,
    _dark: bool,
) -> Result<(), String> {
    Err(format!("window materials are Windows-only (label: {})", label))
}
