//! 采集器：把本机四类 AI Agent 的 token 用量聚合进 collector.db。
//!
//! 运行模型：setup 时 spawn 一个 daemon 线程——启动首轮采集,之后按采集频率增量轮询（默认 30s,设置·General 五档可选）。
//! 单源失败只降级该源状态（source_state 表）,不拖垮整体（AGENTS.md 容错铁律）。
//! UI 只读聚合结果（commands.rs）,批次提交后 emit `usage:changed`（预留链路）。
//!
//! 口径与容错设计。

pub mod claude_code;
pub mod codebuddy;
pub mod codex;
pub mod dsh;
pub mod jsonl;
pub mod project_meta;
pub mod store;
pub mod task_query;
pub mod task_store;
pub mod text;
pub mod turns;
pub mod workbuddy;
pub mod zcode;
/// 真实四源 smoke（仅测试编译,本机手动 `cargo test -- --ignored` 触发)。
#[cfg(test)]
mod smoke;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use std::time::Duration;

use chrono::{Local, TimeZone, Timelike};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;
use store::Store;

/// 单轮每文件读取字节上限（大文件分多轮推进,防内存峰值）。
pub const TAIL_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// 采集频率可选档位（秒;设置·General「Collect every」,前端 designPrefs.sanitize 同域）。
pub const POLL_INTERVAL_CHOICES_SECS: [u64; 5] = [30, 60, 120, 180, 300];
/// 默认采集频率（秒）。
pub const POLL_INTERVAL_DEFAULT_SECS: u64 = 30;
/// prefs.json 键名。
pub const POLL_INTERVAL_PREFS_KEY: &str = "collectIntervalSecs";
/// 睡眠分片:改频率后最迟一个分片即按新值判定（缩短立即生效,不必睡满旧间隔）。
const POLL_SLEEP_TICK: Duration = Duration::from_secs(1);

static POLL_INTERVAL_SECS: AtomicU64 = AtomicU64::new(POLL_INTERVAL_DEFAULT_SECS);

/// 当前采集频率。
pub fn poll_interval() -> Duration {
    Duration::from_secs(POLL_INTERVAL_SECS.load(Ordering::SeqCst))
}

/// 下发采集频率;非档位值拒绝（返回 false,运行时值不变）。
pub fn set_poll_interval_secs(secs: u64) -> bool {
    let ok = POLL_INTERVAL_CHOICES_SECS.contains(&secs);
    if ok {
        POLL_INTERVAL_SECS.store(secs, Ordering::SeqCst);
    }
    ok
}

/// prefs.json 原文 → 采集频率（缺键 / 非档位值 → None,按默认处理）。
pub fn poll_interval_from_prefs(json: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get(POLL_INTERVAL_PREFS_KEY)?.as_u64().filter(|s| POLL_INTERVAL_CHOICES_SECS.contains(s))
}

/// 把采集频率合并进 prefs.json 原文（其余键原样保留;原文缺失 / 损坏 → 以空对象起步）。
pub fn prefs_with_poll_interval(json: Option<&str>, secs: u64) -> String {
    let mut v = json
        .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    v[POLL_INTERVAL_PREFS_KEY] = serde_json::json!(secs);
    v.to_string()
}

/// 从 `since`起睡到当前采集频率;每个分片重读频率,改档即时生效。
fn sleep_until_next_round(since: Instant) {
    loop {
        let Some(left) = poll_interval().checked_sub(since.elapsed()).filter(|d| !d.is_zero()) else { return };
        std::thread::sleep(left.min(POLL_SLEEP_TICK));
    }
}


// ---------- 适配器契约 ----------

pub struct AdapterMeta {
    pub id: &'static str,
    pub name: &'static str,
    /// list_sources 展示用的路径（home 缩写）。
    pub location: &'static str,
    pub kind: &'static str,
}

pub struct ProbeOutcome {
    /// ready | no_source | unsupported_schema | busy | error
    pub status: String,
    pub fingerprint: Option<String>,
}

