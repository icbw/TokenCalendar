//! Windows「辅助功能 → 文本大小」感知。
//!
//! Windows 的文本缩放（`HKCU\Software\Microsoft\Accessibility\TextScaleFactor`，
//! 100–225）被 Edge/WebView2 **绑在 DPI 缩放上整体应用**：文本大小 150% 时，
//! WebView 内容按 DPI × 1.5 光栅化，`devicePixelRatio` 随之 ×1.5，但 Win32 的
//! `GetDpiForWindow` / 显示器 DPI 枚举完全不受影响。
//!
//! 主窗口 / 挂件 / 时间轴是可缩放窗口、布局按百分比流动，内容放大后仍自适应；悬浮球是
//! **固定尺寸窗口**（580×310 / 56×116），表盘落点是固定 CSS 坐标，Rust 侧还按「CSS 像素 =
//! 逻辑像素」硬算交互主体与「指针让出」区域（`SetWindowRgn`）——不计入文本缩放时表盘画到
//! 主体矩形右下方且放大，区域收紧后只剩表盘左上角一道弧。
//!
//! 处置：**文本缩放是全局设置，应用要跟随，不做抵消（不用 set_zoom）**。本模块只负责
//! 提供缓存的缩放系数，`orb_dock.rs` 把它乘进两处 DPI 来源（显示器枚举
//! `screens` 与 `window_dpi_scale`），于是悬浮球窗口按 CSS 尺寸 × DPI × 文本
//! 缩放落地，表盘随文本设置同步放大，主体 / 区域 / 贴边判定全部自洽。
//! 运行时改设置 ⇒ 系统广播 `WM_SETTINGCHANGE` ⇒ orb 子类化线程 `refresh` 后
//! 重放归位意图（窗口按新系数重写尺寸）。
//!
//! 边界：只认 100–400（超出视为坏值 ⇒ 1.0）；键不存在（用户从未改过）⇒ 1.0。

use std::sync::atomic::{AtomicU32, Ordering};

/// 缓存的百分比（启动 `refresh` 写入；`WM_SETTINGCHANGE` 再刷）。
/// 让出轮询每 35ms 取一次 scale，不能每次都读注册表。
static PERCENT: AtomicU32 = AtomicU32::new(100);

/// 百分比 → 系数：100 或缺省 ⇒ 1.0；101–400 ⇒ percent/100；其他（0 / 超范围坏值）⇒ 1.0。
pub fn factor_for(percent: u32) -> f64 {
    if (101..=400).contains(&percent) {
        f64::from(percent) / 100.0
    } else {
        1.0
    }
}

/// 当前文本缩放系数（缓存值；未 refresh 过 = 1.0）。
pub fn factor() -> f64 {
    factor_for(PERCENT.load(Ordering::Relaxed))
}

/// 读注册表刷新缓存，返回是否变化（调用方据此决定要不要重放几何）。
pub fn refresh() -> bool {
    let next = read_percent();
    let prev = PERCENT.swap(next, Ordering::Relaxed);
    crate::dev_log!("[text-scale] TextScaleFactor={next} (was {prev}) -> factor {:.2}", factor_for(next));
    prev != next
}

/// 注册表读取（仅 Windows；读不到 = 100）。
#[cfg(windows)]
fn read_percent() -> u32 {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let sub_key: Vec<u16> = r"Software\Microsoft\Accessibility".encode_utf16().chain([0]).collect();
    let value: Vec<u16> = "TextScaleFactor".encode_utf16().chain([0]).collect();
    let mut data: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub_key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut core::ffi::c_void,
            &mut size,
        )
    };
    if rc == 0 {
        data
    } else {
        100
    }
}

#[cfg(not(windows))]
fn read_percent() -> u32 {
    100
}

#[cfg(test)]
mod tests {
    use super::factor_for;

    #[test]
    fn factor_follows_percent() {
        assert_eq!(factor_for(100), 1.0);
        assert!((factor_for(150) - 1.5).abs() < 1e-9);
        assert!((factor_for(225) - 2.25).abs() < 1e-9);
    }

    #[test]
    fn factor_ignores_bad_values() {
        assert_eq!(factor_for(0), 1.0);
        assert_eq!(factor_for(50), 1.0);
        assert_eq!(factor_for(401), 1.0);
    }
}
