//! 系统托盘：「主窗口关闭 = 隐藏」的退出兜底与窗口显隐入口。
//!
//! - 菜单：挂件 / 主窗口 / 悬浮球 / 时间轴四个勾选项 + 退出（勾选态随窗口显隐实时同步）；
//! - 左键单击 = **显示主界面**（show 语义而非 toggle——挂件是常驻贴片，
//!   开关语义别扭;重复点击左键也不该把主界面藏回去）；
//! - 右键 = 菜单；
//! - 显隐反馈 = 窗口本身，不发 toast / 弹泡提示；
//! - 菜单文字随界面语言：启动读 prefs.json `locale`（缺键 = 系统界面语言），
//!   前端切换语言时经 `set_ui_locale` 即时改写（`set_text`，不重建菜单）。

use std::sync::atomic::Ordering;

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::visibility;
use crate::AppState;

const TRAY_ID: &str = "tokencalendar-tray";
const ID_TOGGLE_WIDGET: &str = "toggle_widget";
const ID_TOGGLE_MAIN: &str = "toggle_main";
const ID_TOGGLE_ORB: &str = "toggle_orb";
const ID_TOGGLE_TIMELINE: &str = "toggle_timeline";
const ID_QUIT: &str = "quit";

/// 托盘勾选项句柄：显隐变更时同步勾选态（避免重建菜单）。
pub struct TrayHandles {
    pub widget: CheckMenuItem<Wry>,
    pub main: CheckMenuItem<Wry>,
    pub orb: CheckMenuItem<Wry>,
    pub timeline: CheckMenuItem<Wry>,
    pub quit: MenuItem<Wry>,
}

/// 界面语言（与前端 `src/lib/i18n` 的 Locale 同域）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Locale {
    En,
    ZhCn,
}

impl Locale {
    pub fn parse(s: &str) -> Option<Locale> {
        match s {
            "en" => Some(Locale::En),
            "zh-CN" => Some(Locale::ZhCn),
            _ => None,
        }
    }
}

/// 托盘菜单文字：widget / main / orb / timeline / quit。
fn labels(locale: Locale) -> [&'static str; 5] {
    match locale {
        Locale::En => ["Widget", "Main window", "Orb", "Timeline", "Quit TokenCalendar"],
        Locale::ZhCn => ["挂件", "主窗口", "悬浮球", "时间轴", "退出 TokenCalendar"],
    }
}

/// prefs.json 原文 → 用户选定的语言（缺键 / 非法值 = None,即跟随系统）。
fn locale_from_prefs(json: &str) -> Option<Locale> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    Locale::parse(v.get("locale")?.as_str()?)
}

/// 系统界面语言：中文（任何地区）→ 简体中文，其余 → 英文（与前端 navigator.language 判定同口径）。
fn system_locale() -> Locale {
    #[cfg(windows)]
    {
        // LANGID 低 10 位是主语言;LANG_CHINESE = 0x04
        let lang = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
        if lang & 0x3ff == 0x04 {
            return Locale::ZhCn;
        }
    }
    Locale::En
}

fn startup_locale(app: &AppHandle) -> Locale {
    crate::data_root::current(app)
        .ok()
        .and_then(|dr| std::fs::read_to_string(dr.prefs_path()).ok())
        .as_deref()
        .and_then(locale_from_prefs)
        .unwrap_or_else(system_locale)
}


/// 前端切换界面语言（或启动时校准）:改写托盘菜单文字。
#[tauri::command]
pub fn set_ui_locale(app: AppHandle, locale: String) -> Result<(), String> {
    let locale = Locale::parse(&locale).ok_or_else(|| format!("unknown locale {locale}"))?;
    let state = app.state::<AppState>();
    let guard = state.tray.lock().unwrap();
    if let Some(h) = guard.as_ref() {
        let [w, m, o, t, q] = labels(locale);
        let _ = h.widget.set_text(w);
        let _ = h.main.set_text(m);
        let _ = h.orb.set_text(o);
        let _ = h.timeline.set_text(t);
        let _ = h.quit.set_text(q);
    }
    crate::dev_log!("[tray] locale {:?}", locale);
    Ok(())
}