pub struct CollectOutcome {
    /// 实际入库事件数（0 = 无新数据）。
    pub events: u64,
    /// 涉及的数据月份（YYYY-MM），供 usage:changed 载荷。
    pub months: BTreeSet<String>,
}

pub struct AdapterError {
    /// probe_status 语义的错误码。
    pub code: String,
    pub message: String,
}

impl AdapterError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        AdapterError { code: code.to_string(), message: message.into() }
    }
}

pub type CollectResult = Result<CollectOutcome, AdapterError>;

pub trait Adapter: Send + Sync {
    fn meta(&self) -> &'static AdapterMeta;
    /// 只读探测（list_sources 实时调用,不采集不写库）。
    fn probe(&self) -> ProbeOutcome;
    /// 增量采集（采集线程调用）：读游标 → 扫数据 → store.commit（聚合+游标同事务）。
    fn collect(&self, store: &mut Store) -> CollectResult;
}

pub fn default_adapters() -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(zcode::ZcodeAdapter::new()),
        Box::new(claude_code::ClaudeCodeAdapter::new()),
        Box::new(codex::CodexAdapter::new()),
        Box::new(workbuddy::WorkBuddyAdapter::new()),
        Box::new(codebuddy::CodebuddyAdapter::new()),
        Box::new(dsh::DshAdapter::new()),
    ]
}

// ---------- 公共工具 ----------

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// RFC3339 时间串 → （本地日, 本地小时 0-23)（小时粒度）。
pub fn rfc3339_to_local_day_hour(s: &str) -> Option<(String, u8)> {
    let dt = chrono::DateTime::parse_from_rfc3339(s).ok()?.with_timezone(&Local);
    Some((dt.format("%Y-%m-%d").to_string(), dt.hour() as u8))
}

/// RFC3339 时间串 → Unix 毫秒（轮时间线用）。
pub fn rfc3339_to_millis(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.timestamp_millis())
}

/// Unix 毫秒 → 本地日（起六源全走 `millis_to_local_day_hour`,仅测试沿用）。
#[cfg(test)]
pub fn millis_to_local_day(millis: i64) -> Option<String> {
    let dt = Local.timestamp_millis_opt(millis).single()?;
    Some(dt.format("%Y-%m-%d").to_string())
}

/// Unix 毫秒 → （本地日, 本地小时 0-23)（小时粒度）。
pub fn millis_to_local_day_hour(millis: i64) -> Option<(String, u8)> {
    let dt = Local.timestamp_millis_opt(millis).single()?;
    Some((dt.format("%Y-%m-%d").to_string(), dt.hour() as u8))
}

/// 数字时间戳（秒或毫秒自适应,≤0 无效）→ 毫秒。旧 WorkBuddy 口径：> 1e10 视为毫秒。
pub fn epoch_number_to_millis(v: f64) -> Option<i64> {
    if v <= 0.0 {
        return None;
    }
    Some(if v > 1e10 { v as i64 } else { (v * 1000.0) as i64 })
}

/// 非负截断（cache/分项列可能为负的防御）。
pub fn clamp0(v: i64) -> i64 {
    v.max(0)
}

// ---------- JSONL 族公共片段 ----------

/// JSONL 文件游标（cursor_json）。baseline/model 字段仅 Codex 差分使用,其余源保持默认。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct FileCursor {
    pub offset: u64,
    pub size: u64,
    pub mtime: i64,
    #[serde(default)]
    pub base_in: i64,
    #[serde(default)]
    pub base_out: i64,
    #[serde(default)]
    pub base_cached: i64,
    #[serde(default)]
    pub base_reason: i64,
    #[serde(default)]
    pub base_total: i64,
    #[serde(default)]
    pub model: String,
    /// 对话轮 pending 标志（claude-code/workbuddy）：遇到真实用户输入行置位,
    /// 下一条带 usage 的行按其模型计一次 turn 并清位。随游标持久化,跨批次/轮次正确。
    #[serde(default)]
    pub pending_turn: bool,
    /// Codex：本文件是子代理会话（session_meta.parent_thread_id 非空）→ 不计轮。
    #[serde(default)]
    pub subagent: bool,
    /// Codex：本文件已出现过 task_started → 旧 user_message 信号不再置位（防同轮双计）。
    #[serde(default)]
    pub task_signal: bool,
    /// 轮累加器（会话 / 当前轮 / 未配对工具 / 响应去重窗口）。
    #[serde(default)]
    pub turn: turns::TurnState,
}

