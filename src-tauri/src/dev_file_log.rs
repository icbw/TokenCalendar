//! dev 文件日志。
//!
//! 设计约束：
//! - **仅 dev 构建生效**（`debug_assertions`）——release 二进制零行为差异、
//!   零文件写入（发布数据根保持干净）;
//! - 落点 = **临时测试目录** `%TEMP%\tokencalendar-dev-logs\`（不进仓库、
//!   不进数据根;跨 dev 会话可读,诊断完随手可删）;
//! - 单文件封顶 1MB,超限滚动到 .old（覆盖）——防止无界增长;
//! - 每次启动清理 7 天前的旧文件（冗余清理）;
//! - `dev_log!` 宏双写:文件 + 控制台（保持既有控制台习惯）。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const MAX_LOG_BYTES: u64 = 1_000_000; // 1MB 封顶
const KEEP_DAYS: i64 = 7;

static SINK: OnceLock<Option<Mutex<File>>> = OnceLock::new();

fn log_dir() -> Option<PathBuf> {
    let base = std::env::var_os("TEMP")?;
    let dir = PathBuf::from(base).join("tokencalendar-dev-logs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// 启动时调用（setup 首行;仅 dev）:清理旧日志 + 打开当日文件。
pub fn init() {
    let Some(dir) = log_dir() else { return };

    // 冗余清理:删 KEEP_DAYS 天前的 .log/.old 文件
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        - KEEP_DAYS * 86400;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let stale = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| (d.as_secs() as i64) < cutoff)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    let path = dir.join(format!(
        "dev-{}.log",
        chrono::Local::now().format("%Y%m%d")
    ));
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    let _ = SINK.set(file.map(Mutex::new));
}

/// 双写一行（时间戳前缀;仅 dev 构建有输出——release 内联为空,零成本）。
pub fn write_line(line: &str) {
    #[cfg(debug_assertions)]
    {
        if let Some(Some(mutex)) = SINK.get().map(|s| s.as_ref()) {
            if let Ok(mut f) = mutex.lock() {
                // 滚动:超限 → 丢弃当前文件为 .old,重开当日文件
                if f.metadata().map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
                    if let Some(dir) = log_dir() {
                        let today = format!("dev-{}", chrono::Local::now().format("%Y%m%d"));
                        let _ = std::fs::rename(
                            dir.join(format!("{today}.log")),
                            dir.join("dev-rolled.old.log"),
                        );
                        if let Ok(nf) = OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(dir.join(format!("{today}.log")))
                        {
                            *f = nf;
                        }
                    }
                }
                let stamp = chrono::Local::now().format("%H:%M:%S%.3f");
                let _ = writeln!(f, "[{stamp}] {line}");
            }
        }
        eprintln!("{line}");
    }
    #[cfg(not(debug_assertions))]
    let _ = line;
}

/// 结构化日志宏:任意表达式拼接（调用方负责不含 token/密钥——凭据安全走查门覆盖）。
#[macro_export]
macro_rules! dev_log {
    ($($arg:tt)*) => {
        $crate::dev_file_log::write_line(&format!($($arg)*))
    };
}
