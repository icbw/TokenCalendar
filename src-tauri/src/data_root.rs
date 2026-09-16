//! 数据根目录解析。
//!
//! - **数据根** = 一切自有运行时数据（collector.db / prefs.json /
//!   window-state.json / exports）的唯一父目录。
//!   默认 `<exe 目录>\data`（安装版即 `D:\Program\TokenCalendar\data`）,
//!   首次启动时惰性创建;创建/写入失败（只读盘、绿色版放只读目录等）→
//!   回退 `%LOCALAPPDATA%\com.tokencalendar.app\`。
//! - **指针** `<anchor>\data-root.json`：用户在设置里迁移数据根后,记录新根的
//!   绝对路径;anchor 固定为 Tauri `app_local_data_dir`（永远可写,不随迁移走）。
//!   指针缺失/损坏/指向不存在目录 → 用默认根,不报错。
//! - dev（debug 构建）与安装版天然隔离：anchor 目录追加 `.dev` 后缀
//!   （`com.tokencalendar.app.dev`）,两套数据互不可。
//!
//! 路径解析在启动早期（setup 首行）做一次,结果存 AppState 供全部消费者
//! （collector / window_state / commands / prefs）取用,运行期不变——迁移
//! 命令写入新指针后提示用户重启生效。

use std::path::PathBuf;

use tauri::{AppHandle, Manager};

/// 指针文件名（位于 anchor 目录下）。
const POINTER_FILE: &str = "data-root.json";
/// 默认数据根相对 exe 的子目录名。
const DATA_DIR_NAME: &str = "data";

/// 解析结果。`root` 一定是目录形态的绝对路径;`custom` 仅诊断展示用。
#[derive(Debug, Clone)]
pub struct DataRoot {
    /// 实际生效的数据根（已确保存在;确保失败时 = fallback 根）。
    pub root: PathBuf,
    /// 用户自定义根（指针记录值;None = 用默认）。
    pub custom: Option<PathBuf>,
    /// 默认根（exe 旁 data）,诊断/「恢复默认」用。
    pub default_root: PathBuf,
    /// 生效根是否为回退根（默认根不可写 → LOCALAPPDATA anchor 自身）。
    pub fell_back: bool,
}

impl DataRoot {
    pub fn db_path(&self) -> PathBuf {
        self.root.join("collector.db")
    }
    /// 订阅快照库（与 collector.db 独立——在线账户额度 ≠ 本机用量聚合,
    /// 数据链路独立;作为数据根成员天然获得迁移/备份语义）。
    pub fn subscriptions_db_path(&self) -> PathBuf {
        self.root.join("subscriptions.db")
    }
    pub fn prefs_path(&self) -> PathBuf {
        self.root.join("prefs.json")
    }
    pub fn window_state_path(&self) -> PathBuf {
        self.root.join("window-state.json")
    }
    pub fn exports_dir(&self) -> PathBuf {
        self.root.join("exports")
    }
}

/// anchor 目录：Tauri app_local_data_dir（%LOCALAPPDATA%\<identifier>）,
/// dev 构建追加 `.dev` 后缀实现开发/安装版数据隔离。
fn anchor_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let mut dir = app.path().app_local_data_dir().map_err(|e| e.to_string())?;
    if cfg!(debug_assertions) {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        dir.set_file_name(format!("{}.dev", name));
    }
    Ok(dir)
}

/// 目录确保存在且可写（探针:建临时文件再删）。失败 = 该根不可用。
fn ensure_usable(dir: &PathBuf) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write-probe");
    match std::fs::write(&probe, b"ok") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 读指针（无/坏 JSON/非绝对路径 → None）。
fn read_pointer(anchor: &PathBuf) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(anchor.join(POINTER_FILE)).ok()?;
    let s = raw.trim().trim_matches('"').to_string();
    if s.is_empty() {
        return None;
    }
    let p = PathBuf::from(&s);
    if !p.is_absolute() {
        return None;
    }
    Some(p)
}

/// 解析数据根（setup 早期调用一次）：
/// 1. 自定义指针存在且根可用 → 用自定义根;
/// 2. 否则默认根 = exe 旁 `data\`,确保可用 → 用之;
/// 3. 仍不可用（只读安装盘等）→ 回退 anchor 目录本身（永远可写）。
pub fn resolve(app: &AppHandle) -> DataRoot {
    let anchor = match anchor_dir(app) {
        Ok(a) => a,
        Err(_) => {
            // anchor 都拿不到（理论不发生）:最后兜底 = exe 旁 data
            let fallback = default_root_of(app);
            return DataRoot { root: fallback.clone(), custom: None, default_root: fallback, fell_back: true };
        }
    };
    let _ = std::fs::create_dir_all(&anchor);

    let custom = read_pointer(&anchor).filter(|p| ensure_usable(p));
    let default_root = default_root_of(app);

    let (root, fell_back) = if let Some(c) = custom.clone() {
        (c, false)
    } else if ensure_usable(&default_root) {
        (default_root.clone(), false)
    } else {
        (anchor.clone(), true)
    };
    DataRoot { root, custom, default_root, fell_back }
}

/// 默认根 = exe 同级 `data\`（安装版 = 安装目录内,卸载器只删自装文件,数据幸存;
/// dev = target/debug/data,与安装版天然分离——anchor 层已再隔离一层）。
/// 安装协议：数据缓存跟随安装目录,用户目录只放
/// 指针/偏好等小型 JSON（anchor）。路径一律运行期由 current_exe 派生,
/// 代码中不存在任何写死的根路径。
fn default_root_of(_app: &AppHandle) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join(DATA_DIR_NAME)))
        .unwrap_or_else(|| PathBuf::from(DATA_DIR_NAME))
}

/// 解析结果一次性写入 AppState 全局（`AppState.data_root: OnceLock<DataRoot>`）。
pub fn init(app: &AppHandle) -> DataRoot {
    let dr = resolve(app);
    crate::dev_log!(
        "[data-root] root={} custom={:?} fell_back={}",
        dr.root.display(),
        dr.custom.as_ref().map(|p| p.display().to_string()),
        dr.fell_back
    );
    dr
}

/// 运行期取用（setup 后全部消费者走此路径;OnceLock 未初始化时兜底即时解析）。
pub fn current(app: &AppHandle) -> Result<DataRoot, String> {
    if let Some(state) = app.try_state::<crate::AppState>() {
        if let Some(dr) = state.data_root.get() {
            return Ok(dr.clone());
        }
    }
    Ok(resolve(app))
}

/// 写自定义根指针（迁移命令用）。空串 = 清除指针（恢复默认）。
pub fn write_pointer(app: &AppHandle, root: &PathBuf) -> Result<(), String> {
    let anchor = anchor_dir(app)?;
    std::fs::create_dir_all(&anchor).map_err(|e| e.to_string())?;
    std::fs::write(
        anchor.join(POINTER_FILE),
        root.display().to_string(),
    )
    .map_err(|e| e.to_string())
}
