//! 采集器本地存储（app_data_dir/collector.db，rusqlite bundled）。
//!
//! 事件不落明细库:契约所需的最低粒度即 `day × agent × model`（矩阵/钻取/导出全是这个粒度），
//! 用量只存聚合表;轮 / 会话另有原始层与物化层（见 task_store.rs）。
//! 幂等性由「游标与聚合同事务提交、游标严格不重复消费」保证。
//! collector.db 是用量历史唯一的持久副本:口径变更只就地升级已有行（migrations.rs）,不清库重扫。
//!
//! 聚合表与常驻表：
//! - `daily_usage` 聚合表（PRIMARY KEY 去重，upsert += 累加;含源本地 `credit` 积分）
//! - `source_cursor` 每源每 scope 的增量游标（JSON）
//! - `source_state` 每源健康状态（list_sources 的数据源）
//! - `project_meta` 项目管理映射（别名 / 隐藏 / 合并,常驻不随迁移清空,见 collector/project_meta.rs）

use std::collections::{BTreeMap, BTreeSet};

use super::turns::UNKNOWN_PROJECT;
use std::path::Path;

use chrono::NaiveDate;
use rusqlite::Connection;

/// 一次采集批次的提交物：聚合增量 + 游标推进，同事务落库。
#[derive(Default)]
pub struct Batch {
    /// （day, agent_key, model_key) → [input, output, total, turns, cache_read, cache_write] 累加量。
    /// turns = 对话轮次（request_count 列）;cache 两列是源原始口径,不参与 input/total 换算。
    pub entries: BTreeMap<(String, String, String), [i64; 6]>,
    /// （day, hour, agent_key, model_key) → [input, output, total, cache_read, cache_write] 累加量
    /// （与日聚合同事务提交;hour = 本地时 0-23）。
    pub hourly: BTreeMap<(String, u8, String, String), [i64; 5]>,
    /// （day, agent_key, model_key) → 源本地积分累加量（只由带积分的源写入,须与同格 usage 同批）。
    pub credits: BTreeMap<(String, String, String), f64>,
    /// （scope, cursor_json) 游标推进（同事务提交，防崩溃重计）。
    pub cursors: Vec<(String, String)>,
    /// 实际采集到的事件数（用于 events_collected 与 usage:changed 判定）。
    pub events: u64,
    /// （agent, session, turn_seq) → 轮现状（整行覆盖写 turn_raw / turn_part,幂等）。
    pub turns: BTreeMap<(String, String, i64), TurnRow>,
    /// （agent, session) → 会话元数据（合并写 session）。
    pub sessions: BTreeMap<(String, String), SessionRow>,
    /// 整会话重建（ZCode 按会话重算覆盖）——提交时先清该会话原始轮行。
    pub replaced_sessions: BTreeSet<(String, String)>,
    /// 已计行 （agent, 行 uuid, 归属会话)——Claude 续聊 / fork 副本文件跨文件去重的键（INSERT OR IGNORE）。
    pub seen_lines: Vec<(String, String, String)>,
    /// 会话别名 （agent, 副本文件的 sessionId, 根会话)——子会话 parent 与查询层按别名归根。
    pub session_aliases: Vec<(String, String, String)>,
    /// （agent, session) → 会话现状观测（不落库;commit 后暂存进 Store 供采集线程取走）。
    pub live: BTreeMap<(String, String), super::attention::LiveTurn>,
}

impl Batch {
    /// 快速探针：给本批该会话的观测挂上源文件与采集时 mtime（文件再变 = 会话有新动静）。
    pub fn watch_live(&mut self, session_id: &str, path: &std::path::Path, mtime: i64) {
        if session_id.is_empty() || mtime <= 0 {
            return;
        }
        for ((_, s), obs) in self.live.iter_mut() {
            if s == session_id {
                obs.watch = Some((path.display().to_string(), mtime));
            }
        }
    }

    /// `turns`：该事件是否开启一个新对话轮（0/1）。turn 由「真实用户输入后的
    /// 第一条带 usage 的行」判定（pending 标志,游标持久化）,无该概念的源传 1
    /// （zcode 走按日重算覆盖,粗值不影响权威值;codebuddy 的 request 行天然=轮）。
    /// `hour`：本地小时（0-23）;None = 该源未提供时间（只入日表）。
    pub fn add(&mut self, day: &str, agent: &str, model: &str, input: i64, output: i64, total: i64, turns: i64) {
        self.add_hour(day, None, agent, model, input, output, total, turns);
    }

    /// 带 hour 的入口。日表照常累加（日视图口径不变）,
    /// 小时表仅在有 hour 时累加。
    pub fn add_hour(
        &mut self,
        day: &str,
        hour: Option<u8>,
        agent: &str,
        model: &str,
        input: i64,
        output: i64,
        total: i64,
        turns: i64,
    ) {
        self.add_usage(day, hour, agent, model, Tokens { input, output, total, cache_read: 0, cache_write: 0 }, turns);
    }

    /// 完整入口：带 cache 分项。cache 列只做展示分段,不改 input/total 口径。
    pub fn add_usage(&mut self, day: &str, hour: Option<u8>, agent: &str, model: &str, t: Tokens, turns: i64) {
        if t.total == 0 && t.input == 0 && t.output == 0 && t.cache_read == 0 && t.cache_write == 0 {
            return;
        }
        let e = self
            .entries
            .entry((day.to_string(), agent.to_string(), model.to_string()))
            .or_insert([0; 6]);
        e[0] += t.input;
        e[1] += t.output;
        e[2] += t.total;
        e[3] += turns;
        e[4] += t.cache_read;
        e[5] += t.cache_write;
        if let Some(h) = hour {
            let he = self
                .hourly
                .entry((day.to_string(), h, agent.to_string(), model.to_string()))
                .or_insert([0; 5]);
            he[0] += t.input;
            he[1] += t.output;
            he[2] += t.total;
            he[3] += t.cache_read;
            he[4] += t.cache_write;
        }
        self.events += 1;
    }

    /// 源本地积分入账（CodeBuddy request / WorkBuddy rawUsage 行级 `credit`）。非正数忽略。
    /// 只在同一 （day, agent, model) 已有 usage 入账处调用,避免产生零 token 的孤立聚合行。
    pub fn add_credit(&mut self, day: &str, agent: &str, model: &str, credit: f64) {
        if !(credit > 0.0) || !credit.is_finite() {
            return;
        }
        *self.credits.entry((day.to_string(), agent.to_string(), model.to_string())).or_insert(0.0) += credit;
    }

    /// 写入一轮现状（同键后写覆盖先写:累加器每次 flush 都是整轮快照）。
    pub fn add_turn(&mut self, agent: &str, row: TurnRow) {
        self.turns.insert((agent.to_string(), row.session_id.clone(), row.turn_seq), row);
    }

    /// 合并会话元数据：project / parent 取首个非空,title 取最新非空,起止取 min / max。
    pub fn upsert_session(&mut self, agent: &str, row: SessionRow) {
        let key = (agent.to_string(), row.session_id.clone());
        match self.sessions.get_mut(&key) {
            None => {
                self.sessions.insert(key, row);
            }
            Some(cur) => {
                let replace = row.project_key.is_some()
                    && (row.project_authoritative || cur.project_key.as_deref().map_or(true, |p| p == UNKNOWN_PROJECT));
                if replace {
                    cur.project_key = row.project_key;
                    cur.project_authoritative |= row.project_authoritative;
                }
                if cur.parent_id.is_none() {
                    cur.parent_id = row.parent_id;
                }
                if row.title.is_some() {
                    cur.title = row.title;
                }
                cur.started_at = min_opt(cur.started_at, row.started_at);
                cur.ended_at = cur.ended_at.max(row.ended_at);
            }
        }
    }

    /// 标记整会话重建（提交时先删该会话全部 turn_raw / turn_part,再写本批行）。
    pub fn replace_session(&mut self, agent: &str, session_id: &str) {
        self.replaced_sessions.insert((agent.to_string(), session_id.to_string()));
    }
}

fn min_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// 轮的一个 （本地日, 模型) 切片:token 与调用在这里按事件自身的日 / 模型落账,
/// 保证 daily_project（折叠项目后）与 daily_usage 逐 （day, agent, model) 守恒。
/// `turn_mark` = 该切片计入 request_count 的轮数（与 daily_usage 同一次判定）。
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnPart {
    pub day: String,
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub total: i64,
    /// cache 两项落到原始层,口径变更才能只读库内原始层就地重算。
    /// **必须带 `serde（default)`**:本结构随 `TurnAcc.parts` 序列化进游标,缺字段会让旧游标
    /// 整条反序列化失败 → 该源从零重扫 → 历史整份重复入账。
    #[serde(default)]
    pub cache_read: i64,
    #[serde(default)]
    pub cache_write: i64,
    pub model_calls: i64,
    pub turn_mark: i64,
}

/// 单个会话（含子会话）的一轮自身值;tokens 由 parts 求和。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TurnRow {
    pub session_id: String,
    pub turn_seq: i64,
    /// 首条响应的本地日（无响应 = 轮首本地日）。
    pub day: String,
    pub project_key: String,
    /// 首条响应的模型（无响应 = 最近已知模型 / unknown）。
    pub model_key: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub wall_ms: Option<i64>,
    pub model_ms: Option<i64>,
    pub tool_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub gap_ms: Option<i64>,
    pub model_calls: i64,
    pub tool_calls: i64,
    /// API / 工具错误（不含用户中止）。
    pub error_count: i64,
    pub retry_count: i64,
    /// 用户主动中止（Codex turn_aborted、ZCode cancelled_by_user、DSH interrupted、
    /// 零响应即被下一次输入顶掉）;与 error_count 分列,不互相计入。
    pub aborted: bool,
    pub parts: Vec<TurnPart>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRow {
    pub session_id: String,
    pub project_key: Option<String>,
    /// project_key 来自源自己的会话记录（Codex `threads.cwd`）→ 合并与落库都后写胜;
    /// 否则沿用「首个非 unknown 胜」（轮目录推断）。
    pub project_authoritative: bool,
    pub parent_id: Option<String>,
    /// 【内容列】见 `CONTENT_COLUMNS`。
    pub title: Option<String>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
}

/// 内容列清单：仅本地可视化;任何导出 / 上报 / 云端聚合一律排除。
pub const CONTENT_COLUMNS: &[(&str, &str)] = &[("session", "title")];

/// 离开阈值**默认值**:gap ≤ 阈值才计入 idle_ms。运行时值由 designPrefs
/// `idleThresholdMin` 下发,见 `task_store:idle_threshold_ms`。
pub const IDLE_THRESHOLD_MS: i64 = 30 * 60 * 1000;

/// 一条 usage 事件的 token 分项：input = cache-exclusive、
/// output = provider 口径、total = provider total;cache_read / cache_write = 源原始
/// cache 命中 / 写入量（无该字段的源为 0）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tokens {
    pub input: i64,
    pub output: i64,
    pub total: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

pub struct StoreRow {
    pub key: String,
    pub label: String,
    /// 下标 0 = 1 号；None=未来日期，Some（0)=真实零。
    pub values: Vec<Option<i64>>,
    /// 与 values 平行的请求/对话数（未来日为 0；hover「N messages」用）。
    pub message_counts: Vec<i64>,
    pub month_total: i64,
}

pub struct StoreSlice {
    pub key: String,
    pub label: String,
    pub tokens: i64,
}

pub struct StoreBreakdownDay {
    pub day: String,
    pub slices: Option<Vec<StoreSlice>>,
}

pub struct SourceState {
    pub probe_status: String,
    pub schema_fingerprint: Option<String>,
    pub last_success_at: Option<i64>,
    pub last_attempt_at: Option<i64>,
    pub last_error_code: Option<String>,
    pub last_error_message: Option<String>,
    pub events_collected: i64,
}

// credit 月报的查询结果（经 commands.rs 映射为 serde 契约）。
/// 积分池成员（CodeBuddy / WorkBuddy 共享积分）。
pub const CREDIT_POOL_AGENTS: &[&str] = &["codebuddy", "workbuddy"];
/// 积分按模型分布的来源 agent。
pub const CREDIT_MODEL_AGENT: &str = "codebuddy";

pub struct CreditModelRow {
    pub key: String,
    pub label: String,
    pub credit: f64,
    pub requests: i64,
}

pub struct CreditDayRow {
    pub day: String,
    pub credit: f64,
}

