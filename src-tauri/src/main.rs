// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_focus;
mod attention_watch;
mod chrome;
mod collector;
mod commands;
mod data_root;
#[cfg(debug_assertions)]
mod dev_file_log;
mod effects;
#[cfg(test)]
mod fixture;
mod orb_dock;
mod snap;
mod startup_gate;
mod subscription;
mod text_scale;
mod timeline_form;
mod tray;
mod visibility;
mod window_state;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use tauri::{Manager, WindowEvent};

use collector::store::Store;
use data_root::DataRoot;

/// 结构化 dev 日志宏：任意表达式拼接（调用方负责不含 token/密钥——凭据安全走查门覆盖）。
/// dev 构建双写文件 + 控制台；release 构建**不展开任何调用**（模块整体不进编译，
/// 记参也不参与格式化——零成本、零文件写入）。定义在 crate 根：模块被 cfg 后
/// 调用点（`crate:dev_log!`）仍处处可解析。
#[macro_export]
macro_rules! dev_log {
    ($($arg:tt)*) => {{
        #[cfg(debug_assertions)]
        {
            $crate::dev_file_log::write_line(&format!($($arg)*));
        }
        #[cfg(not(debug_assertions))]
        {
            // release：记参仍参与 `format_args!` 的编译期格式校验（不移动、不分配、
            // 不落盘——只是让内联捕获的变量保持「被使用」，避免 unused 警告）。
            let _ = format_args!($($arg)*);
        }
    }};
}

pub struct AppState {
    /// 数据根（setup 首行解析,运行期不变;全部自有数据落点经此派生）。
    pub data_root: OnceLock<DataRoot>,
    /// 采集暂停开关（起对采集轮询实际生效）。
    pub paused: AtomicBool,
    /// 可见性单一源（起取代 window_mode 互斥模式）。
    /// 初值为「首次启动」默认，setup 中由 window_state:restore 按落盘状态覆盖。
    pub widget_visible: AtomicBool,
    pub main_visible: AtomicBool,
    /// 悬浮球可见性（默认 false——新窗口形态默认不弹，托盘/设置页开启）。
    pub orb_visible: AtomicBool,
    /// 项目推进时间轴可见性（默认 false——第四窗口默认不弹，托盘/设置页开启）。
    pub timeline_visible: AtomicBool,
    /// 落盘状态是否已由 `window_state:restore` 装载（false 期间 persist 拒写——
    /// 否则会把 AppState 初值当作「当前状态」覆盖 window-state.json,下次启动悬浮球
    /// 变成「不可见 + 未贴边 + 默认位置竖条」）。
    pub window_state_restored: AtomicBool,
    /// Moved/Resized 事件节流持久化的上次写盘时刻（None = 从未写过）。
    pub last_persist: Mutex<Option<Instant>>,
    /// 主窗口最大化状态去重缓存（Resized 高频事件里只在真变时广播）。
    pub last_maximized: Mutex<bool>,
    /// 托盘勾选项句柄（tray:init 后填充，任何显隐路径同步勾选态）。
    pub tray: Mutex<Option<tray::TrayHandles>>,
    /// 命令线程的采集库**读连接**（写连接归采集线程独占,WAL 一写多读）。
    /// 打开失败时保持 None,usage 命令返回错误（前端走既有降级路径）。
    pub collector_reader: OnceLock<Arc<Mutex<Store>>>,
    /// 挂件吸附状态（None = 自由态；写者 = snap 子类化线程与
    /// restore，persist/set_snap_state 消费）。
    pub widget_snap: Mutex<Option<snap::SnapState>>,
    /// 吸附开关（应急停用杠杆,P3 设置页接线;文件缺省 = 开）。
    pub widget_snap_enabled: AtomicBool,
    /// 悬浮球贴边停靠状态（None = 自由态;写者 = orb_dock 子类化
    /// 线程与 restore/命令,persist/get_orb_form 消费）。
    pub orb_dock: Mutex<Option<orb_dock::OrbDockState>>,
    /// 悬浮球当前形态（true = 展开卡片）。Rust 侧权威副本：几何判定与
    /// 点击穿透命中不再从窗口尺寸反推（跨屏 DPI 重排会让物理尺寸推不出逻辑尺寸）。
    /// 写者 = set_orb_size 命令与 orb_dock 归位路径。
    pub orb_expanded: AtomicBool,
    /// 注意力表（内存,不落库;写者 = 采集线程每轮 tick + ack 命令）。
    pub attention: Mutex<collector::attention::AttentionTable>,
    /// 时间轴形态（看板 / 条态 + 看板几何 + 条态位置宽度;写者 = set_timeline_form
    /// 与条态拖动落定,restore 装载,persist 消费）。
    pub timeline_form: Mutex<timeline_form::FormState>,
}