pub fn init(app: &AppHandle) -> Result<(), tauri::Error> {
    let state = app.state::<AppState>();
    let locale = startup_locale(app);
    crate::dev_log!("[tray] startup locale {:?}", locale);
    let [l_widget, l_main, l_orb, l_timeline, l_quit] = labels(locale);
    let toggle_widget = CheckMenuItem::with_id(
        app,
        ID_TOGGLE_WIDGET,
        l_widget,
        true,
        state.widget_visible.load(Ordering::SeqCst),
        None::<&str>,
    )?;
    let toggle_main = CheckMenuItem::with_id(
        app,
        ID_TOGGLE_MAIN,
        l_main,
        true,
        state.main_visible.load(Ordering::SeqCst),
        None::<&str>,
    )?;
    let toggle_orb = CheckMenuItem::with_id(
        app,
        ID_TOGGLE_ORB,
        l_orb,
        true,
        state.orb_visible.load(Ordering::SeqCst),
        None::<&str>,
    )?;
    let toggle_timeline = CheckMenuItem::with_id(
        app,
        ID_TOGGLE_TIMELINE,
        l_timeline,
        true,
        state.timeline_visible.load(Ordering::SeqCst),
        None::<&str>,
    )?;
    let quit_item = MenuItem::with_id(app, ID_QUIT, l_quit, true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[&toggle_widget, &toggle_main, &toggle_orb, &toggle_timeline, &sep, &quit_item],
    )?;

    // 句柄存入 AppState，供 sync_checks 在任何显隐路径上更新勾选态
    if let Some(s) = app.try_state::<AppState>() {
        *s.tray.lock().unwrap() = Some(TrayHandles {
            widget: toggle_widget,
            main: toggle_main,
            orb: toggle_orb,
            timeline: toggle_timeline,
            quit: quit_item.clone(),
        });
    }

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(
            app.default_window_icon()
                .expect("tauri.conf.json declares bundle icons")
                .clone(),
        )
        .tooltip("TokenCalendar")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            ID_TOGGLE_WIDGET => tray_toggle(app, visibility::WIDGET_LABEL),
            ID_TOGGLE_MAIN => tray_toggle(app, visibility::MAIN_LABEL),
            ID_TOGGLE_ORB => tray_toggle(app, visibility::ORB_LABEL),
            ID_TOGGLE_TIMELINE => tray_toggle(app, visibility::TIMELINE_LABEL),
            ID_QUIT => quit_app(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event
            {
                let app = tray.app_handle();
                // 左键 = 显示主界面（show 语义:已可见时保持可,只唤回不隐藏;
                // visibility:set_visible 内部已处理 focus/重复 show 幂等）
                if let Err(e) = visibility::set_visible(app, visibility::MAIN_LABEL, true) {
                    crate::dev_log!("[tray] show main failed: {}", e);
                }
            }
        })
        .build(app)?;
    Ok(())
}

fn tray_toggle(app: &AppHandle, label: &str) {
    if let Err(e) = visibility::toggle(app, label) {
        crate::dev_log!("[tray] toggle {} failed: {}", label, e);
    }
}

/// 退出前落盘（含可见性），再退出。CloseRequested 不触发，必须手动 persist。
fn quit_app(app: &AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        crate::window_state::persist(app, &state);
    }
    app.exit(0);
}

/// 任何显隐路径之后同步托盘勾选态。
pub fn sync_checks(state: &tauri::State<'_, AppState>) {
    let guard = state.tray.lock().unwrap();
    if let Some(handles) = guard.as_ref() {
        let _ = handles
            .widget
            .set_checked(state.widget_visible.load(Ordering::SeqCst));
        let _ = handles
            .main
            .set_checked(state.main_visible.load(Ordering::SeqCst));
        let _ = handles
            .orb
            .set_checked(state.orb_visible.load(Ordering::SeqCst));
        let _ = handles
            .timeline
            .set_checked(state.timeline_visible.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_from_prefs_reads_only_known_values() {
        assert_eq!(locale_from_prefs(r#"{"locale":"zh-CN","x":1}"#), Some(Locale::ZhCn));
        assert_eq!(locale_from_prefs(r#"{"locale":"en"}"#), Some(Locale::En));
        assert_eq!(locale_from_prefs(r#"{"locale":"fr"}"#), None);
        assert_eq!(locale_from_prefs(r#"{}"#), None);
        assert_eq!(locale_from_prefs("broken"), None);
    }
}