/// credit 按模型×日序列:一个模型的逐日 credit。
pub struct CreditModelDayRow {
    pub key: String,
    pub label: String,
    /// 连续日轴（月内 1 号 → 月末/今天）上该模型的逐日 credit,缺失日补 0。
    pub by_day: Vec<CreditDayRow>,
}

pub struct CreditSummary {
    pub has_data: bool,
    pub total_credit: f64,
    pub total_requests: i64,
    pub by_model: Vec<CreditModelRow>,
    pub by_day: Vec<CreditDayRow>,
    /// 模型维逐日 credit（CodeBuddy 行口径,与 by_model 同一过滤;双组图按模型曲线用）。
    pub by_model_day: Vec<CreditModelDayRow>,
}

/// 时间范围序列:后端按 bucket 聚合的连续序列 + 系列（维度分组）。
pub struct RangeSeriesPoint {
    /// bucket key:day 粒度 = "YYYY-MM-DD";hour 粒度 = "YYYY-MM-DD HH"（HH 本地时）。
    pub bucket: String,
    /// 系列值,与 series_keys 一一对应;缺失补 0。
    pub values: Vec<i64>,
}

pub struct RangeSeries {
    /// 系列标识（= 维度 key,如 agent_key / model_key),与 point.values 对齐。
    pub series_keys: Vec<String>,
    pub series_labels: Vec<String>,
    pub points: Vec<RangeSeriesPoint>,
}

pub struct Store {
    conn: Connection,
    /// 已提交批次的会话现状观测,等采集线程 `take_live` 合入注意力表。
    live: BTreeMap<(String, String), super::attention::LiveTurn>,
    /// 已提交批次里轮结束时刻的**区间**（毫秒,（最早, 最晚);采集线程每源 collect 后
    /// `take_turn_span` 取走）——订阅侧的本地活动信号。要区间而不只要最晚一条：
    /// 最晚一条只能证明「用户刚才在工作」,而首轮回填 / 整会话重建的批次里最晚一条
    /// 往往也是新的,区间起点才能把「这批 token 是刚消耗的」与「这批是历史补导」
    /// 分开（见 subscription/demand.rs）。
    turn_span: Option<(i64, i64)>,
    /// 已提交批次的 token 明细（源 id → 模型 → [输入, 输出, 缓存读, 缓存写]）。
    /// 采集线程每源 collect 后 `take_source_usage` 取走——订阅取数时机的本地信号:
    /// 分模型是因为限流窗口按价值计,贵模型少量 token 也可能吃掉可观额度
    /// （见 subscription/cost.rs）。
    pending_usage: BTreeMap<String, BTreeMap<String, [i64; 4]>>,
}

/// 当前 schema 版本（`PRAGMA user_version`）。v13 起每次升版对应 migrations.rs 里一个就地升级步骤:
/// v13 项目归属口径、v14 正文件夹名误作项目键、v15 Claude Code total 改四项和并给原始层补 cache 列。
pub const SCHEMA_VERSION: i64 = 15;
/// 低于此版本的库走一次清库重建到此版本（结构差异逐版累积,无法就地补齐）;此后只就地迁移。
const LEGACY_RESET_VERSION: i64 = 12;

/// 清库重建时 DROP 的表清单——必须与 RESET_SCHEMA 的 CREATE 清单逐一对应
/// （由测试 `reset_drop_and_create_lists_match` 守护）。
const RESET_TABLES: &[&str] = &[
    "daily_usage",
    "hourly_usage",
    "source_cursor",
    "session",
    "turn_raw",
    "turn_part",
    "turn",
    "daily_project",
    "seen_line",
    "session_alias",
];

pub(super) const RESET_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS daily_usage (
    day               TEXT    NOT NULL,
    agent_key         TEXT    NOT NULL,
    model_key         TEXT    NOT NULL,
    input_tokens      INTEGER NOT NULL DEFAULT 0,
    output_tokens     INTEGER NOT NULL DEFAULT 0,
    total_tokens      INTEGER NOT NULL DEFAULT 0,
    request_count     INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    credit            REAL    NOT NULL DEFAULT 0,
    PRIMARY KEY (day, agent_key, model_key)
);
CREATE TABLE IF NOT EXISTS hourly_usage (
    day               TEXT    NOT NULL,
    hour              INTEGER NOT NULL,
    agent_key         TEXT    NOT NULL,
    model_key         TEXT    NOT NULL,
    input_tokens      INTEGER NOT NULL DEFAULT 0,
    output_tokens     INTEGER NOT NULL DEFAULT 0,
    total_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, hour, agent_key, model_key)
);
CREATE TABLE IF NOT EXISTS source_cursor (
    source_id   TEXT NOT NULL,
    scope       TEXT NOT NULL,
    cursor_json TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (source_id, scope)
);
-- 会话（任务）元数据,含子会话（parent_id 非空,不单独成任务）。
-- title 是唯一的【内容列】:仅本地可视化,导出 / 上报一律排除（CONTENT_COLUMNS）。
CREATE TABLE IF NOT EXISTS session (
    agent_key      TEXT NOT NULL,
    session_id     TEXT NOT NULL,
    project_key    TEXT NOT NULL,
    parent_id      TEXT,
    title          TEXT,
    started_at     INTEGER NOT NULL,
    ended_at       INTEGER,
    subagent_count INTEGER NOT NULL DEFAULT 0,
    subagent_calls INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent_key, session_id)
);
CREATE INDEX IF NOT EXISTS session_by_parent ON session (agent_key, parent_id);
-- 轮原始层（采集器写入）:每个会话（含子会话）每轮的**自身值**,累加器整行覆盖,幂等。
CREATE TABLE IF NOT EXISTS turn_raw (
    agent_key      TEXT NOT NULL,
    session_id     TEXT NOT NULL,
    turn_seq       INTEGER NOT NULL,
    day            TEXT NOT NULL,
    project_key    TEXT NOT NULL,
    model_key      TEXT NOT NULL,
    started_at     INTEGER NOT NULL,
    ended_at       INTEGER NOT NULL,
    wall_ms        INTEGER,
    model_ms       INTEGER,
    tool_ms        INTEGER,
    ttft_ms        INTEGER,
    gap_ms         INTEGER,
    model_calls    INTEGER NOT NULL DEFAULT 0,
    tool_calls     INTEGER NOT NULL DEFAULT 0,
    error_count    INTEGER NOT NULL DEFAULT 0,
    retry_count    INTEGER NOT NULL DEFAULT 0,
    aborted        INTEGER NOT NULL DEFAULT 0,
    input_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens  INTEGER NOT NULL DEFAULT 0,
    total_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent_key, session_id, turn_seq)
);
CREATE INDEX IF NOT EXISTS turn_raw_by_day ON turn_raw (agent_key, day);
-- 轮的 (本地日, 模型) 切片:token / 调用按事件自身日与模型落账（与 daily_usage 逐格守恒）。
CREATE TABLE IF NOT EXISTS turn_part (
    agent_key      TEXT NOT NULL,
    session_id     TEXT NOT NULL,
    turn_seq       INTEGER NOT NULL,
    day            TEXT NOT NULL,
    model_key      TEXT NOT NULL,
    input_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens  INTEGER NOT NULL DEFAULT 0,
    total_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    model_calls    INTEGER NOT NULL DEFAULT 0,
    turn_mark      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent_key, session_id, turn_seq, day, model_key)
);
CREATE INDEX IF NOT EXISTS turn_part_by_day ON turn_part (agent_key, day);
-- 轮级事实（任务视图读表）:只含根会话,子会话的 token / 调用 / 模型与工具时间按时间并入父轮
-- （由 turn_raw 物化重算）;gap_ms 为原始轮间空档,阈值在聚合时套用。
CREATE TABLE IF NOT EXISTS turn (
    agent_key      TEXT NOT NULL,
    session_id     TEXT NOT NULL,
    turn_seq       INTEGER NOT NULL,
    day            TEXT NOT NULL,
    project_key    TEXT NOT NULL,
    model_key      TEXT NOT NULL,
    started_at     INTEGER NOT NULL,
    ended_at       INTEGER,
    wall_ms        INTEGER,
    model_ms       INTEGER,
    tool_ms        INTEGER,
    ttft_ms        INTEGER,
    gap_ms         INTEGER,
    model_calls    INTEGER NOT NULL DEFAULT 0,
    tool_calls     INTEGER NOT NULL DEFAULT 0,
    subagent_count INTEGER NOT NULL DEFAULT 0,
    subagent_calls INTEGER NOT NULL DEFAULT 0,
    error_count    INTEGER NOT NULL DEFAULT 0,
    retry_count    INTEGER NOT NULL DEFAULT 0,
    aborted        INTEGER NOT NULL DEFAULT 0,
    input_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens  INTEGER NOT NULL DEFAULT 0,
    total_tokens   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent_key, session_id, turn_seq)
);
CREATE INDEX IF NOT EXISTS turn_by_day ON turn (day, agent_key);
-- 项目维日聚合（由 turn_raw / turn_part 按日重算覆盖;idle_ms 已套离开阈值）。
CREATE TABLE IF NOT EXISTS daily_project (
    day            TEXT NOT NULL,
    agent_key      TEXT NOT NULL,
    model_key      TEXT NOT NULL,
    project_key    TEXT NOT NULL,
    turns          INTEGER NOT NULL DEFAULT 0,
    model_calls    INTEGER NOT NULL DEFAULT 0,
    tool_calls     INTEGER NOT NULL DEFAULT 0,
    wall_ms        INTEGER NOT NULL DEFAULT 0,
    model_ms       INTEGER NOT NULL DEFAULT 0,
    tool_ms        INTEGER NOT NULL DEFAULT 0,
    idle_ms        INTEGER NOT NULL DEFAULT 0,
    subagent_calls INTEGER NOT NULL DEFAULT 0,
    error_count    INTEGER NOT NULL DEFAULT 0,
    aborted_count  INTEGER NOT NULL DEFAULT 0,
    input_tokens   INTEGER NOT NULL DEFAULT 0,
    output_tokens  INTEGER NOT NULL DEFAULT 0,
    total_tokens   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (day, agent_key, model_key, project_key)
);
-- v12:已计行（派生数据）。Claude Code 续聊 / fork 把整份历史复制进新会话文件（行 uuid 不变、sessionId 改写）,
-- 采集器按行 uuid 跨文件去重:首个带 uuid 的行已被别的会话计过 → 整个文件是该根会话的续篇。
CREATE TABLE IF NOT EXISTS seen_line (
    agent_key   TEXT NOT NULL,
    uuid        TEXT NOT NULL,
    session_id  TEXT NOT NULL,
    PRIMARY KEY (agent_key, uuid)
);
CREATE INDEX IF NOT EXISTS seen_line_by_session ON seen_line (agent_key, session_id);
-- v12:会话别名:副本文件自己的 sessionId → 根会话（子会话 parent 归根、查询层按别名归根）。
CREATE TABLE IF NOT EXISTS session_alias (
    agent_key   TEXT NOT NULL,
    alias       TEXT NOT NULL,
    root        TEXT NOT NULL,
    PRIMARY KEY (agent_key, alias)
);";


pub fn days_in_month(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let first_next = NaiveDate::from_ymd_opt(ny, nm, 1).expect("valid next month");
    (first_next - NaiveDate::from_ymd_opt(y, m, 1).expect("valid month")).num_days() as u32
}