fn main() {
    // 启动闸门：setup 完成前到达的前端 IPC 暂存、setup 末尾重放（见 startup_gate.rs）
    let startup_gate = startup_gate::StartupGate::new();
    let setup_gate = startup_gate.clone();
    tauri::Builder::default()
        // 单实例守卫：挂件常驻 + close=hide 场景下重复启动会堆出双份托盘/挂件；
        // 二次启动转发到首实例（把挂件唤回可）后立即退出。必须最先注册。
        // dev/安装版隔离：插件互斥体名 = `{identifier}-sim`，两构建
        // 共用 identifier 时同名——dev 常驻时双击安装版 exe 会被 FindWindowW 转发进
        // dev 进程并 exit（0)（用户看到的"安装版"实为 dev 窗口，显示 dev 数据目录）。
        // dev 经 tauri.dev.conf.json 覆盖 identifier（见 package.json dev:full），
        // 两构建互斥体/窗口类名分流，可并存。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Err(e) = visibility::set_visible(app, visibility::WIDGET_LABEL, true) {
                crate::dev_log!("[single-instance] show widget failed: {}", e);
            }
        }))
        // 目录选择器（设置·Data tab 的迁移/备份/恢复路径选择）
        .plugin(tauri_plugin_dialog::init())
        // 应用更新（设置·About：检查更新 / 自动更新）——更新源与公钥在
        // tauri.conf.json 的 plugins.updater；安装包签名校验由插件执行。
        .plugin(tauri_plugin_updater::Builder::new().build())
        // 开机自启（设置·General「Launch at login」）：系统启动项是唯一事实源，
        // 插件按 productName 读写 HKCU Run 键（与 NSIS 卸载清理的 ${PRODUCTNAME}
        // 同名，卸载即清干净）；不传附加参数——启动后落点由 window-state.json 决定。
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .manage(AppState {
            data_root: OnceLock::new(),
            paused: AtomicBool::new(false),
            widget_visible: AtomicBool::new(true),
            main_visible: AtomicBool::new(false),
            orb_visible: AtomicBool::new(false),
            timeline_visible: AtomicBool::new(false),
            window_state_restored: AtomicBool::new(false),
            last_persist: Mutex::new(None),
            last_maximized: Mutex::new(false),
            tray: Mutex::new(None),
            collector_reader: OnceLock::new(),
            widget_snap: Mutex::new(None),
            widget_snap_enabled: AtomicBool::new(true),
            orb_dock: Mutex::new(None),
            orb_expanded: AtomicBool::new(false),
            attention: Mutex::new(Default::default()),
            timeline_form: Mutex::new(Default::default()),
        })
        .invoke_handler(startup_gate.wrap(tauri::generate_handler![
            commands::get_monthly_matrix,
            commands::get_breakdown,
            commands::get_credit_summary,
            commands::get_range_series,
            commands::get_project_month_rows,
            commands::get_project_breakdown,
            commands::get_task_list,
            commands::get_task_turns,
            commands::get_effort_series,
            commands::get_gap_histogram,
            commands::get_project_span,
            commands::get_project_timeline,
            commands::get_attention,
            commands::ack_attention,
            agent_focus::focus_agent_window,
            agent_focus::open_agent_session,
            commands::list_project_meta,
            commands::set_project_meta,
            commands::merge_projects,
            commands::unmerge_projects,
            commands::get_scratch_rule,
            commands::get_collect_interval,
            commands::set_collect_interval,
            commands::set_scratch_rule,
            commands::open_project_folder,
            commands::get_idle_threshold,
            commands::set_idle_threshold,
            commands::list_sources,
            commands::get_paused,
            commands::set_paused,
            commands::export_month_csv,
            commands::export_month_json,
            commands::get_data_info,
            commands::migrate_data_root,
            commands::backup_data,
            commands::restore_data,
            commands::open_data_dir,
            commands::open_external_url,
            commands::get_prefs_raw,
            commands::set_prefs_raw,
            commands::set_widget_size,
            commands::set_orb_size,
            orb_dock::orb_undock,
            orb_dock::get_orb_form,
            commands::get_snap_enabled,
            commands::set_snap_enabled,
            commands::get_autostart,
            commands::set_autostart,
            commands::main_minimize,
            commands::main_toggle_maximize,
            commands::main_close,
            effects::set_window_material,
            visibility::window_ready,
            visibility::get_visibility,
            visibility::show_widget,
            visibility::hide_widget,
            visibility::toggle_widget,
            visibility::show_main,
            visibility::hide_main,
            visibility::toggle_main,
            visibility::show_orb,
            visibility::hide_orb,
            visibility::toggle_orb,
            visibility::show_timeline,
            visibility::hide_timeline,
            visibility::toggle_timeline,
            timeline_form::get_timeline_form,
            timeline_form::set_timeline_form,
            timeline_form::set_timeline_style,
            subscription::get_subscription_snapshots,
            subscription::scan_subscription_credentials,
            subscription::bind_subscription,
            subscription::unbind_subscription,
            subscription::refresh_subscriptions_now,
            subscription::set_subscription_poll_secs,
            subscription::boost::get_subscription_boost,
            subscription::boost::set_subscription_boost,
            subscription::idle::get_subscription_idle,
            subscription::idle::set_subscription_idle_enabled,
            subscription::idle::note_subscription_attention,
        ]))
        .setup(move |app| {
            let handle = app.handle().clone();
            // dev 文件日志最先初始化（仅 dev 构建;落 %TEMP%\tokencalendar-dev-logs\,
            // Agent 诊断可读;release 不编译此模块——零写入）
            #[cfg(debug_assertions)]
            dev_file_log::init();
            dev_log!("[boot] tokencalendar {} starting", env!("CARGO_PKG_VERSION"));
            // 数据根最先解析（全部自有数据落点的前置依赖;dev/安装版在此分流）
            let _ = handle.state::<AppState>().data_root.set(data_root::init(&handle));
            // 窗口形态一次性装配（边缘原子组合原样复用，此后不再切换）
            if let Some(w) = handle.get_webview_window("widget") {
                chrome::apply_widget_chrome(&w);
            }
            if let Some(w) = handle.get_webview_window("main") {
                chrome::apply_main_chrome(&w);
            }
            // 悬浮球：边缘处置同挂件「系统全静默 + CSS 全权」组合
            // （无材质档，组合固定不切换——）
            if let Some(w) = handle.get_webview_window("orb") {
                chrome::apply_orb_chrome(&w);
            }
            // 时间轴：边缘栈 = 挂件默认态组合（不置顶、可缩放、无材质档）
            if let Some(w) = handle.get_webview_window("timeline") {
                chrome::apply_timeline_chrome(&w);
            }
            // Windows「文本大小」感知：WebView2 把辅助功能文本缩放绑在 DPI 上
            // 整体放大内容,Win32 DPI 不知情 ⇒ 固定尺寸的悬浮球主体/让出区域与实际表盘
            // 错位（只剩左上角一道弧）。启动先读一次缓存系数,orb_dock 的物理换算乘上它
            // （必须早于 restore_orb 的归位写入）;运行时改设置由 orb 子类化的
            // WM_SETTINGCHANGE 刷新并重放（text_scale.rs）。
            text_scale::refresh();
            // 悬浮球贴边停靠：子类化 orb 窗口拦 WM_EXITSIZEMOVE
            // （松手贴缘→dock 收起/离缘→undock 展开;与 snap.rs 同通道模式,
            // 各自子类化各自窗口互不干扰）
            // ⚠ 必须排在 window_state:restore **之前**：restore_orb 的归位写入要
            // 经 apply_desired 记「意图」。
            orb_dock::install(&handle);
            // 恢复上次会话的窗口位置/尺寸与可见性标志（不直接 show，
            // 显示统一由前端首帧后的 window_ready 裁决）
            window_state::restore(&handle);
            // 时间轴上次是条态则折条（依赖 restore 装载的形态状态与 text_scale 系数）;
            // 子类化约束条态拖动（钉顶缘横向滑动）并在显示器 / 文本大小变化时重施
            timeline_form::restore(&handle);
            timeline_form::install(&handle);
            // 边缘吸附：子类化 widget 窗口拦 WM_ENTERSIZEMOVE/
            // EXITSIZEMOVE/MOVING（S1 通道探针零行为变更，无条件转发消息链）
            snap::install(&handle);
            // 最小托盘：主窗口关闭 = 隐藏后，退出兜底在此
            tray::init(&handle)?;
            // 采集器：先开库（写连接归采集线程,读连接进 AppState 供命令查询）,
            // 再 spawn 后台轮询。库打不开只降级 usage 命令,不阻断应用。
            // 离开阈值先于采集线程载入（线程启动即按它校验 daily_project）
            commands::load_idle_threshold(&handle);
            // 项目自动折叠规则（查询时解析,载入即生效）
            commands::load_scratch_rule(&handle);
            // 采集频率（设置·General Collection 组）先于采集线程载入
            commands::load_collect_interval(&handle);
            match collector::open_store(&handle) {
                Ok(write_store) => {
                    if let Ok(reader) = collector::open_store(&handle) {
                        let _ = app.state::<AppState>().collector_reader.set(Arc::new(Mutex::new(reader)));
                    }
                    collector::spawn(handle.clone(), write_store);
                    // 注意力自动退出：前台自动确认 + 快速探针（只读采样,1s 一拍）
                    attention_watch::spawn(handle.clone());
                }
                Err(e) => crate::dev_log!("[collector] init failed, usage commands will error: {}", e),
            }
            // 订阅额度轮询：独立 daemon（绑定前零网络;失败不影响应用）
            if let Err(e) = subscription::spawn(handle.clone()) {
                crate::dev_log!("[subscription] init failed, quota commands will error: {}", e);
            }
            // 状态全部就位（窗口状态 / 采集读连接 / 订阅）后才放行前端 IPC——早到的调用
            // 此刻按序重放,读到的是恢复后的状态而非 AppState 初值
            setup_gate.open();
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // 主窗口/挂件关闭 = 隐藏（保活 webview，托盘可恢复）；退出只走托盘
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let app = window.app_handle();
                let label = window.label().to_string();
                if let Err(e) = visibility::set_visible(app, &label, false) {
                    crate::dev_log!("[window] close->hide {} failed: {}", label, e);
                }
            }
            // 拖动/缩放期间高频触发：2s 节流落盘（退出与显隐变更有即时落盘兜底）
            WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                let app = window.app_handle();
                if let Some(state) = app.try_state::<AppState>() {
                    window_state::persist_throttled(app, &state);
                }
                // 自绘标题栏：最大化状态变化广播（前端圆角归零/按钮态切换）。
                // Resized 抖动去重：只在状态真变时发。
                if window.label() == "main" {
                    let maximized = window.is_maximized().unwrap_or(false);
                    let state = app.state::<AppState>();
                    let mut last = state.last_maximized.lock().unwrap();
                    if *last != maximized {
                        *last = maximized;
                        drop(last);
                        let _ = tauri::Emitter::emit(app, "main-maximized-changed", maximized);
                    }
                }
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