impl FileCursor {
    pub fn fresh() -> Self {
        FileCursor {
            offset: 0,
            size: 0,
            mtime: 0,
            base_in: 0,
            base_out: 0,
            base_cached: 0,
            base_reason: 0,
            base_total: 0,
            model: String::new(),
            pending_turn: false,
            subagent: false,
            task_signal: false,
            turn: turns::TurnState::default(),
        }
    }
    /// 文件 generation 与游标一致且已读到 EOF → 无新内容。
    pub fn up_to_date(&self, path: &Path) -> bool {
        jsonl::generation(path)
            .map(|(size, mtime)| size == self.size && mtime == self.mtime && self.offset >= size)
            .unwrap_or(false)
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

pub fn load_cursor(store: &Store, source_id: &str, scope: &str) -> FileCursor {
    store
        .get_cursor(source_id, scope)
        .and_then(|j| serde_json::from_str::<FileCursor>(&j).ok())
        .unwrap_or_else(FileCursor::fresh)
}

pub struct FileConsume {
    pub new_offset: u64,
    pub lines: Vec<String>,
    /// 截断/重写导致归零重读（调用方须重置差分基线）。
    pub reset: bool,
}

/// 推进单个 JSONL 文件：无变化 → None；正常 → 增量行；
/// 截断（size < offset）→ 归零重读。文件暂不可读（io 错误）→ None 下轮重试。
pub fn advance_file(path: &Path, cursor: &mut FileCursor) -> Option<FileConsume> {
    if cursor.up_to_date(path) {
        return None;
    }
    let consume = match jsonl::tail(path, cursor.offset, TAIL_MAX_BYTES) {
        Ok(jsonl::Tail::Advanced { new_offset, lines }) => {
            FileConsume { new_offset, lines, reset: false }
        }
        Ok(jsonl::Tail::Truncated) | Err(_) => {
            let ok = match jsonl::tail(path, 0, TAIL_MAX_BYTES) {
                Ok(jsonl::Tail::Advanced { new_offset, lines }) => {
                    FileConsume { new_offset, lines, reset: true }
                }
                _ => return None,
            };
            ok
        }
    };
    Some(consume)
}

/// 收尾一个文件：落游标进 batch（offset + generation）。
pub fn seal_cursor(cursor: &mut FileCursor, path: &Path, scope: &str, batch: &mut store::Batch) {
    if let Some((size, mtime)) = jsonl::generation(path) {
        cursor.size = size;
        cursor.mtime = mtime;
    }
    batch.cursors.push((scope.to_string(), cursor.to_json()));
}

// ---------- 后台编排 ----------

/// 打开采集库连接。应用打开两次（采集线程写连接 + 命令线程读连接），
/// SQLite WAL 模式一写多读：804MB 级首扫解析不阻塞 UI 查询。
/// 库路径 = 数据根（发布版数据架构,见 data_root.rs）。
pub fn open_store(app: &AppHandle) -> Result<Store, String> {
    let root = crate::data_root::current(app)?;
    std::fs::create_dir_all(&root.root).map_err(|e| e.to_string())?;
    Store::open(&root.db_path())
}

/// setup 时调用：采集线程**独占**写连接。线程内错误只打日志,不影响应用生命周期。
pub fn spawn(app: AppHandle, store: Store) {
    let _ = std::thread::Builder::new().name("collector".into()).spawn(move || run(app, store));
}

fn run(app: AppHandle, mut store: Store) {
    crate::dev_log!("[collector] thread started");

    // daily_project 套用的离开阈值与运行时值（prefs 载入）不一致 → 全表重算一次
    // （新库 / 迁移清库后标记缺失、上次 set_idle_threshold 重算失败,均在此自愈）。
    let threshold = task_store::idle_threshold_ms();
    if store.project_threshold_marker() != Some(threshold) {
        match store.recompute_projects(threshold) {
            Ok(n) => crate::dev_log!("[collector] daily_project recomputed for idle threshold {} ms ({} day(s))", threshold, n),
            Err(e) => crate::dev_log!("[collector] daily_project recompute failed: {}", e),
        }
    }

    let adapters = default_adapters();
    let mut first_pass = true;

    loop {
        let paused = app
            .try_state::<AppState>()
            .map(|s| s.paused.load(Ordering::SeqCst))
            .unwrap_or(false);
        if !paused {
            for adapter in &adapters {
                let meta = adapter.meta();
                match adapter.collect(&mut store) {
                    Ok(outcome) => {
                        store.record_success(meta.id, adapter.probe().fingerprint.as_deref());
                        // 有新数据或首轮完成 → 通知前端刷新
                        if outcome.events > 0 || first_pass {
                            notify_usage_changed(&app, &outcome.months);
                        }
                        if outcome.events > 0 {
                            crate::dev_log!("[collector] {} +{} events", meta.id, outcome.events);
                        }
                    }
                    Err(e) => {
                        crate::dev_log!("[collector] {} {}: {}", meta.id, e.code, e.message);
                        store.record_failure(meta.id, &e.code, &e.message);
                    }
                }
            }
            first_pass = false;
        }
        sleep_until_next_round(Instant::now());
    }
}

#[derive(Clone, Serialize)]
struct ChangedKeys {
    months: Vec<String>,
    agent_keys: Vec<String>,
    model_keys: Vec<String>,
    revision: u64,
}

/// usage:changed 订号（采集线程与命令面共用,单调递增）。
static REVISION: AtomicU64 = AtomicU64::new(0);

/// 广播 `usage:changed`（采集批次提交后;起 `set_idle_threshold` 重算后复用）。
pub fn notify_usage_changed(app: &AppHandle, months: &BTreeSet<String>) {
    let payload = ChangedKeys {
        months: months.iter().cloned().collect(),
        agent_keys: Vec::new(),
        model_keys: Vec::new(),
        revision: REVISION.fetch_add(1, Ordering::SeqCst) + 1,
    };
    if let Err(e) = app.emit("usage:changed", payload) {
        crate::dev_log!("[collector] emit usage:changed failed: {}", e);
    }
}

#[cfg(test)]
mod poll_interval_tests {
    use super::*;

    #[test]
    fn poll_interval_prefs_roundtrip_and_choices() {
        assert_eq!(poll_interval_from_prefs(r#"{"collectIntervalSecs":180,"x":1}"#), Some(180));
        assert_eq!(poll_interval_from_prefs(r#"{"collectIntervalSecs":45}"#), None, "非档位值按默认");
        assert_eq!(poll_interval_from_prefs(r#"{"collectIntervalSecs":"60"}"#), None);
        assert_eq!(poll_interval_from_prefs("bad"), None);
        let json = prefs_with_poll_interval(Some(r#"{"idleThresholdMin":45}"#), 300);
        assert!(json.contains("\"idleThresholdMin\":45") && json.contains("\"collectIntervalSecs\":300"), "{json}");
        assert!(prefs_with_poll_interval(Some("broken"), 60).contains("\"collectIntervalSecs\":60"));
        assert!(!set_poll_interval_secs(45), "非档位拒绝");
        assert_eq!(POLL_INTERVAL_CHOICES_SECS[0], POLL_INTERVAL_DEFAULT_SECS);
    }
}