pub(super) fn parse_month(month: &str) -> Option<(i32, u32)> {
    let (y, m) = month.split_once('-')?;
    let y: i32 = y.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if !(1..=12).contains(&m) {
        return None;
    }
    Some((y, m))
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        Self::backup_before_migration(&conn, path)?;
        Self::init(conn)
    }

    /// 测试用内存库。
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        Self::init(conn)
    }

    fn init(mut conn: Connection) -> Result<Self, String> {
        // 常驻表（迁移不清）：source_state 健康状态、
        // project_meta 项目管理映射（用户维护的元数据,清库重建后原样生效）。
        // request_model 是不再使用的 CodeBuddy 官网导出对账账本,旧库遇到即 DROP
        // （原始 xlsx 仍留在数据根 imports 目录,程序不读取）。
        // 随清库重建的表统一在 RESET_SCHEMA。
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 4000;
             CREATE TABLE IF NOT EXISTS source_state (
                 source_id           TEXT PRIMARY KEY,
                 probe_status        TEXT NOT NULL DEFAULT 'no_source',
                 schema_fingerprint  TEXT,
                 last_success_at     INTEGER,
                 last_attempt_at     INTEGER,
                 last_error_code     TEXT,
                 last_error_message  TEXT,
                 events_collected    INTEGER NOT NULL DEFAULT 0
             );
             DROP TABLE IF EXISTS request_model;
             CREATE TABLE IF NOT EXISTS project_meta (
                 project_key  TEXT PRIMARY KEY,
                 alias        TEXT,
                 hidden       INTEGER NOT NULL DEFAULT 0,
                 merged_into  TEXT,
                 note         TEXT,
                 updated_at   INTEGER NOT NULL
             );",
        )
        .map_err(|e| e.to_string())?;
        // 版本迁移:< LEGACY_RESET_VERSION 的库 DROP + CREATE 一次到该版本;
        // 之后逐版就地升级（migrations.rs,每步一个 IMMEDIATE 事务）:结构用 CREATE IF NOT EXISTS / ALTER 补齐,
        // 口径用库内原始层就地重算,源里已不存在的行原样保留。迁移前的备份由 `open` 负责。
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        if version < LEGACY_RESET_VERSION {
            let drops: String = RESET_TABLES.iter().map(|t| format!("DROP TABLE IF EXISTS {t};\n")).collect();
            conn.execute_batch(&format!(
                "BEGIN;\n{drops}{RESET_SCHEMA}\nPRAGMA user_version = {LEGACY_RESET_VERSION};\nCOMMIT;"
            ))
            .map_err(|e| e.to_string())?;
        }
        conn.execute_batch(RESET_SCHEMA).map_err(|e| e.to_string())?;
        if version == 0 {
            // 全新库:建表即当前版本,无历史行可升级
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}")).map_err(|e| e.to_string())?;
        } else {
            if version < 13 {
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|e| e.to_string())?;
                let report = super::migrations::upgrade_v13(&tx)?;
                tx.execute_batch("PRAGMA user_version = 13").map_err(|e| e.to_string())?;
                tx.commit().map_err(|e| e.to_string())?;
                crate::dev_log!("[collector] schema {} -> 13 in place: {:?}", version, report);
            }
            if version < 14 {
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|e| e.to_string())?;
                let report = super::migrations::upgrade_v14(&tx)?;
                tx.execute_batch("PRAGMA user_version = 14").map_err(|e| e.to_string())?;
                tx.commit().map_err(|e| e.to_string())?;
                crate::dev_log!("[collector] schema -> 14 in place: {:?}", report);
            }
            if version < 15 {
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|e| e.to_string())?;
                let report = super::migrations::upgrade_v15(&tx)?;
                tx.execute_batch("PRAGMA user_version = 15").map_err(|e| e.to_string())?;
                tx.commit().map_err(|e| e.to_string())?;
                crate::dev_log!("[collector] schema -> 15 in place: {:?}", report);
            }
        }
        Ok(Store { conn, live: BTreeMap::new(), turn_span: None, pending_usage: BTreeMap::new() })
    }

    /// 迁移前备份：`<库目录>/backups/collector-v<旧版本>-<时间>.db`（`VACUUM INTO`,含 WAL 里未落盘的内容）。
    /// 备份失败 → 拒绝打开（不迁移、不写库;用量命令降级,数据原样）。
    fn backup_before_migration(conn: &Connection, path: &Path) -> Result<(), String> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        if version == 0 || version >= SCHEMA_VERSION {
            return Ok(());
        }
        let dir = path.parent().map(|d| d.join("backups")).ok_or("db path has no parent")?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let dest = dir.join(format!("collector-v{version}-{stamp}.db"));
        if dest.exists() {
            return Ok(());
        }
        let sql = format!("VACUUM INTO '{}'", dest.display().to_string().replace('\'', "''"));
        conn.execute(&sql, []).map_err(|e| format!("pre-migration backup failed ({}): {e}", dest.display()))?;
        crate::dev_log!("[collector] schema v{} backed up to {}", version, dest.display());
        Ok(())
    }

    /// 同 crate 采集子模块的只读查询入口（如 `task_query`;写路径仍只走 commit）。
    pub(super) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// 同 crate 采集子模块的写事务入口（如 `project_meta` 的用户映射写入;不经 commit）。
    pub(super) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// 源侧元数据里的会话标题就地写到已有会话行（只改变了的行;不建新会话行）。返回改写行数。
    pub(super) fn sync_session_titles(&mut self, agent: &str, titles: &[(&str, &str)]) -> Result<usize, String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        let mut changed = 0;
        {
            let mut stmt = tx
                .prepare_cached("UPDATE session SET title = ?3 WHERE agent_key = ?1 AND session_id = ?2 AND title IS NOT ?3")
                .map_err(|e| e.to_string())?;
            for (id, title) in titles {
                changed += stmt.execute(rusqlite::params![agent, id, title]).map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(changed)
    }

    /// 按离开阈值重算 daily_project 全表（只读原始层 turn_raw / turn_part,不动游标）,
    /// 并落阈值标记。IMMEDIATE 事务:与采集写串行,半算状态不落盘。返回重算的 （agent, 日) 数。
    pub fn recompute_projects(&mut self, idle_threshold_ms: i64) -> Result<usize, String> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let n = super::task_store::recompute_all(&tx, idle_threshold_ms)?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(n)
    }

    /// daily_project 当前套用的离开阈值（None = 从未全表重算过,如迁移后的新库）。
    pub fn project_threshold_marker(&self) -> Option<i64> {
        super::task_store::threshold_marker(&self.conn)
    }

    /// 取走自上次调用以来已提交批次的轮结束时刻区间 （最早, 最晚)（None = 期间无轮数据）。
    pub fn take_turn_span(&mut self) -> Option<(i64, i64)> {
        self.turn_span.take()
    }

    /// 取走该源自上次调用以来已提交的分模型 token 明细（空 = 期间无新用量）。
    /// 订阅侧据此估算「大约又消耗了百分之几」并决定何时取读数（见 subscription/demand.rs）。
    pub fn take_source_usage(&mut self, source_id: &str) -> BTreeMap<String, [i64; 4]> {
        self.pending_usage.remove(source_id).unwrap_or_default()
    }

    /// 取走已提交批次的会话现状观测。
    pub fn take_live(&mut self) -> BTreeMap<(String, String), super::attention::LiveTurn> {
        std::mem::take(&mut self.live)
    }

    /// 启动播种：游标在 `since` 之后更新过、且带轮状态的会话文件（JSONL 族 + DSH）。
    /// Claude 会话族未判定的文件（只有头部元数据行）跳过——其 sessionId 不能成会话。
    /// 附带快速探针目标（游标 scope 是文件路径;DSH scope 是会话目录 + `file`）与游标记下的 mtime。
    pub fn recent_turn_states(&self, since: i64) -> Vec<(String, Option<(String, i64)>, super::turns::TurnState)> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT source_id, scope, cursor_json FROM source_cursor WHERE updated_at >= ?1 AND cursor_json LIKE '%\"turn\"%'",
        ) else {
            return Vec::new();
        };
        let Ok(rows) =
            stmt.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
        else {
            return Vec::new();
        };
        rows.flatten()
            .filter_map(|(source, scope, json)| {
                let mut v: serde_json::Value = serde_json::from_str(&json).ok()?;
                let resolved = v.get("family_resolved").and_then(|x| x.as_bool()).unwrap_or(false);
                if source == "claude-code" && !resolved {
                    return None;
                }
                let st: super::turns::TurnState = serde_json::from_value(v.get_mut("turn")?.take()).ok()?;
                let mtime = v.get("mtime").and_then(|x| x.as_i64()).filter(|m| *m > 0);
                let path = match v.get("file").and_then(|x| x.as_str()) {
                    Some(f) => std::path::Path::new(&scope).join(f).display().to_string(),
                    None => scope,
                };
                (!st.session_id.is_empty()).then(|| (source, mtime.map(|m| (path, m)), st))
            })
            .collect()
    }

    pub fn get_cursor(&self, source_id: &str, scope: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT cursor_json FROM source_cursor WHERE source_id = ?1 AND scope = ?2",
                [source_id, scope],
                |r| r.get(0),
            )
            .ok()
    }

    /// 聚合增量与游标推进同事务提交：崩溃时二者要么都在要么都不在。
    pub fn commit(&mut self, source_id: &str, batch: &Batch) -> Result<(), String> {
        let now = now_millis();
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO daily_usage (day, agent_key, model_key, input_tokens, output_tokens, total_tokens, request_count,
                                              cache_read_tokens, cache_write_tokens)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(day, agent_key, model_key) DO UPDATE SET
                        input_tokens  = input_tokens  + excluded.input_tokens,
                        output_tokens = output_tokens + excluded.output_tokens,
                        total_tokens  = total_tokens  + excluded.total_tokens,
                        request_count = request_count + excluded.request_count,
                        cache_read_tokens  = cache_read_tokens  + excluded.cache_read_tokens,
                        cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens",
                )
                .map_err(|e| e.to_string())?;
            for ((day, agent, model), [input, output, total, requests, cache_read, cache_write]) in &batch.entries {
                stmt.execute(rusqlite::params![day, agent, model, input, output, total, requests, cache_read, cache_write])
                    .map_err(|e| e.to_string())?;
            }
        }
        // 源本地积分:与日聚合同事务累加（行由上面的 usage 入账建出;孤立写入也按零 token 行落库）。
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO daily_usage (day, agent_key, model_key, credit) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(day, agent_key, model_key) DO UPDATE SET credit = credit + excluded.credit",
                )
                .map_err(|e| e.to_string())?;
            for ((day, agent, model), credit) in &batch.credits {
                stmt.execute(rusqlite::params![day, agent, model, credit]).map_err(|e| e.to_string())?;
            }
        }
        // 小时粒度:与日聚合同事务,守恒关系 hourly（日合计) == daily。
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO hourly_usage (day, hour, agent_key, model_key, input_tokens, output_tokens, total_tokens,
                                               cache_read_tokens, cache_write_tokens)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(day, hour, agent_key, model_key) DO UPDATE SET
                        input_tokens  = input_tokens  + excluded.input_tokens,
                        output_tokens = output_tokens + excluded.output_tokens,
                        total_tokens  = total_tokens  + excluded.total_tokens,
                        cache_read_tokens  = cache_read_tokens  + excluded.cache_read_tokens,
                        cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens",
                )
                .map_err(|e| e.to_string())?;
            for ((day, hour, agent, model), [input, output, total, cache_read, cache_write]) in &batch.hourly {
                stmt.execute(rusqlite::params![day, *hour as i64, agent, model, input, output, total, cache_read, cache_write])
                    .map_err(|e| e.to_string())?;
            }
        }
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO source_cursor (source_id, scope, cursor_json, updated_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(source_id, scope) DO UPDATE SET
                        cursor_json = excluded.cursor_json, updated_at = excluded.updated_at",
                )
                .map_err(|e| e.to_string())?;
            for (scope, json) in &batch.cursors {
                stmt.execute(rusqlite::params![source_id, scope, json, now])
                    .map_err(|e| e.to_string())?;
            }
        }
        if batch.events > 0 {
            tx.execute(
                "UPDATE source_state SET events_collected = events_collected + ?1 WHERE source_id = ?2",
                rusqlite::params![batch.events as i64, source_id],
            )
            .map_err(|e| e.to_string())?;
        }
        // 已计行与会话别名（派生数据,与游标同事务;重复键忽略）。
        {
            let mut stmt = tx
                .prepare_cached("INSERT OR IGNORE INTO seen_line (agent_key, uuid, session_id) VALUES (?1, ?2, ?3)")
                .map_err(|e| e.to_string())?;
            for (agent, uuid, session) in &batch.seen_lines {
                stmt.execute(rusqlite::params![agent, uuid, session]).map_err(|e| e.to_string())?;
            }
            let mut stmt = tx
                .prepare_cached("INSERT OR IGNORE INTO session_alias (agent_key, alias, root) VALUES (?1, ?2, ?3)")
                .map_err(|e| e.to_string())?;
            for (agent, alias, root) in &batch.session_aliases {
                stmt.execute(rusqlite::params![agent, alias, root]).map_err(|e| e.to_string())?;
            }
        }
        // 轮 / 会话原始层 + 物化 + 项目维重算,与聚合和游标同一事务。
        super::task_store::apply(&tx, batch)?;
        tx.commit().map_err(|e| e.to_string())?;
        // 提交成功才暂存观测（失败的批次下轮重读,观测随之重发）
        self.live.extend(batch.live.iter().map(|(k, v)| (k.clone(), v.clone())));
        let ends = batch.turns.values().map(|t| t.ended_at);
        if let (Some(first), Some(last)) = (ends.clone().min(), ends.max()) {
            self.turn_span = Some(
                self.turn_span
                    .map_or((first, last), |(f, l)| (f.min(first), l.max(last))),
            );
        }
        // 提交的分模型 token（entries 值 = [输入, 输出, 总, 轮次, 缓存读, 缓存写]）
        for ((_, _, model), v) in &batch.entries {
            if v[0] + v[1] + v[4] + v[5] == 0 {
                continue;
            }
            let slot = self
                .pending_usage
                .entry(source_id.to_string())
                .or_default()
                .entry(model.clone())
                .or_insert([0; 4]);
            slot[0] += v[0];
            slot[1] += v[1];
            slot[2] += v[4];
            slot[3] += v[5];
        }
        Ok(())
    }

    /// 某行 uuid 已被哪个会话计过（None = 未见过）。
    pub fn seen_line_session(&self, agent: &str, uuid: &str) -> Option<String> {
        self.conn
            .query_row("SELECT session_id FROM seen_line WHERE agent_key = ?1 AND uuid = ?2", [agent, uuid], |r| r.get(0))
            .ok()
    }

    /// 某会话已计过的全部行 uuid（续篇文件按此跳过复制的历史行;一个会话几千行量级）。
    pub fn seen_lines_of(&self, agent: &str, session: &str) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        if let Ok(mut stmt) = self.conn.prepare_cached("SELECT uuid FROM seen_line WHERE agent_key = ?1 AND session_id = ?2") {
            if let Ok(rows) = stmt.query_map([agent, session], |r| r.get::<_, String>(0)) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// 某会话已落库的原始轮序号（`turn_raw`,保留源给的 turn_seq;物化 `turn` 会重编号,不可用）。
    /// CodeBuddy 旧游标补账用：轮与用量同事务写入,缺轮 = 未入账。
    pub fn raw_turn_seqs(&self, agent: &str, session: &str) -> std::collections::HashSet<i64> {
        let mut out = std::collections::HashSet::new();
        if let Ok(mut stmt) = self.conn.prepare_cached("SELECT turn_seq FROM turn_raw WHERE agent_key = ?1 AND session_id = ?2") {
            if let Ok(rows) = stmt.query_map([agent, session], |r| r.get::<_, i64>(0)) {
                out.extend(rows.flatten());
            }
        }
        out
    }

    /// 副本文件 sessionId → 根会话（无别名 = 自己就是根）。
    pub fn session_root_alias(&self, agent: &str, alias: &str) -> Option<String> {
        self.conn
            .query_row("SELECT root FROM session_alias WHERE agent_key = ?1 AND alias = ?2", [agent, alias], |r| r.get(0))
            .ok()
    }

    /// 采集成功后更新源状态（probe_status=ready，清除错误）。
    pub fn record_success(&mut self, source_id: &str, fingerprint: Option<&str>) {
        let _ = self.conn.execute(
            "INSERT INTO source_state (source_id, probe_status, schema_fingerprint, last_success_at, last_attempt_at)
             VALUES (?1, 'ready', ?2, ?3, ?3)
             ON CONFLICT(source_id) DO UPDATE SET
                probe_status = 'ready',
                schema_fingerprint = COALESCE(?2, schema_fingerprint),
                last_success_at = ?3, last_attempt_at = ?3,
                last_error_code = NULL, last_error_message = NULL",
            rusqlite::params![source_id, fingerprint, now_millis()],
        );
    }

    /// 采集失败：错误码直接作为 probe_status（busy/error/no_source/unsupported_schema…）。
    pub fn record_failure(&mut self, source_id: &str, code: &str, message: &str) {
        let _ = self.conn.execute(
            "INSERT INTO source_state (source_id, probe_status, last_attempt_at, last_error_code, last_error_message)
             VALUES (?1, ?2, ?3, ?2, ?4)
             ON CONFLICT(source_id) DO UPDATE SET
                probe_status = ?2, last_attempt_at = ?3,
                last_error_code = ?2, last_error_message = ?4",
            rusqlite::params![source_id, code, now_millis(), message],
        );
    }

    /// 用源真值**覆盖**某日某 agent 的 request_count（distinct turn 不可加,行级
    /// 增量只写粗值,权威值由采集端按日重算后经此覆盖）。先清该日该 agent 全部
    /// 计数再逐 model 写入——turn 归属的 model 集合可能缩小。
    pub fn apply_turn_counts(&mut self, agent: &str, day: &str, counts: &[(String, i64)]) {
        if self
            .conn
            .execute(
                "UPDATE daily_usage SET request_count = 0 WHERE day = ?1 AND agent_key = ?2",
                rusqlite::params![day, agent],
            )
            .is_err()
        {
            return;
        }
        for (model, n) in counts {
            let _ = self.conn.execute(
                "UPDATE daily_usage SET request_count = ?1
                 WHERE day = ?2 AND agent_key = ?3 AND model_key = ?4",
                rusqlite::params![n, day, agent, model],
            );
        }
    }

    pub fn source_state(&self, source_id: &str) -> SourceState {
        let row = self
            .conn
            .query_row(
                "SELECT probe_status, schema_fingerprint, last_success_at, last_attempt_at,
                        last_error_code, last_error_message, events_collected
                 FROM source_state WHERE source_id = ?1",
                [source_id],
                |r| {
                    Ok(SourceState {
                        probe_status: r.get(0)?,
                        schema_fingerprint: r.get(1)?,
                        last_success_at: r.get(2)?,
                        last_attempt_at: r.get(3)?,
                        last_error_code: r.get(4)?,
                        last_error_message: r.get(5)?,
                        events_collected: r.get(6)?,
                    })
                },
            )
            .ok();
        row.unwrap_or(SourceState {
            probe_status: "no_source".to_string(),
            schema_fingerprint: None,
            last_success_at: None,
            last_attempt_at: None,
            last_error_code: None,
            last_error_message: None,
            events_collected: 0,
        })
    }

    /// 月度矩阵（group_by: "agent" | "model"；metric: "total" | "input" | "output"）。
    /// 键集 = 月内有记录的 key；无记录日按 null ≠ 0 语义补位；行按月总量降序。
    pub fn month_rows(&self, month: &str, group_by: &str, metric: &str, today: NaiveDate) -> Option<Vec<StoreRow>> {
        let (y, m) = parse_month(month)?;
        let first = NaiveDate::from_ymd_opt(y, m, 1)?;
        let dim = days_in_month(y, m) as usize;
        let metric_col = match metric {
            "input" => "input_tokens",
            "output" => "output_tokens",
            _ => "total_tokens",
        };
        let (key_col, label_of) = match group_by {
            "model" => ("model_key", Box::new(model_label) as Box<dyn Fn(&str) -> String + Send>),
            _ => ("agent_key", Box::new(agent_label) as Box<dyn Fn(&str) -> String + Send>),
        };
        let prefix = format!("{}-%", month);

        // 月内各 key 的每日合计与月合计一次查完（tokens 与请求数两列）
        let sql = format!(
            "SELECT {key_col} AS k, substr(day, 9) AS d, SUM({metric_col}) AS v, SUM(request_count) AS rc
             FROM daily_usage WHERE day LIKE ?1 GROUP BY k, d"
        );
        let mut daily: BTreeMap<String, Vec<(usize, i64)>> = BTreeMap::new();
        let mut reqs: BTreeMap<String, Vec<(usize, i64)>> = BTreeMap::new();
        let mut stmt = self.conn.prepare(&sql).ok()?;
        let rows = stmt
            .query_map([&prefix], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .ok()?;
        for (k, d, v, rc) in rows.flatten() {
            // d 是 "DD" 文本（substr（day, 9)），转 1-based 下标
            if let Ok(day_num) = d.parse::<usize>() {
                if day_num >= 1 && day_num <= dim {
                    daily.entry(k.clone()).or_default().push((day_num - 1, v));
                    reqs.entry(k).or_default().push((day_num - 1, rc));
                }
            }
        }
        drop(stmt);

        let mut out: Vec<StoreRow> = daily
            .into_iter()
            .map(|(k, cells)| {
                // 契约语义先铺底：过去无记录日 = Some（0)（灰格），未来 = None（透明格）
                let mut values: Vec<Option<i64>> = (0..dim)
                    .map(|i| {
                        let date = first + chrono::Duration::days(i as i64);
                        if date > today { None } else { Some(0) }
                    })
                    .collect();
                let mut message_counts = vec![0i64; dim];
                let mut month_total = 0i64;
                for (idx, v) in cells {
                    values[idx] = Some(v);
                    month_total += v;
                }
                for (idx, rc) in reqs.remove(&k).unwrap_or_default() {
                    message_counts[idx] = rc;
                }
                StoreRow { label: label_of(&k), values, message_counts, month_total, key: k }
            })
            .collect();
        out.sort_by(|a, b| b.month_total.cmp(&a.month_total).then(a.key.cmp(&b.key)));
        Some(out)
    }

    /// 行钻取：kind="agent" → 每日模型构成；kind="model" → 每日 Agent 构成。
    /// 未来日期不产出 breakdown day。
    pub fn breakdown(&self, kind: &str, key: &str, month: &str, today: NaiveDate) -> Option<Vec<StoreBreakdownDay>> {
        let (y, m) = parse_month(month)?;
        let first = NaiveDate::from_ymd_opt(y, m, 1)?;
        let dim = days_in_month(y, m);
        let prefix = format!("{}-%", month);
        let (filter_col, slice_col) = match kind {
            "model" => ("model_key", "agent_key"),
            _ => ("agent_key", "model_key"),
        };
        let sql = format!(
            "SELECT day, {slice_col} AS k, SUM(total_tokens) AS v
             FROM daily_usage WHERE {filter_col} = ?1 AND day LIKE ?2
             GROUP BY day, k ORDER BY day"
        );
        let mut per_day: BTreeMap<String, Vec<(String, i64)>> = BTreeMap::new();
        let mut stmt = self.conn.prepare(&sql).ok()?;
        let rows = stmt
            .query_map(rusqlite::params![key, prefix], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .ok()?;
        for (day, k, v) in rows.flatten() {
            per_day.entry(day).or_default().push((k, v));
        }
        drop(stmt);

        let mut out = Vec::with_capacity(dim as usize);
        for d in 0..dim {
            let date = first + chrono::Duration::days(d as i64);
            if date > today {
                break;
            }
            let day_key = date.format("%Y-%m-%d").to_string();
            let mut slices: Vec<StoreSlice> = per_day
                .remove(&day_key)
                .map(|cells| {
                    cells
                        .into_iter()
                        .filter(|(_, v)| *v > 0)
                        .map(|(k, v)| StoreSlice { label: match kind { "model" => agent_label(&k), _ => model_label(&k) }, key: k, tokens: v })
                        .collect()
                })
                .unwrap_or_default();
            slices.sort_by(|a, b| b.tokens.cmp(&a.tokens));
            out.push(StoreBreakdownDay {
                day: day_key,
                slices: if slices.is_empty() { None } else { Some(slices) },
            });
        }
        Some(out)
    }

    /// 备份快照：SQLite `VACUUM INTO`（运行中安全的一致性导出,含 WAL 内容合并）。
    pub fn vacuum_into(&self, dest: &Path) -> Result<(), String> {
        if dest.exists() {
            return Err(format!("backup target exists: {}", dest.display()));
        }
        let sql = format!("VACUUM INTO '{}'", dest.display().to_string().replace('\'', "''"));
        self.conn.execute(&sql, []).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 导出明细（聚合粒度即契约明细粒度）：day, agent, model, total>0。
    pub fn export_rows(&self, month: &str) -> Result<Vec<(String, String, String, i64)>, String> {
        let prefix = format!("{}-%", month);
        const SQL: &str = "SELECT day, agent_key, model_key, total_tokens FROM daily_usage
                 WHERE day LIKE ?1 AND total_tokens > 0 ORDER BY day, agent_key, model_key";
        // 内容列排除:导出 SQL 不得触碰 CONTENT_COLUMNS 所在表。
        debug_assert!(CONTENT_COLUMNS.iter().all(|(table, _)| !SQL.split_whitespace().any(|w| w == *table)));
        let mut stmt = self
            .conn
            .prepare(SQL)
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([&prefix], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
            })
            .map_err(|e| e.to_string())?;
        Ok(rows.flatten().collect())
    }

    /// 时间范围序列（后端聚合）。
    ///
    /// - bucket: "day"（daily_usage 表）| "hour"（hourly_usage 表,bucket = day+HH）
    /// - dimension: "agent" | "model" | "total"（单系列全合计）
    /// - metric: "total" | "input" | "output"
    /// - filter: 可选维度筛选（Some（（dim,key)) 只保留该维度指定 key 的行;
    ///   dim 必须与聚合维度同族——agent 维可用 agent 筛,model 维可用 model 筛;
    ///   跨族筛选在 SQL 里同样成立（如 model 维度只看 zcode 的系列构成）。
    /// - range: [start_day, end_day] 闭区间（"YYYY-MM-DD"）
    ///
    /// 每系列在范围内连续补零（含全零中间日）,前端不重拼;系列按范围内总量降序。
    pub fn range_series(
        &self,
        start_day: &str,
        end_day: &str,
        bucket: &str,
        dimension: &str,
        metric: &str,
        filter: Option<(&str, &str)>,
    ) -> Option<RangeSeries> {
        let (table, bucket_expr) = match bucket {
            "hour" => ("hourly_usage", "day || ' ' || printf('%02d', hour)"),
            _ => ("daily_usage", "day"),
        };
        let metric_col = match metric {
            "input" => "input_tokens",
            "output" => "output_tokens",
            _ => "total_tokens",
        };
        // 聚合维度与筛选谓词相互独立:任一维都可作为系列轴,另一维用 filter 收窄。
        let (key_col, label_of): (&str, Box<dyn Fn(&str) -> String + Send>) = match dimension {
            "model" => ("model_key", Box::new(model_label)),
            "total" => ("'__total__'", Box::new(|_| "全部".to_string())),
            "agent" => ("agent_key", Box::new(agent_label)),
            _ => return None,
        };
        let mut where_clauses: Vec<String> = vec!["day >= ?1".into(), "day <= ?2".into()];
        let mut params: Vec<String> = vec![start_day.to_string(), end_day.to_string()];
        if let Some((fdim, fkey)) = filter {
            let col = match fdim {
                "model" => "model_key",
                "agent" => "agent_key",
                _ => return None,
            };
            where_clauses.push(format!("{col} = ?{}", params.len() + 1));
            params.push(fkey.to_string());
        }
        let sql = format!(
            "SELECT {key_col} AS k, {bucket_expr} AS b, SUM({metric_col}) AS v
             FROM {table}
             WHERE {} AND {metric_col} > 0
             GROUP BY k, b ORDER BY b",
            where_clauses.join(" AND ")
        );

        let mut per_series: BTreeMap<String, Vec<(String, i64)>> = BTreeMap::new();
        let mut stmt = self.conn.prepare(&sql).ok()?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })
            .ok()?;
        for (k, b, v) in rows.flatten() {
            per_series.entry(k).or_default().push((b, v));
        }
        drop(stmt);

        // 连续 bucket 轴:day 按日历推进;hour 按小时推进（范围过长时由调用方限定了粒度）。
        let axis: Vec<String> = match bucket {
            "hour" => {
                let start = NaiveDate::parse_from_str(start_day, "%Y-%m-%d").ok()?;
                let end = NaiveDate::parse_from_str(end_day, "%Y-%m-%d").ok()?;
                // 逐日 × 24 小时展开（hour 恒 0-23,不走 datetime 解析避免格式歧义）
                let mut out = Vec::new();
                let mut cur = start;
                while cur <= end {
                    for h in 0..24u32 {
                        out.push(format!("{} {h:02}", cur.format("%Y-%m-%d")));
                    }
                    cur += chrono::Duration::days(1);
                }
                out
            }
            _ => {
                let start = NaiveDate::parse_from_str(start_day, "%Y-%m-%d").ok()?;
                let end = NaiveDate::parse_from_str(end_day, "%Y-%m-%d").ok()?;
                let mut cur = start;
                let mut out = Vec::new();
                while cur <= end {
                    out.push(cur.format("%Y-%m-%d").to_string());
                    cur += chrono::Duration::days(1);
                }
                out
            }
        };

        // 系列按范围总量降序;total 维恒单系列（即使无数据也补基线）。
        let mut series: Vec<(String, i64)> = per_series
            .iter()
            .map(|(k, cells)| (k.clone(), cells.iter().map(|(_, v)| v).sum()))
            .collect();
        series.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        if dimension == "total" {
            series = vec![("__total__".to_string(), series.first().map(|(_, v)| *v).unwrap_or(0))];
        }

        let series_keys: Vec<String> = series.iter().map(|(k, _)| k.clone()).collect();
        let series_labels = series_keys.iter().map(|k| label_of(k)).collect();

        let points = axis
            .iter()
            .map(|b| {
                let values = series_keys
                    .iter()
                    .map(|k| {
                        per_series
                            .get(k)
                            .and_then(|cells| cells.iter().find(|(cb, _)| cb == b))
                            .map(|(_, v)| *v)
                            .unwrap_or(0)
                    })
                    .collect();
                RangeSeriesPoint { bucket: b.clone(), values }
            })
            .collect();

        Some(RangeSeries { series_keys, series_labels, points })
    }

    /// credit 月报（读 daily_usage 的源本地积分）。
    ///
    /// 口径：
    /// - 月度总量 / 按日走势 = `CREDIT_POOL_AGENTS`（CodeBuddy + WorkBuddy 共享积分池）;
    /// - 按模型分布 / 模型×日 = 只取 `CREDIT_MODEL_AGENT`（CodeBuddy;WorkBuddy 模型维由其矩阵行展示）;
    /// - requests = 带积分格的 request_count 之和（对话轮次口径）;
    /// - has_data = 该月池内是否有任何积分（无积分月份 UI 走空态,不渲染 0）。
    pub fn credit_summary(&self, month: &str) -> Option<CreditSummary> {
        parse_month(month)?;
        let prefix = format!("{}-%", month);
        let pool = CREDIT_POOL_AGENTS.iter().map(|a| format!("'{a}'")).collect::<Vec<_>>().join(", ");

        let (total_requests, total_credit) = self
            .conn
            .query_row(
                &format!(
                    "SELECT COALESCE(SUM(request_count), 0), COALESCE(SUM(credit), 0) FROM daily_usage
                     WHERE day LIKE ?1 AND agent_key IN ({pool}) AND credit > 0"
                ),
                [&prefix],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)),
            )
            .unwrap_or((0, 0.0));
        let has_data = total_credit > 0.0;

        // 按模型分布:CodeBuddy 行;unknown 归组展示不丢弃。
        let mut by_model: Vec<CreditModelRow> = Vec::new();
        if has_data {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT model_key, COALESCE(SUM(request_count), 0), COALESCE(SUM(credit), 0) FROM daily_usage
                     WHERE day LIKE ?1 AND agent_key = ?2 AND credit > 0
                     GROUP BY model_key ORDER BY 3 DESC",
                )
                .ok()?;
            let rows = stmt
                .query_map(rusqlite::params![&prefix, CREDIT_MODEL_AGENT], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?))
                })
                .ok()?;
            for (k, n, c) in rows.flatten() {
                by_model.push(CreditModelRow { label: model_label(&k), key: k, credit: c, requests: n });
            }
        }

        // 按日走势:积分池（与月度总量同口径）。
        let mut by_day: Vec<CreditDayRow> = Vec::new();
        if has_data {
            let mut stmt = self
                .conn
                .prepare(&format!(
                    "SELECT day, COALESCE(SUM(credit), 0) FROM daily_usage
                     WHERE day LIKE ?1 AND agent_key IN ({pool}) AND credit > 0 GROUP BY day ORDER BY day"
                ))
                .ok()?;
            let rows = stmt
                .query_map([&prefix], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))
                .ok()?;
            for (d, c) in rows.flatten() {
                by_day.push(CreditDayRow { day: d, credit: c });
            }
        }

        // 按模型×日:与 by_model 同一口径。
        // 只有有积分的日;连续日轴由调用方（命令层）补零。
        let mut by_model_day: Vec<CreditModelDayRow> = Vec::new();
        if has_data {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT model_key, day, COALESCE(SUM(credit), 0) FROM daily_usage
                     WHERE day LIKE ?1 AND agent_key = ?2 AND credit > 0
                     GROUP BY model_key, day ORDER BY model_key, day",
                )
                .ok()?;
            let rows = stmt
                .query_map(rusqlite::params![&prefix, CREDIT_MODEL_AGENT], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?))
                })
                .ok()?;
            let mut per_model: BTreeMap<String, Vec<CreditDayRow>> = BTreeMap::new();
            for (k, d, c) in rows.flatten() {
                per_model.entry(k).or_default().push(CreditDayRow { day: d, credit: c });
            }
            for (k, cells) in per_model {
                let label = model_label(&k);
                by_model_day.push(CreditModelDayRow { key: k, label, by_day: cells });
            }
        }

        Some(CreditSummary {
            has_data,
            total_credit,
            total_requests,
            by_model,
            by_day,
            by_model_day,
        })
    }

    /// 某个 agent 在 `[from_day, to_day]`（本地日,闭区间）内的
    /// **分模型 × 小时**用量四项 `[输入, 输出, 缓存读, 缓存写]`。
    ///
    /// 订阅侧的价目按**时刻**取（`price:price_at`）,所以这里给到小时而不是天：
    /// 模型降价的分界线落在哪一刻就从哪一刻切开,不需要「这一天算旧价还是新价」的人为规则。
    /// 小时表与日表在 codex / claude-code 两源上逐位相等,用它不损失量。
    pub fn model_usage_hours(
        &self,
        agent_key: &str,
        from_day: &str,
        to_day: &str,
    ) -> Vec<(String, u8, String, [i64; 4])> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT day, hour, model_key, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens
               FROM hourly_usage
              WHERE agent_key = ?1 AND day >= ?2 AND day <= ?3
              ORDER BY day, hour, model_key",
        ) else {
            return vec![];
        };
        let it = stmt.query_map(rusqlite::params![agent_key, from_day, to_day], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?.clamp(0, 23) as u8,
                r.get::<_, String>(2)?,
                [r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?],
            ))
        });
        it.map(|rs| rs.flatten().collect()).unwrap_or_default()
    }

    /// 同区间的**分模型用户轮次**（`daily_usage.request_count` 口径
    /// = 用户发起的对话轮次,不是模型调用 / 工具调用）。
    ///
    /// 只到天——小时表里没有这一列。所以它**不按价目段切分**,只挂在模型上
    /// （切分一天的轮次得靠按 token 比例摊,那是造数据）。
    pub fn model_requests(
        &self,
        agent_key: &str,
        from_day: &str,
        to_day: &str,
    ) -> Vec<(String, i64)> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT model_key, COALESCE(SUM(request_count), 0)
               FROM daily_usage
              WHERE agent_key = ?1 AND day >= ?2 AND day <= ?3
              GROUP BY model_key ORDER BY model_key",
        ) else {
            return vec![];
        };
        let it = stmt.query_map(rusqlite::params![agent_key, from_day, to_day], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        });
        it.map(|rs| rs.flatten().collect()).unwrap_or_default()
    }

    /// 该 agent 有用量记录的日期跨度（`None` = 一条都没有）。
    /// 查询面用它把「区间没给」折成「全部历史」。
    pub fn agent_day_span(&self, agent_key: &str) -> Option<(String, String)> {
        self.conn
            .query_row(
                "SELECT MIN(day), MAX(day) FROM daily_usage WHERE agent_key = ?1",
                [agent_key],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .ok()
            .and_then(|(a, b)| Some((a?, b?)))
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct TestTurn {
    pub session_id: String,
    pub turn_seq: i64,
    pub day: String,
    pub project_key: String,
    pub model_key: String,
    pub wall_ms: Option<i64>,
    pub model_ms: Option<i64>,
    pub tool_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub gap_ms: Option<i64>,
    pub model_calls: i64,
    pub tool_calls: i64,
    pub subagent_count: i64,
    pub subagent_calls: i64,
    pub error_count: i64,
    pub retry_count: i64,
    pub aborted: bool,
    pub total_tokens: i64,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct TestSession {
    pub session_id: String,
    pub project_key: String,
    pub parent_id: Option<String>,
    pub title: Option<String>,
    pub subagent_count: i64,
    pub subagent_calls: i64,
}

#[cfg(test)]
impl Store {
    /// 物化 turn 表（根会话）按会话 / 开始时间排序。
    pub fn test_turns(&self, agent: &str) -> Vec<TestTurn> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session_id, turn_seq, day, project_key, model_key, wall_ms, model_ms, tool_ms, ttft_ms, gap_ms,
                        model_calls, tool_calls, subagent_count, subagent_calls, error_count, retry_count, total_tokens, aborted
                 FROM turn WHERE agent_key = ?1 ORDER BY session_id, started_at, turn_seq",
            )
            .unwrap();
        stmt.query_map([agent], |r| {
            Ok(TestTurn {
                session_id: r.get(0)?,
                turn_seq: r.get(1)?,
                day: r.get(2)?,
                project_key: r.get(3)?,
                model_key: r.get(4)?,
                wall_ms: r.get(5)?,
                model_ms: r.get(6)?,
                tool_ms: r.get(7)?,
                ttft_ms: r.get(8)?,
                gap_ms: r.get(9)?,
                model_calls: r.get(10)?,
                tool_calls: r.get(11)?,
                subagent_count: r.get(12)?,
                subagent_calls: r.get(13)?,
                error_count: r.get(14)?,
                retry_count: r.get(15)?,
                total_tokens: r.get(16)?,
                aborted: r.get::<_, i64>(17)? != 0,
            })
        })
        .unwrap()
        .flatten()
        .collect()
    }

    pub fn test_sessions(&self, agent: &str) -> Vec<TestSession> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session_id, project_key, parent_id, title, subagent_count, subagent_calls
                 FROM session WHERE agent_key = ?1 ORDER BY session_id",
            )
            .unwrap();
        stmt.query_map([agent], |r| {
            Ok(TestSession {
                session_id: r.get(0)?,
                project_key: r.get(1)?,
                parent_id: r.get(2)?,
                title: r.get(3)?,
                subagent_count: r.get(4)?,
                subagent_calls: r.get(5)?,
            })
        })
        .unwrap()
        .flatten()
        .collect()
    }

    /// 任务列表口径:根会话（parent_id 为空）且有轮。
    pub fn test_task_sessions(&self, agent: &str) -> Vec<String> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT s.session_id FROM session s
                 WHERE s.agent_key = ?1 AND s.parent_id IS NULL
                   AND EXISTS (SELECT 1 FROM turn t WHERE t.agent_key = s.agent_key AND t.session_id = s.session_id)
                 ORDER BY s.started_at, s.session_id",
            )
            .unwrap();
        stmt.query_map([agent], |r| r.get::<_, String>(0)).unwrap().flatten().collect()
    }

    /// 守恒断言:daily_project 折叠 project_key 后逐 （day, agent, model) 与 daily_usage 的
    /// total_tokens / request_count 相等（双向全连接）。返回不一致描述,空 = 守恒。
    pub fn test_project_conservation(&self) -> Vec<String> {
        let sql = "
            WITH u AS (SELECT day, agent_key, model_key, SUM(total_tokens) t, SUM(request_count) n FROM daily_usage GROUP BY 1, 2, 3),
                 p AS (SELECT day, agent_key, model_key, SUM(total_tokens) t, SUM(turns) n FROM daily_project GROUP BY 1, 2, 3),
                 k AS (SELECT day, agent_key, model_key FROM u UNION SELECT day, agent_key, model_key FROM p)
            SELECT k.day, k.agent_key, k.model_key, COALESCE(u.t, 0), COALESCE(p.t, 0), COALESCE(u.n, 0), COALESCE(p.n, 0)
            FROM k LEFT JOIN u USING (day, agent_key, model_key) LEFT JOIN p USING (day, agent_key, model_key)
            WHERE COALESCE(u.t, 0) <> COALESCE(p.t, 0) OR COALESCE(u.n, 0) <> COALESCE(p.n, 0)
            ORDER BY 1, 2, 3";
        let mut stmt = self.conn.prepare(sql).unwrap();
        stmt.query_map([], |r| {
            Ok(format!(
                "{} {} {}: tokens daily={} project={} | turns daily={} project={}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?
            ))
        })
        .unwrap()
        .flatten()
        .collect()
    }

    /// smoke 诊断:（原始轮数, 根会话物化轮数, 负 wall 行数, 子会话数, 任务数)。
    pub fn test_task_stats(&self, agent: &str) -> (i64, i64, i64, i64, i64) {
        let q = |sql: &str| self.conn.query_row(sql, [agent], |r| r.get::<_, i64>(0)).unwrap_or(-1);
        (
            q("SELECT COUNT(*) FROM turn_raw WHERE agent_key = ?1"),
            q("SELECT COUNT(*) FROM turn WHERE agent_key = ?1"),
            q("SELECT COUNT(*) FROM turn_raw WHERE agent_key = ?1 AND (wall_ms < 0 OR model_ms < 0 OR tool_ms < 0 OR gap_ms < 0)"),
            q("SELECT COUNT(*) FROM session WHERE agent_key = ?1 AND parent_id IS NOT NULL"),
            self.test_task_sessions(agent).len() as i64,
        )
    }

    /// smoke 诊断:月内 （request_count Σ, 物化轮行数, 零调用轮, Σ error_count)。
    pub fn test_month_turn_stats(&self, agent: &str, month: &str) -> (i64, i64, i64, i64) {
        let prefix = if month.is_empty() { "%".to_string() } else { format!("{month}-%") };
        let q = |sql: &str| self.conn.query_row(sql, rusqlite::params![agent, prefix], |r| r.get::<_, i64>(0)).unwrap_or(-1);
        (
            q("SELECT COALESCE(SUM(request_count), 0) FROM daily_usage WHERE agent_key = ?1 AND day LIKE ?2"),
            q("SELECT COUNT(*) FROM turn WHERE agent_key = ?1 AND day LIKE ?2"),
            q("SELECT COUNT(*) FROM turn WHERE agent_key = ?1 AND day LIKE ?2 AND model_calls = 0"),
            q("SELECT COALESCE(SUM(error_count), 0) FROM turn WHERE agent_key = ?1 AND day LIKE ?2"),
        )
    }

    /// smoke 诊断:物化轮 （中止轮数, 带错误轮数, Σerror, 既中止又带错误的轮数)。
    pub fn test_abort_error_stats(&self, agent: &str) -> (i64, i64, i64, i64) {
        let q = |sql: &str| self.conn.query_row(sql, [agent], |r| r.get::<_, i64>(0)).unwrap_or(-1);
        (
            q("SELECT COUNT(*) FROM turn WHERE agent_key = ?1 AND aborted = 1"),
            q("SELECT COUNT(*) FROM turn WHERE agent_key = ?1 AND error_count > 0"),
            q("SELECT COALESCE(SUM(error_count), 0) FROM turn WHERE agent_key = ?1"),
            q("SELECT COUNT(*) FROM turn WHERE agent_key = ?1 AND aborted = 1 AND error_count > 0"),
        )
    }

    /// 诊断:（已计行数, 会话别名数 = 被折进根会话的副本文件数)。
    pub fn test_family_stats(&self, agent: &str) -> (i64, i64) {
        let q = |sql: &str| self.conn.query_row(sql, [agent], |r| r.get::<_, i64>(0)).unwrap_or(-1);
        (
            q("SELECT COUNT(*) FROM seen_line WHERE agent_key = ?1"),
            q("SELECT COUNT(*) FROM session_alias WHERE agent_key = ?1"),
        )
    }

    /// 子会话出现在任务列表里的个数（应恒为 0）。
    pub fn test_child_sessions_in_tasks(&self, agent: &str) -> i64 {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM turn t JOIN session s ON s.agent_key = t.agent_key AND s.session_id = t.session_id
                 WHERE t.agent_key = ?1 AND s.parent_id IS NOT NULL",
                [agent],
                |r| r.get(0),
            )
            .unwrap_or(-1)
    }

    /// smoke 诊断:月内每 agent 的 （cache_read, cache_write, hourly_total, daily_total)。
    pub fn cache_totals(&self, month: &str) -> Vec<(String, i64, i64, i64, i64)> {
        let prefix = format!("{month}-%");
        let mut stmt = self
            .conn
            .prepare(
                "SELECT d.agent_key, SUM(d.cache_read_tokens), SUM(d.cache_write_tokens), SUM(d.total_tokens),
                        (SELECT COALESCE(SUM(h.total_tokens), 0) FROM hourly_usage h WHERE h.agent_key = d.agent_key AND h.day LIKE ?1)
                 FROM daily_usage d WHERE d.day LIKE ?1 GROUP BY d.agent_key",
            )
            .unwrap();
        stmt.query_map([&prefix], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(4)?, r.get(3)?)))
            .unwrap()
            .flatten()
            .collect()
    }
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// agent_key → 展示名（源固定映射，未知 key 原样返回）。
pub fn agent_label(key: &str) -> String {
    match key {
        "zcode" => "ZCode".to_string(),
        "workbuddy" => "WorkBuddy".to_string(),
        "claude-code" => "Claude Code".to_string(),
        "codex" => "Codex".to_string(),
        "codebuddy" => "CodeBuddy".to_string(),
        "dsh" => "DeepSeek Harness".to_string(),
        other => other.to_string(),
    }
}

/// model_key → 展示名（未知模型键原样返回;unknown 是模型缺失的兜底行）。
pub fn model_label(key: &str) -> String {
    match key {
        "unknown" => "Unknown".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn insert(conn: &mut Store, day: &str, agent: &str, model: &str, i: i64, o: i64, t: i64) {
        let mut batch = Batch::default();
        batch.add(day, agent, model, i, o, t, 1);
        conn.commit("test", &batch).unwrap();
    }

    const TODAY: NaiveDate = NaiveDate::from_ymd_opt(2026, 9, 6).unwrap();

    #[test]
    fn upsert_accumulates() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "glm-5.3", 10, 5, 15);
        insert(&mut s, "2026-09-01", "zcode", "glm-5.3", 1, 2, 3);
        let rows = s.month_rows("2026-09", "agent", "total", TODAY).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "zcode");
        assert_eq!(rows[0].values[0], Some(18));
        assert_eq!(rows[0].month_total, 18);
        // 每次 add 计一次请求；两次 add 同一格 → request_count=2
        assert_eq!(rows[0].message_counts[0], 2);
        assert_eq!(rows[0].message_counts.len(), rows[0].values.len());
    }

    #[test]
    fn matrix_null_semantics() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-05", "codex", "gpt-5", 7, 3, 10);
        let rows = s.month_rows("2026-09", "agent", "total", TODAY).unwrap();
        let r = &rows[0];
        assert_eq!(r.values.len(), 30);
        // 过去无记录日 = Some(0)，今天 = Some(0)，未来 = None
        assert_eq!(r.values[0], Some(0));
        assert_eq!(r.values[4], Some(10));
        assert_eq!(r.values[5], Some(0)); // 9-6 = today
        assert!(r.values[6].is_none());
    }

    #[test]
    fn metric_columns() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "m", 30, 12, 42);
        for (metric, expect) in [("total", 42), ("input", 30), ("output", 12)] {
            let rows = s.month_rows("2026-09", "agent", metric, TODAY).unwrap();
            assert_eq!(rows[0].values[0], Some(expect), "metric={}", metric);
        }
    }

    #[test]
    fn agent_model_rows_conserve() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "a", 10, 5, 15);
        insert(&mut s, "2026-09-01", "codex", "a", 4, 1, 5);
        insert(&mut s, "2026-09-01", "zcode", "b", 2, 3, 5);
        let agents = s.month_rows("2026-09", "agent", "total", TODAY).unwrap();
        let models = s.month_rows("2026-09", "model", "total", TODAY).unwrap();
        let sa: i64 = agents.iter().map(|r| r.month_total).sum();
        let sm: i64 = models.iter().map(|r| r.month_total).sum();
        assert_eq!(sa, sm);
        assert_eq!(sa, 25);
        // 降序排列
        assert!(agents.windows(2).all(|w| w[0].month_total >= w[1].month_total));
    }

    #[test]
    fn breakdown_conserves_cell() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "glm", 10, 5, 15);
        insert(&mut s, "2026-09-01", "zcode", "gpt", 0, 0, 7);
        insert(&mut s, "2026-09-02", "zcode", "glm", 1, 1, 2);
        let agents = s.month_rows("2026-09", "agent", "total", TODAY).unwrap();
        let z = agents.iter().find(|r| r.key == "zcode").unwrap();
        assert_eq!(z.values[0], Some(22));
        let bd = s.breakdown("agent", "zcode", "2026-09", TODAY).unwrap();
        // 每天都产出（≤today 共 6 天），无数据日 slices=None
        assert_eq!(bd.len(), 6);
        let d1 = &bd[0];
        let slices = d1.slices.as_ref().unwrap();
        assert_eq!(slices.len(), 2); // glm=15 + gpt=7，零贡献行不存在
        let sum: i64 = slices.iter().map(|x| x.tokens).sum();
        assert_eq!(sum, 22);
        assert_eq!(bd[1].slices.as_ref().unwrap()[0].tokens, 2);
    }

    #[test]
    fn breakdown_model_kind_labels_agents() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "glm-5.3", 10, 5, 15);
        let bd = s.breakdown("model", "glm-5.3", "2026-09", TODAY).unwrap();
        let slice = &bd[0].slices.as_ref().unwrap()[0];
        assert_eq!(slice.key, "zcode");
        assert_eq!(slice.label, "ZCode");
    }

    #[test]
    fn export_rows_filters_zero() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "m", 0, 0, 5);
        insert(&mut s, "2026-09-02", "codex", "m", 0, 0, 0); // 全零不入导出
        let rows = s.export_rows("2026-09").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], ("2026-09-01".into(), "zcode".into(), "m".into(), 5));
    }

    #[test]
    fn cursor_roundtrip() {
        let mut s = Store::open_in_memory().unwrap();
        assert!(s.get_cursor("zcode", "db").is_none());
        let mut b = Batch::default();
        b.cursors.push(("db".into(), "{\"rowid\":42}".into()));
        s.commit("zcode", &b).unwrap();
        assert_eq!(s.get_cursor("zcode", "db").unwrap(), "{\"rowid\":42}");
    }

    #[test]
    fn invalid_month_returns_none() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.month_rows("2026-13", "agent", "total", TODAY).is_none());
        assert!(s.month_rows("bad", "agent", "total", TODAY).is_none());
    }

    #[test]
    fn days_in_month_handles_leap_years() {
        assert_eq!(days_in_month(2026, 9), 30);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
    }

    fn credit_row(s: &mut Store, agent: &str, model: &str, day: &str, credit: f64) {
        let mut b = Batch::default();
        b.add_usage(day, Some(9), agent, model, Tokens { input: 10, output: 0, total: 10, cache_read: 0, cache_write: 0 }, 1);
        b.add_credit(day, agent, model, credit);
        s.commit(agent, &b).unwrap();
    }

    #[test]
    fn credit_summary_empty_month_is_no_data() {
        let s = Store::open_in_memory().unwrap();
        let sum = s.credit_summary("2026-08").unwrap();
        assert!(!sum.has_data);
        assert_eq!(sum.total_credit, 0.0);
        assert_eq!(sum.total_requests, 0);
        assert!(sum.by_model.is_empty());
        assert!(sum.by_day.is_empty());
        // 非法月份
        assert!(s.credit_summary("bad").is_none());
        assert!(s.credit_summary("2026-13").is_none());
    }

    #[test]
    fn credit_summary_totals_include_workbuddy_model_rows_exclude() {
        let mut s = Store::open_in_memory().unwrap();
        credit_row(&mut s, "codebuddy", "glm-5.3-flash", "2026-09-01", 1.5);
        credit_row(&mut s, "codebuddy", "deepseek-v4-pro", "2026-09-01", 2.0);
        // WorkBuddy:共享积分池 → 计入总量;模型维度排除
        credit_row(&mut s, "workbuddy", "hy4-preview", "2026-09-02", 0.9);
        credit_row(&mut s, "codebuddy", "glm-5.3-flash", "2026-09-03", 0.6);
        // unknown 模型:归「Unknown」行,不丢弃
        credit_row(&mut s, "codebuddy", "unknown", "2026-09-03", 1.0);
        // 池外 agent 与无积分行不计
        credit_row(&mut s, "zcode", "glm-5.3", "2026-09-03", 7.0);
        let mut b = Batch::default();
        b.add_usage("2026-09-03", Some(9), "codebuddy", "no-credit", Tokens { input: 1, output: 0, total: 1, cache_read: 0, cache_write: 0 }, 1);
        s.commit("codebuddy", &b).unwrap();
        // 同格二次入账累加
        credit_row(&mut s, "codebuddy", "glm-5.3-flash", "2026-09-03", 0.4);

        let sum = s.credit_summary("2026-09").unwrap();
        assert!(sum.has_data);
        assert_eq!(sum.total_requests, 6, "带积分格的 request_count 之和（glm 09-03 两次入账各 1 轮）");
        assert!((sum.total_credit - 6.4).abs() < 1e-9, "池内求和含 WB: 1.5+2+0.9+0.6+1+0.4");

        let labels: Vec<&str> = sum.by_model.iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"Unknown"), "unknown 行归组展示不丢弃");
        assert!(!labels.contains(&"hy4-preview"), "WB 行不进模型分布");
        assert!(!labels.iter().any(|l| l.contains("no-credit")), "无积分格不进分布");
        let model_sum: f64 = sum.by_model.iter().map(|r| r.credit).sum();
        assert!((model_sum - 5.5).abs() < 1e-9);
        assert!(sum.by_model.windows(2).all(|w| w[0].credit >= w[1].credit));

        assert_eq!(sum.by_day.len(), 3);
        assert_eq!(sum.by_day[0].day, "2026-09-01");
        assert!((sum.by_day[0].credit - 3.5).abs() < 1e-9);
        assert!((sum.by_day[2].credit - 2.0).abs() < 1e-9);

        let aug = s.credit_summary("2026-08").unwrap();
        assert!(!aug.has_data);

        assert_eq!(sum.by_model_day.len(), 3, "glm/deepseek/unknown 三系列");
        let glm = sum.by_model_day.iter().find(|m| m.key == "glm-5.3-flash").unwrap();
        assert_eq!(glm.by_day.len(), 2);
        assert!((glm.by_day[0].credit - 1.5).abs() < 1e-9);
        assert!((glm.by_day[1].credit - 1.0).abs() < 1e-9);
        assert!(!sum.by_model_day.iter().any(|m| m.key == "hy4-preview"), "WB 行不进模型×日");
        assert!(aug.by_model_day.is_empty());
    }

    #[test]
    fn hourly_and_daily_conserve() {
        let mut s = Store::open_in_memory().unwrap();
        let mut b = Batch::default();
        b.add_hour("2026-09-01", Some(9), "zcode", "glm", 10, 5, 15, 1);
        b.add_hour("2026-09-01", Some(9), "zcode", "glm", 1, 1, 2, 0);
        b.add_hour("2026-09-01", Some(14), "zcode", "glm", 3, 0, 3, 1);
        // 无 hour 的事件:只入日表
        b.add("2026-09-01", "codex", "gpt", 4, 1, 5, 1);
        s.commit("test", &b).unwrap();

        // 日表含全部事件;小时表按日合计 == 日表(仅对有 hour 的行)
        let daily = s.month_rows("2026-09", "agent", "total", TODAY).unwrap();
        let z = daily.iter().find(|r| r.key == "zcode").unwrap();
        assert_eq!(z.month_total, 20); // 15+2+3
        let c = daily.iter().find(|r| r.key == "codex").unwrap();
        assert_eq!(c.month_total, 5);

        let (y, m) = (2026i32, 9u32);
        let _ = (y, m);
        // 小时序列守恒:zcode 当日 total 合计 == 日表 zcode 总量
        let series = s.range_series("2026-09-01", "2026-09-01", "hour", "agent", "total", Some(("agent", "zcode"))).unwrap();
        let hour_sum: i64 = series.points.iter().flat_map(|p| p.values.iter()).sum();
        assert_eq!(hour_sum, 20);
        assert_eq!(series.points.len(), 24, "单日 hour 轴恒 24 点补零");
        // bucket key 形如 "2026-09-01 09"
        assert!(series.points.iter().any(|p| p.bucket == "2026-09-01 09"));
        assert!(series.points.iter().any(|p| p.bucket == "2026-09-01 14"));
    }

    #[test]
    fn range_series_day_axis_and_filters() {
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "glm", 10, 5, 15);
        insert(&mut s, "2026-09-03", "codex", "gpt", 4, 1, 5);

        // 全维单系列:中间日(9-2)补零
        let total = s.range_series("2026-09-01", "2026-09-03", "day", "total", "total", None).unwrap();
        assert_eq!(total.series_keys, vec!["__total__"]);
        assert_eq!(total.points.len(), 3);
        assert_eq!(total.points[0].values, vec![15]);
        assert_eq!(total.points[1].values, vec![0]);
        assert_eq!(total.points[2].values, vec![5]);

        // agent 维两系列按总量降序,逐点对齐
        let ag = s.range_series("2026-09-01", "2026-09-03", "day", "agent", "total", None).unwrap();
        assert_eq!(ag.series_keys, vec!["zcode", "codex"]);
        assert_eq!(ag.series_labels, vec!["ZCode", "Codex"]);
        assert_eq!(ag.points[0].values, vec![15, 0]);
        assert_eq!(ag.points[2].values, vec![0, 5]);

        // model 筛选收窄:只看 zcode
        let filtered = s.range_series("2026-09-01", "2026-09-03", "day", "model", "total", Some(("agent", "zcode"))).unwrap();
        assert_eq!(filtered.series_keys, vec!["glm"]);
        assert_eq!(filtered.points[2].values, vec![0]);

        // metric 列切换
        let inp = s.range_series("2026-09-01", "2026-09-01", "day", "total", "input", None).unwrap();
        assert_eq!(inp.points[0].values, vec![10]);

        // 非法参数
        assert!(s.range_series("bad", "2026-09-03", "day", "total", "total", None).is_none());
        assert!(s.range_series("2026-09-01", "2026-09-03", "day", "bad_dim", "total", None).is_none());
    }

    #[test]
    fn migration_from_v11_resets_but_keeps_project_meta() {
        // v11 库:无 seen_line / session_alias → 清库重建。
        let dir = std::env::temp_dir().join(format!("tc_v12_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("migrate11.db");
        let _ = std::fs::remove_file(&db);
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            let v11_schema = RESET_SCHEMA.split("-- v12:").next().unwrap().to_string();
            assert!(!v11_schema.contains("seen_line") && !v11_schema.contains("session_alias"), "构造的 v11 schema 不应含新表");
            conn.execute_batch(&format!(
                "{v11_schema}
                 INSERT INTO daily_usage (day, agent_key, model_key, total_tokens, request_count) VALUES ('2026-09-16','claude-code','m',10,1);
                 INSERT INTO session (agent_key, session_id, project_key, started_at) VALUES ('claude-code','s1','p',1);
                 INSERT INTO source_cursor VALUES ('claude-code','f','{{}}',1);
                 CREATE TABLE project_meta (project_key TEXT PRIMARY KEY, alias TEXT, hidden INTEGER NOT NULL DEFAULT 0,
                     merged_into TEXT, note TEXT, updated_at INTEGER NOT NULL);
                 INSERT INTO project_meta (project_key, alias, updated_at) VALUES ('e:/p','Alias',1);
                 PRAGMA user_version = 11;"
            ))
            .unwrap();
        }
        {
            let store = Store::open(&db).unwrap();
            let v: i64 = store.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(v, SCHEMA_VERSION);
            assert!(store.get_cursor("claude-code", "f").is_none(), "游标清空 = 全量重扫");
            assert!(store.export_rows("2026-09").unwrap().is_empty());
            assert!(store.test_sessions("claude-code").is_empty());
            assert!(!table_columns(&store, "seen_line").is_empty() && !table_columns(&store, "session_alias").is_empty());
            let alias: String = store.conn.query_row("SELECT alias FROM project_meta WHERE project_key = 'e:/p'", [], |r| r.get(0)).unwrap();
            assert_eq!(alias, "Alias", "常驻表跨清库保留");
        }
        for f in ["migrate11.db", "migrate11.db-wal", "migrate11.db-shm"] {
            let _ = std::fs::remove_file(dir.join(f));
        }
    }

    fn part(day: &str, model: &str, total: i64, calls: i64, mark: i64) -> TurnPart {
        TurnPart { day: day.into(), model: model.into(), input: total, output: 0, total, cache_read: 0, cache_write: 0, model_calls: calls, turn_mark: mark }
    }

    fn raw_turn(sid: &str, seq: i64, day: &str, start: i64, gap: Option<i64>, parts: Vec<TurnPart>) -> TurnRow {
        TurnRow {
            session_id: sid.into(),
            turn_seq: seq,
            day: day.into(),
            project_key: "e:/p".into(),
            model_key: parts.first().map(|p| p.model.clone()).unwrap_or_else(|| "unknown".into()),
            started_at: start,
            ended_at: start + 1_000,
            wall_ms: Some(1_000),
            model_ms: Some(400),
            tool_ms: Some(100),
            ttft_ms: None,
            gap_ms: gap,
            model_calls: parts.iter().map(|p| p.model_calls).sum(),
            tool_calls: 1,
            error_count: 0,
            retry_count: 0,
            aborted: false,
            parts,
        }
    }

    /// 订阅侧的本地活动信号:已提交批次的轮结束时刻区间跨批累积（起点取最早、终点取最晚）,
    /// 取走即清;无轮批次不产生信号。
    #[test]
    fn turn_span_accumulates_and_takes() {
        let mut s = Store::open_in_memory().unwrap();
        assert_eq!(s.take_turn_span(), None);
        let mut b = Batch::default();
        b.add_turn("a", raw_turn("s", 1, "2026-09-01", 9_000_000, None, vec![]));
        b.add_turn("a", raw_turn("s", 2, "2026-09-01", 5_000_000, None, vec![]));
        s.commit("a", &b).unwrap();
        let mut b = Batch::default();
        b.add_turn("b", raw_turn("t", 1, "2026-09-01", 2_000_000, None, vec![]));
        s.commit("b", &b).unwrap();
        assert_eq!(s.take_turn_span(), Some((2_001_000, 9_001_000)), "两批取并集");
        assert_eq!(s.take_turn_span(), None, "取走即清");
        s.commit("a", &Batch::default()).unwrap();
        assert_eq!(s.take_turn_span(), None, "无轮批次不产生信号");
    }

    /// 跨午夜 / 多模型的轮:token 与 turns 按切片的日与模型落账,daily_project 仍逐格守恒;
    /// idle 只计 gap ≤ 阈值;子会话 wall / idle 不入项目维,调用计入 subagent_calls。
    #[test]
    fn daily_project_conservation_idle_and_child_rules() {
        let mut s = Store::open_in_memory().unwrap();
        let mut b = Batch::default();
        let t = |i: i64| Tokens { input: i, output: 0, total: i, cache_read: 0, cache_write: 0 };
        // daily_usage 同源写入（模拟适配器 response() 的双写）
        b.add_usage("2026-09-01", Some(23), "a", "m1", t(10), 1);
        b.add_usage("2026-09-02", Some(0), "a", "m2", t(20), 0);
        b.add_usage("2026-09-02", Some(1), "a", "m1", t(5), 1);
        b.add_usage("2026-09-02", Some(1), "a", "m3", t(7), 0);
        b.add_turn("a", raw_turn("root", 1, "2026-09-01", 1_000_000, None, vec![part("2026-09-01", "m1", 10, 1, 1), part("2026-09-02", "m2", 20, 1, 0)]));
        b.add_turn("a", raw_turn("root", 2, "2026-09-02", 9_000_000, Some(IDLE_THRESHOLD_MS + 1), vec![part("2026-09-02", "m1", 5, 1, 1)]));
        b.add_turn("a", raw_turn("child", 1, "2026-09-02", 9_000_500, Some(5_000), vec![part("2026-09-02", "m3", 7, 1, 0)]));
        b.upsert_session("a", SessionRow { session_id: "root".into(), project_key: Some("e:/p".into()), ..SessionRow::default() });
        b.upsert_session("a", SessionRow { session_id: "child".into(), project_key: Some("e:/p".into()), parent_id: Some("root".into()), ..SessionRow::default() });
        s.commit("a", &b).unwrap();
        assert!(s.test_project_conservation().is_empty(), "{:?}", s.test_project_conservation());

        let (wall, idle, sub_calls, calls): (i64, i64, i64, i64) = s
            .conn
            .query_row(
                "SELECT SUM(wall_ms), SUM(idle_ms), SUM(subagent_calls), SUM(model_calls) FROM daily_project WHERE day = '2026-09-02'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(wall, 1_000, "只计根会话 wall（子会话在父轮墙钟内）");
        assert_eq!(idle, 0, "gap 超阈值不计 idle;子会话 gap 不计");
        assert_eq!((sub_calls, calls), (1, 3));
        let turns = s.test_turns("a");
        assert_eq!(turns.len(), 2);
        assert_eq!((turns[1].subagent_count, turns[1].subagent_calls, turns[1].model_calls), (1, 1, 2), "子轮按时间并入第 2 轮");
        assert_eq!(turns[1].model_ms, Some(800));
        assert_eq!(turns[1].wall_ms, Some(1_000));
    }

    #[test]
    fn replace_session_rebuilds_task_layers() {
        let mut s = Store::open_in_memory().unwrap();
        let mut b = Batch::default();
        b.add_usage("2026-09-01", Some(9), "zcode", "m", Tokens { input: 3, output: 0, total: 3, cache_read: 0, cache_write: 0 }, 1);
        b.add_turn("zcode", raw_turn("s", 1, "2026-09-01", 1_000, None, vec![part("2026-09-01", "m", 3, 1, 1)]));
        b.add_turn("zcode", raw_turn("s", 2, "2026-09-01", 5_000, Some(3_000), vec![]));
        s.commit("zcode", &b).unwrap();
        assert_eq!(s.test_turns("zcode").len(), 2);
        // 整会话重建:第 2 轮消失 → turn / daily_project 同步收缩
        let mut b = Batch::default();
        b.replace_session("zcode", "s");
        b.add_turn("zcode", raw_turn("s", 1, "2026-09-01", 1_000, None, vec![part("2026-09-01", "m", 3, 1, 1)]));
        s.commit("zcode", &b).unwrap();
        assert_eq!(s.test_turns("zcode").len(), 1);
        assert!(s.test_project_conservation().is_empty(), "{:?}", s.test_project_conservation());
    }

    #[test]
    fn content_columns_never_in_export() {
        assert_eq!(CONTENT_COLUMNS, &[("session", "title")]);
        let mut s = Store::open_in_memory().unwrap();
        insert(&mut s, "2026-09-01", "zcode", "m", 1, 1, 2);
        // 导出只读 daily_usage 元数据列（debug_assert 守护 SQL 不触碰内容列）
        assert_eq!(s.export_rows("2026-09").unwrap().len(), 1);
    }

    fn table_columns(store: &Store, table: &str) -> Vec<String> {
        let mut stmt = store.conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
        stmt.query_map([], |r| r.get::<_, String>(1)).unwrap().flatten().collect()
    }

    #[test]
    fn reset_drop_and_create_lists_match() {
        // 迁移 DROP 清单与 CREATE 清单逐一对应（多一张 = 残表,少一张 = 缺表）。
        let created: BTreeSet<&str> = RESET_SCHEMA
            .split("CREATE TABLE IF NOT EXISTS ")
            .skip(1)
            .filter_map(|s| s.split_whitespace().next())
            .collect();
        let dropped: BTreeSet<&str> = RESET_TABLES.iter().copied().collect();
        assert_eq!(created, dropped);
        assert_eq!(RESET_TABLES.len(), dropped.len(), "DROP 清单无重复");
    }

    #[test]
    fn fresh_db_has_full_schema() {
        let s = Store::open_in_memory().unwrap();
        let v: i64 = s.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        for t in RESET_TABLES.iter().chain(["source_state", "project_meta"].iter()) {
            assert!(!table_columns(&s, t).is_empty(), "缺表 {t}");
        }
        assert!(table_columns(&s, "request_model").is_empty(), "v11 起不建对账表");
        assert!(table_columns(&s, "daily_usage").contains(&"credit".to_string()));
    }

    #[test]
    fn migration_from_v6_resets_and_drops_ledger() {
        // 模拟 v6 旧库（无 cache 列、无三张新表;对账账本有数据）重开:
        // init 应 DROP 重建清库表 + 推版本到当前;request_model 一并删除。
        let dir = std::env::temp_dir().join(format!("tc_v7_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("migrate.db");
        let _ = std::fs::remove_file(&db);
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE daily_usage (day TEXT, agent_key TEXT, model_key TEXT,
                     input_tokens INTEGER, output_tokens INTEGER, total_tokens INTEGER, request_count INTEGER,
                     PRIMARY KEY (day, agent_key, model_key));
                 CREATE TABLE hourly_usage (day TEXT, hour INTEGER, agent_key TEXT, model_key TEXT,
                     input_tokens INTEGER, output_tokens INTEGER, total_tokens INTEGER,
                     PRIMARY KEY (day, hour, agent_key, model_key));
                 CREATE TABLE source_cursor (source_id TEXT, scope TEXT, cursor_json TEXT, updated_at INTEGER,
                     PRIMARY KEY (source_id, scope));
                 CREATE TABLE request_model (source_id TEXT NOT NULL, request_id TEXT NOT NULL, model_key TEXT NOT NULL,
                     client TEXT, day TEXT, credit REAL, PRIMARY KEY (source_id, request_id));
                 INSERT INTO daily_usage VALUES ('2026-09-01','codex','gpt',1,1,2,1);
                 INSERT INTO hourly_usage VALUES ('2026-09-01',9,'codex','gpt',1,1,2);
                 INSERT INTO source_cursor VALUES ('codex','f','{}',1);
                 INSERT INTO request_model VALUES ('codebuddy','r1','glm','CodeBuddyIDE','2026-09-01',1.5);
                 PRAGMA user_version = 6;",
            )
            .unwrap();
        }
        {
            let store = Store::open(&db).unwrap();
            let v: i64 = store.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
            assert_eq!(v, SCHEMA_VERSION, "v6 库重开应一次迁移到当前版本");
            // 清库重扫语义:聚合与游标全清
            assert!(store.month_rows("2026-09", "agent", "total", TODAY).unwrap().is_empty());
            assert!(store.get_cursor("codex", "f").is_none());
            // cache 两列只加在日/时表;主键不变
            for t in ["daily_usage", "hourly_usage"] {
                let cols = table_columns(&store, t);
                assert!(cols.contains(&"cache_read_tokens".to_string()) && cols.contains(&"cache_write_tokens".to_string()), "{t} 缺 cache 列");
            }
            for t in ["session", "turn_raw", "turn_part", "turn", "daily_project"] {
                assert!(!table_columns(&store, t).is_empty(), "缺新表 {t}");
            }
            // 对账账本被删除
            assert!(table_columns(&store, "request_model").is_empty());
        }
        {
            // 二次打开:版本已到位,不再清库
            let mut store = Store::open(&db).unwrap();
            insert(&mut store, "2026-09-02", "zcode", "glm", 1, 1, 2);
            drop(store);
            let store = Store::open(&db).unwrap();
            assert_eq!(store.month_rows("2026-09", "agent", "total", TODAY).unwrap().len(), 1);
        }
        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(dir.join("migrate.db-wal"));
        let _ = std::fs::remove_file(dir.join("migrate.db-shm"));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn cache_columns_accumulate_daily_and_hourly() {
        let mut s = Store::open_in_memory().unwrap();
        let mut b = Batch::default();
        let t = Tokens { input: 10, output: 5, total: 115, cache_read: 100, cache_write: 7 };
        b.add_usage("2026-09-01", Some(9), "claude-code", "m", t, 1);
        b.add_usage("2026-09-01", Some(10), "claude-code", "m", t, 0);
        s.commit("claude-code", &b).unwrap();
        let (cr, cw, rc): (i64, i64, i64) = s
            .conn
            .query_row("SELECT cache_read_tokens, cache_write_tokens, request_count FROM daily_usage", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!((cr, cw, rc), (200, 14, 1));
        let (hcr, hcw): (i64, i64) = s
            .conn
            .query_row("SELECT SUM(cache_read_tokens), SUM(cache_write_tokens) FROM hourly_usage", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!((hcr, hcw), (200, 14), "小时表 cache 按日合计守恒");
    }
}
