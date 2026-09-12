//! DSH （DeepSeek Harness) 适配器：
//! `$DSH_HOME/sessions/<projectKey>/session-<uuid>/session[.vN].jsonl[.zstd]`。
//!
//! 许可声明：本文件的多帧 zstd 帧扫描器（`scan_zstd_frames` / `scan_frame_at`）
//! 移植自 DSH 的 `session-persistence-jsonl/zstd.ts`；DSH 以 MIT 许可发布
//! （Copyright （c) 2026 DeepSeek），详见仓库根 THIRD-PARTY-NOTICES.md。
//!
//! 口径（本机活跃+闭合双会话逐行核对 + 官方 projcache
//! tokenUsage totals 数字对账）：
//!
//! - **usage 行** = `assistant/message` 的 `data.usage`：`inputTokens`（已排
//!   cache）/`outputTokens`/`totalTokens`/`cacheReadTokens`,守恒
//!   `total = input + output + cacheRead` 逐条成立 → input **无需 cache 减法**,
//!   total 取 provider total（缺失回退 input+output,守则通用规则）。
//! - **对话轮** = `user/message` 且 `data.source.kind=="user"`（真实用户输入,
//!   带 rpcId）。注入伪行 kind ∈ {agent-instructions, plugin, skill-catalog}
//!   不置位（AGENTS.md/运行时上下文/skill 目录以 role=user 注入,一字段判别,
//!   不做字符串匹配）。pending 模式:真实用户行置位,下一条 usage 行按其模型
//!   计 1 turn 并清位。DSH 的 turn 折叠语义（活跃 turn 内追加的用户消息被
//!   splice 进当前 turn,turn/start 数不满）被 kind 判据天然覆盖。
//! - **模型名归一化**：arkcli helper 写入 DSH 的是火山 API 模型 id,日志
//!   `message.source.model` 记 id 而非显示名；
//!   与其他采集器（codebuddy/workbuddy/zcode 记 glm-5.3-flash、deepseek-v4-flash）
//!   对齐,未映射 id 原样通过（新模型容错）。
//! - **文件层**：多帧 zstd 拼接容器（DSH 逐批追加一帧,帧内 JSONL 批以换行结尾）,
//!   游标 = 最后完整帧的字节边界;尾部不完整帧（torn）跳过等下轮。
//!   代际文件 session[.vN] 选版本最高者;代际切换/文件截断 → offset 归零重读,
//!   事件 seq 水位去重（seq 跨代际稳定,迁移重编码不变）。
//! - **种子会话**（分叉,isSeeded）：头部 `inheritedEventCount` = 复制父会话的
//!   前缀事件数,`seq < inheritedEventCount` 跳过。
//! - request/header、request/context、turn/start、session/title* 等一律忽略
//!   （title 生成无 usage;request/header 每会话仅一条,不可作请求计数）。

use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::store::{Batch, Store};
use super::{
    Adapter, AdapterError, AdapterMeta, CollectOutcome, CollectResult, ProbeOutcome, clamp0,
    home_dir, millis_to_local_day_hour, TAIL_MAX_BYTES,
};

pub struct DshAdapter {
    sessions_dir: PathBuf,
}

static META: AdapterMeta = AdapterMeta {
    id: "dsh",
    name: "DeepSeek Harness",
    location: "~/.dsh/sessions",
    kind: "jsonl-zstd",
};

impl DshAdapter {
    pub fn new() -> Self {
        // DSH home 解析与官方一致：$DSH_HOME 环境变量 > ~/.dsh（空串视为未设）。
        let base = std::env::var_os("DSH_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join(".dsh")));
        let sessions_dir = base.map(|b| b.join("sessions")).unwrap_or_default();
        DshAdapter { sessions_dir }
    }
}

// ---------- 游标 ----------

/// DSH 会话游标（scope = 会话目录绝对路径）。与 FileCursor 分离：读取边界是
/// 帧边界而非行边界,且需要 seq 水位与代际文件名。JSON 存 source_cursor.cursor_json。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct DshCursor {
    /// 选中的代际文件名；变更 = 代际升级 → offset 归零重读。
    #[serde(default)]
    file: String,
    /// zstd: 最后完整帧字节边界；明文: 最后完整行边界。
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    mtime: i64,
    /// 已消费的最大 usage 事件 seq（跨代际稳定；重读时 seq <= 该值跳过）。
    #[serde(default = "default_last_seq")]
    last_seq: i64,
    /// 种子会话继承前缀长度（seq < 该值 = 父会话副本,不置位不计数）。
    #[serde(default)]
    inherited: i64,
    /// 对话轮 pending 标志（真实用户输入 → 下一条 usage 行计 1 turn）。
    #[serde(default)]
    pending_turn: bool,
}

fn default_last_seq() -> i64 {
    -1
}

impl DshCursor {
    fn fresh() -> Self {
        DshCursor {
            file: String::new(),
            offset: 0,
            size: 0,
            mtime: 0,
            last_seq: -1,
            inherited: 0,
            pending_turn: false,
        }
    }
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
}

fn load_dsh_cursor(store: &Store, scope: &str) -> DshCursor {
    store
        .get_cursor(META.id, scope)
        .and_then(|j| serde_json::from_str::<DshCursor>(&j).ok())
        .unwrap_or_else(DshCursor::fresh)
}

// ---------- 发现与代际选择 ----------

/// 解析会话日志文件名 → （代际版本, 是否 zstd)。
/// `session.jsonl` → （0,false)；`session.vN.jsonl` → （N,false)；`.zstd` 后缀同理。
/// 其余（session.lock、session-title 等）返回 None。
fn parse_generation_filename(name: &str) -> Option<(u64, bool)> {
    let (base, zstd) = match name.strip_suffix(".zstd") {
        Some(b) => (b, true),
        None => (name, false),
    };
    let rest = base.strip_prefix("session")?;
    if rest == ".jsonl" {
        return Some((0, zstd));
    }
    rest.strip_suffix(".jsonl")?
        .strip_prefix(".v")?
        .parse::<u64>()
        .ok()
        .map(|v| (v, zstd))
}

/// 单会话目录：列出全部代际文件,选版本最高者（同版本 zstd 优先——同一根
/// 物理编码唯一,双后缀并存属异常,取可解压的 zstd 侧）。
fn select_generation(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(u64, bool, PathBuf)> = None;
    for e in entries.flatten() {
        if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if let Some((ver, zstd)) = parse_generation_filename(&name) {
            let better = match &best {
                None => true,
                Some((bv, bz, _)) => ver > *bv || (ver == *bv && zstd && !*bz),
            };
            if better {
                best = Some((ver, zstd, e.path()));
            }
        }
    }
    best.map(|(_, _, p)| p)
}

/// 发现全部会话：sessions/<project>/<session-uuid>/ → （会话目录, 选中文件)。
/// 单目录不可读静默跳过（容错铁律）。
fn discover_session_files(root: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(root) else {
        return out;
    };
    for project in projects.flatten() {
        if !project.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(sessions) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for sess in sessions.flatten() {
            if !sess.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            // 会话目录名形如 session-<uuid>；前缀宽松匹配,异常目录自然无日志。
            if !sess.file_name().to_string_lossy().starts_with("session-") {
                continue;
            }
            let Some(best) = select_generation(&sess.path()) else {
                continue;
            };
            out.push((sess.path(), best));
        }
    }
    out
}

// ---------- 多帧 zstd 读取 ----------

/// 扫描拼接 zstd 流的全部完整帧边界。移植 DSH session-persistence-jsonl/zstd.ts
/// 的帧头/块头遍历（zlib ZSTD_MAGIC 0xFD2FB528）；只读容错：魔法数不符、结构
/// 残缺（torn 尾帧）一律停在上一完整帧,不抛错不崩溃。
fn scan_zstd_frames(buf: &[u8]) -> Vec<(usize, usize)> {
    const MAGIC: u32 = 0xFD2FB528;
    let mut frames = Vec::new();
    let mut off = 0usize;
    while off < buf.len() {
        let Some(end) = scan_frame_at(buf, off, MAGIC) else {
            break;
        };
        frames.push((off, end));
        off = end;
    }
    frames
}

/// 从 start 扫一个帧,返回排他帧尾；结构残缺返回 None。
fn scan_frame_at(buf: &[u8], start: usize, magic: u32) -> Option<usize> {
    let mut off = start;
    if buf.len() - off < 4 {
        return None;
    }
    if u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) != magic {
        return None;
    }
    off += 4;
    if off == buf.len() {
        return None;
    }
    let descriptor = buf[off];
    off += 1;
    // 帧头描述符：FCS 大小标志（6-7)/单段（5)/校验和（2)/字典（0-1)。
    // 保留位不校验（只读容错;DSH 写侧恒为合法头,异常位交给解压失败兜底）。
    let fcs_flag = descriptor >> 6;
    let single_segment = descriptor & 0x20 != 0;
    let checksum = descriptor & 0x04 != 0;
    let dict_flag = (descriptor & 0x03) as usize;
    let dict_bytes = if dict_flag == 3 { 4 } else { dict_flag };
    let fcs_bytes = if fcs_flag == 0 {
        if single_segment {
            1
        } else {
            0
        }
    } else {
        1usize << fcs_flag
    };
    let header_rest = (if single_segment { 0 } else { 1 }) + dict_bytes + fcs_bytes;
    if buf.len() - off < header_rest {
        return None;
    }
    off += header_rest;
    // 块序列：3 字节小端块头（last_block|type|size）,直到 last_block。
    loop {
        if buf.len() - off < 3 {
            return None;
        }
        let bh = buf[off] as u32 | ((buf[off + 1] as u32) << 8) | ((buf[off + 2] as u32) << 16);
        off += 3;
        let last_block = bh & 1 != 0;
        let block_type = (bh >> 1) & 0x03;
        let block_size = (bh >> 3) as usize;
        if block_type == 0x03 {
            return None; // 保留块类型 = 结构损坏
        }
        // RLE 块（0x01)负载恒 1 字节；Raw/Compressed 块为块头声明大小。
        let payload = if block_type == 0x01 { 1usize } else { block_size };
        if buf.len() - off < payload {
            return None;
        }
        off += payload;
        if last_block {
            break;
        }
    }
    if checksum {
        if buf.len() - off < 4 {
            return None;
        }
        off += 4;
    }
    Some(off)
}

/// 逐帧解压；任一帧解压失败 → 停在上一好帧。
/// 返回 （明文拼接, 最后好帧的排他尾)。
fn decompress_frames(buf: &[u8], frames: &[(usize, usize)]) -> (Vec<u8>, usize) {
    let mut text = Vec::new();
    let mut good = 0usize;
    for &(s, e) in frames {
        match zstd::decode_all(std::io::Cursor::new(&buf[s..e])) {
            Ok(piece) => {
                text.extend_from_slice(&piece);
                good += 1;
            }
            Err(_) => break,
        }
    }
    let end = if good == 0 { 0 } else { frames[good - 1].1 };
    (text, end)
}

fn split_lines(text: &[u8]) -> Vec<String> {
    let s = String::from_utf8_lossy(text);
    s.lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

struct DshConsume {
    new_offset: u64,
    lines: Vec<String>,
    /// 本次从 0 重读（截断/代际切换）——行内含头行,水位去重兜底。
    reset: bool,
}

/// 推进单个会话日志：zstd 走帧边界,明文走行边界（jsonl:tail）。
/// 无新内容 → None；截断（size < offset）→ 归零重读。
fn advance_dsh_file(path: &Path, zstd: bool, cursor: &DshCursor) -> Option<DshConsume> {
    let (size, mtime) = super::jsonl::generation(path)?;
    if size == cursor.size && mtime == cursor.mtime && cursor.offset >= size {
        return None; // generation 未变且已读到 EOF
    }
    if size < cursor.offset {
        return read_from(path, zstd, 0, true);
    }
    read_from(path, zstd, cursor.offset, false)
}

fn read_from(path: &Path, zstd: bool, start: u64, reset: bool) -> Option<DshConsume> {
    if zstd {
        let mut file = std::fs::File::open(path).ok()?;
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut raw = Vec::new();
        // 单轮字节上限：超限停在最后完整帧边界,下轮续读。
        file.take(TAIL_MAX_BYTES).read_to_end(&mut raw).ok()?;
        let frames = scan_zstd_frames(&raw);
        if frames.is_empty() {
            // 无完整帧（首帧未落盘的极端瞬态）→ 无消费。
            return None;
        }
        let (text, frame_end) = decompress_frames(&raw, &frames);
        let lines = split_lines(&text);
        Some(DshConsume { new_offset: start + frame_end as u64, lines, reset })
    } else {
        match super::jsonl::tail(path, start, TAIL_MAX_BYTES) {
            Ok(super::jsonl::Tail::Advanced { new_offset, lines }) => {
                Some(DshConsume { new_offset, lines, reset })
            }
            _ => None,
        }
    }
}

// ---------- 口径解析 ----------

/// 模型名归一化：arkcli 写入的火山 API 模型 id → 其他采集器通行的模型名
/// （collector.db ：codebuddy/workbuddy/zcode 记 glm-5.3-flash 与
/// deepseek-v4-flash）。未映射 id 原样通过。
fn normalize_model(model: &str) -> String {
    match model {
        "glm-5-3-flash" => "glm-5.3-flash",
        "deepseek-v4-flash-ga-260731" => "deepseek-v4-flash",
        "deepseek-v4-pro-ga-260813" => "deepseek-v4-pro",
        other => other,
    }
    .to_string()
}

enum DshLine {
    /// 头行（type=="session",仅日志首）：种子会话继承前缀长度。
    Header { inherited: i64 },
    /// 真实用户输入（kind=="user"）。
    UserInput { seq: i64 },
    /// usage 行（assistant/message + data.usage）。
    Usage {
        seq: i64,
        day: String,
        hour: u8,
        model: String,
        input: i64,
        output: i64,
        total: i64,
    },
    None,
}

fn parse_line(line: &str) -> DshLine {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return DshLine::None;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("session") => DshLine::Header {
            inherited: v
                .get("inheritedEventCount")
                .and_then(|x| x.as_i64())
                .unwrap_or(0),
        },
        Some("user/message") => {
            // 真实用户输入判据 = source.kind=="user"（结构化字段,非字符串匹配）。
            let kind = v.pointer("/data/source/kind").and_then(|k| k.as_str());
            let Some(seq) = v.get("seq").and_then(|x| x.as_i64()) else {
                return DshLine::None;
            };
            if kind == Some("user") {
                DshLine::UserInput { seq }
            } else {
                DshLine::None
            }
        }
        Some("assistant/message") => {
            let Some(usage) = v.pointer("/data/usage") else {
                return DshLine::None;
            };
            let input = clamp0(usage.get("inputTokens").and_then(|x| x.as_i64()).unwrap_or(0));
            let output = clamp0(usage.get("outputTokens").and_then(|x| x.as_i64()).unwrap_or(0));
            if input == 0 && output == 0 {
                return DshLine::None;
            }
            let total = usage
                .get("totalTokens")
                .and_then(|x| x.as_i64())
                .map(clamp0)
                .unwrap_or(input + output);
            let model = v
                .pointer("/data/message/source/model")
                .and_then(|m| m.as_str())
                .filter(|s| !s.is_empty())
                .map(normalize_model)
                .unwrap_or_else(|| "unknown".to_string());
            let Some(seq) = v.get("seq").and_then(|x| x.as_i64()) else {
                return DshLine::None;
            };
            let Some(time) = v.get("time").and_then(|x| x.as_i64()) else {
                return DshLine::None;
            };
            let Some((day, hour)) = millis_to_local_day_hour(time) else {
                return DshLine::None;
            };
            DshLine::Usage { seq, day, hour, model, input, output, total }
        }
        _ => DshLine::None,
    }
}

// ---------- 适配器 ----------

impl Adapter for DshAdapter {
    fn meta(&self) -> &'static AdapterMeta {
        &META
    }

    fn probe(&self) -> ProbeOutcome {
        if self.sessions_dir.is_dir() {
            ProbeOutcome { status: "ready".into(), fingerprint: None }
        } else {
            ProbeOutcome { status: "no_source".into(), fingerprint: None }
        }
    }

    fn collect(&self, store: &mut Store) -> CollectResult {
        if !self.sessions_dir.is_dir() {
            return Err(AdapterError::new(
                "no_source",
                format!("missing {}", self.sessions_dir.display()),
            ));
        }
        let mut files = discover_session_files(&self.sessions_dir);
        // mtime 升序（旧文件先消费,与既有源一致）。
        files.sort_by_key(|(_, f)| super::jsonl::generation(f).map(|(_, m)| m).unwrap_or(0));

        let mut batch = Batch::default();
        let mut months: BTreeSet<String> = BTreeSet::new();

        for (dir, file) in files {
            let scope = dir.display().to_string();
            let mut cursor = load_dsh_cursor(store, &scope);
            let fname = file
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if cursor.file != fname {
                // 首见/代际升级：offset 归零重读；seq 水位与 pending 保留
                //（事件 seq 跨代际稳定,水位去重防迁移重编码重计）。
                cursor.file = fname.clone();
                cursor.offset = 0;
                cursor.size = 0;
                cursor.mtime = 0;
            }
            let zstd = fname.ends_with(".zstd");
            let Some(consume) = advance_dsh_file(&file, zstd, &cursor) else {
                continue;
            };
            if consume.reset {
                eprintln!("[collector] dsh session reset (truncated/generation switch): {}", scope);
            }

            let mut next = cursor;
            for line in &consume.lines {
                match parse_line(line) {
                    DshLine::Header { inherited } => next.inherited = inherited,
                    DshLine::UserInput { seq } => {
                        // 水位 + 种子双守卫：已消费行与父会话继承行不置位。
                        if seq > next.last_seq && seq >= next.inherited {
                            next.pending_turn = true;
                        }
                    }
                    DshLine::Usage { seq, day, hour, model, input, output, total } => {
                        if seq > next.last_seq {
                            next.last_seq = seq;
                            if seq >= next.inherited {
                                let turns = if next.pending_turn {
                                    next.pending_turn = false;
                                    1
                                } else {
                                    0
                                };
                                batch.add_hour(
                                    &day,
                                    Some(hour),
                                    META.id,
                                    &model,
                                    input,
                                    output,
                                    total,
                                    turns,
                                );
                                months.insert(day[..7].to_string());
                            }
                        }
                    }
                    DshLine::None => {}
                }
            }
            next.offset = consume.new_offset;
            if let Some((size, mtime)) = super::jsonl::generation(&file) {
                next.size = size;
                next.mtime = mtime;
            }
            batch.cursors.push((scope, next.to_json()));
        }

        store.commit(META.id, &batch).map_err(|e| AdapterError::new("error", e))?;
        Ok(CollectOutcome { events: batch.events, months })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- 冻结样本行（2026-09-09 真实会话,口径基准） ----------

    const REAL_USER: &str = r#"{"type":"user/message","seq":8,"time":1788901883263,"data":{"content":[{"type":"text","text":"这是一个作为历史会话的测试流"}],"source":{"kind":"user","rpcId":"736c66f5-8423-49a8-b115-1bd09c42d305","clientTimeZone":"Europe/Rome"},"role":"user","id":"34b8c73a"},"surfaceOp":"append"}"#;
    const INJECTED_INSTRUCTIONS: &str = r#"{"type":"user/message","seq":9,"time":1788901883264,"data":{"content":[{"type":"text","text":"<system-reminder> AGENTS.md"}],"source":{"kind":"agent-instructions","form":"instructions","baseline":true},"role":"user","id":"375609f8"}}"#;
    const INJECTED_PLUGIN: &str = r#"{"type":"user/message","seq":10,"time":1788901883264,"data":{"content":[{"type":"text","text":"Current runtime context."}],"source":{"kind":"plugin","plugin":"@deepseek-ai/dsh-system-prompt","form":"snapshot"},"role":"user","id":"f2de7deb"}}"#;
    const INJECTED_SKILLS: &str = r#"{"type":"user/message","seq":11,"time":1788901883265,"data":{"content":[{"type":"text","text":"<system-reminder> skills"}],"source":{"kind":"skill-catalog","form":"catalog","entries":[]},"role":"user","id":"ed7e606c"}}"#;
    const USAGE_TURN1: &str = r#"{"type":"assistant/message","seq":16,"time":1788901890263,"data":{"turn":1,"step":1,"message":{"role":"assistant","content":[],"source":{"kind":"model","provider":"arkcli-agent-plan","model":"deepseek-v4-flash-ga-260731"},"id":"m1"},"usage":{"inputTokens":6475,"outputTokens":46,"totalTokens":14713,"cacheReadTokens":8192}}}"#;
    const USAGE_TURN2: &str = r#"{"type":"assistant/message","seq":24,"time":1788902002626,"data":{"turn":2,"step":1,"message":{"role":"assistant","content":[],"source":{"kind":"model","provider":"arkcli-agent-plan","model":"deepseek-v4-flash-ga-260731"},"id":"m2"},"usage":{"inputTokens":6528,"outputTokens":45,"totalTokens":14765,"cacheReadTokens":8192}}}"#;
    const TOOL_CALL: &str = r#"{"type":"tool/call","seq":20,"time":1788901890300,"data":{"turn":1,"step":1,"call":{"name":"read","arguments":{}}}}"#;

    #[test]
    fn real_user_vs_injected_lines() {
        // 真实用户行（kind=user,带 rpcId）→ 置 pending
        assert!(matches!(parse_line(REAL_USER), DshLine::UserInput { seq: 8 }));
        // 注入伪行（agent-instructions / plugin / skill-catalog）→ 忽略
        assert!(matches!(parse_line(INJECTED_INSTRUCTIONS), DshLine::None));
        assert!(matches!(parse_line(INJECTED_PLUGIN), DshLine::None));
        assert!(matches!(parse_line(INJECTED_SKILLS), DshLine::None));
        // 缺 seq 的用户行防御
        assert!(matches!(parse_line(r#"{"type":"user/message","data":{"source":{"kind":"user"}}}"#), DshLine::None));
    }

    #[test]
    fn usage_line_fields_and_conservation() {
        let DshLine::Usage { seq, day, hour, model, input, output, total } = parse_line(USAGE_TURN1)
        else {
            panic!("expected usage");
        };
        assert_eq!(seq, 16);
        assert_eq!(input, 6475);
        assert_eq!(output, 46);
        assert_eq!(total, 14713); // provider total = 6475+46+8192
        assert_eq!(model, "deepseek-v4-flash"); // 归一化后
        assert_eq!(day.len(), 10);
        assert!(hour <= 23);
        // 无 usage 的 assistant 行 → 忽略（不消费 pending）
        assert!(matches!(parse_line(r#"{"type":"assistant/message","seq":30,"time":1788902002626,"data":{"turn":2,"step":2,"message":{"source":{"model":"m"}}}}"#), DshLine::None));
        // 坏 JSON → 忽略
        assert!(matches!(parse_line("not json"), DshLine::None));
        // 无关事件类型 → 忽略
        assert!(matches!(parse_line(TOOL_CALL), DshLine::None));
    }

    #[test]
    fn pending_set_and_consume_across_turns() {
        // 真实用户行置位 → 同轮后续 usage 只首条计 1 轮；新用户行重新置位再消费。
        let mut cursor = DshCursor::fresh();
        let mut turns = Vec::new();
        for line in [REAL_USER, USAGE_TURN1, USAGE_TURN2] {
            match parse_line(line) {
                DshLine::UserInput { seq } => {
                    if seq > cursor.last_seq && seq >= cursor.inherited {
                        cursor.pending_turn = true;
                    }
                }
                DshLine::Usage { seq, .. } => {
                    if seq > cursor.last_seq {
                        cursor.last_seq = seq;
                        turns.push(if cursor.pending_turn {
                            cursor.pending_turn = false;
                            1
                        } else {
                            0
                        });
                    }
                }
                _ => {}
            }
        }
        assert_eq!(turns, vec![1, 0]);
    }

    #[test]
    fn model_normalization_table() {
        assert_eq!(normalize_model("glm-5-3-flash"), "glm-5.3-flash");
        assert_eq!(normalize_model("deepseek-v4-flash-ga-260731"), "deepseek-v4-flash");
        assert_eq!(normalize_model("deepseek-v4-pro-ga-260813"), "deepseek-v4-pro");
        // 未映射 id / 已对齐 id / 兜底 原样通过
        assert_eq!(normalize_model("glm-5.3"), "glm-5.3");
        assert_eq!(normalize_model("kimi-k3"), "kimi-k3");
        assert_eq!(normalize_model("some-future-model"), "some-future-model");
    }

    #[test]
    fn generation_filename_parsing() {
        assert_eq!(parse_generation_filename("session.jsonl"), Some((0, false)));
        assert_eq!(parse_generation_filename("session.jsonl.zstd"), Some((0, true)));
        assert_eq!(parse_generation_filename("session.v3.jsonl"), Some((3, false)));
        assert_eq!(parse_generation_filename("session.v3.jsonl.zstd"), Some((3, true)));
        assert_eq!(parse_generation_filename("session.v12.jsonl.zstd"), Some((12, true)));
        assert_eq!(parse_generation_filename("session.lock"), None);
        assert_eq!(parse_generation_filename("session.json"), None);
        assert_eq!(parse_generation_filename("other.jsonl"), None);
    }

    // ---------- 帧扫描与多帧解压 ----------

    fn frame(lines: &[&str]) -> Vec<u8> {
        // DSH 写侧约定：帧内批 JSONL 以换行结尾（eventLines + 写者补尾换行）。
        zstd::bulk::compress(format!("{}\n", lines.join("\n")).as_bytes(), 3).unwrap()
    }

    #[test]
    fn frame_scan_multi_frame_and_torn_tail() {
        let f1 = frame(&[r#"{"a":1}"#]);
        let f2 = frame(&[r#"{"b":2}"#, r#"{"b":3}"#]);
        let f3 = frame(&[r#"{"c":4}"#]);
        let mut buf = Vec::new();
        buf.extend_from_slice(&f1);
        buf.extend_from_slice(&f2);
        buf.extend_from_slice(&f3);
        let frames = scan_zstd_frames(&buf);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].1, buf.len());
        // 逐帧解压还原全部行
        let (text, end) = decompress_frames(&buf, &frames);
        assert_eq!(end, buf.len());
        assert_eq!(split_lines(&text), vec![r#"{"a":1}"#, r#"{"b":2}"#, r#"{"b":3}"#, r#"{"c":4}"#]);
        // torn 尾：第 4 帧只写一半 → 仍识别 3 完整帧
        let f4 = frame(&[r#"{"d":5}"#]);
        buf.extend_from_slice(&f4[..f4.len() / 2]);
        let frames = scan_zstd_frames(&buf);
        assert_eq!(frames.len(), 3);
        // 垃圾字节 → 0 帧（容错停在 0）
        assert!(scan_zstd_frames(b"garbage-bytes-here").is_empty());
    }

    #[test]
    fn advance_incremental_offset_and_truncation() {
        let dir = std::env::temp_dir().join("tokencalendar-test-dsh");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.v3.jsonl.zstd");
        let _ = std::fs::remove_file(&path);
        let mut file_bytes: Vec<u8> = Vec::new();
        file_bytes.extend_from_slice(&frame(&[r#"{"seq":1}"#]));
        std::fs::write(&path, &file_bytes).unwrap();

        let cursor = DshCursor::fresh();
        let c1 = advance_dsh_file(&path, true, &cursor).unwrap();
        assert_eq!(c1.lines, vec![r#"{"seq":1}"#]);
        assert!(!c1.reset);

        // generation 未变 → None
        let mut cursor = cursor;
        cursor.offset = c1.new_offset;
        cursor.size = file_bytes.len() as u64;
        cursor.mtime = super::super::jsonl::generation(&path).unwrap().1;
        assert!(advance_dsh_file(&path, true, &cursor).is_none());

        // 追加新帧 → 只推进增量
        file_bytes.extend_from_slice(&frame(&[r#"{"seq":2}"#, r#"{"seq":3}"#]));
        std::fs::write(&path, &file_bytes).unwrap();
        let c2 = advance_dsh_file(&path, true, &cursor).unwrap();
        assert_eq!(c2.lines, vec![r#"{"seq":2}"#, r#"{"seq":3}"#]);
        assert_eq!(c2.new_offset, file_bytes.len() as u64);
        // collect 每轮 seal 都会落 size/mtime——游标如实跟进。
        cursor.offset = c2.new_offset;
        cursor.size = file_bytes.len() as u64;
        cursor.mtime = super::super::jsonl::generation(&path).unwrap().1;

        // 截断 → 归零重读（reset 标记）。
        let small = frame(&[r#"{"seq":9}"#]);
        std::fs::write(&path, &small).unwrap();
        let c3 = advance_dsh_file(&path, true, &cursor).unwrap();
        assert!(c3.reset);
        assert_eq!(c3.lines, vec![r#"{"seq":9}"#]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn select_generation_picks_highest_version() {
        let dir = std::env::temp_dir().join("tokencalendar-test-dsh-gen");
        let sess = dir.join("session-abc");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("session.jsonl.zstd"), b"v0").unwrap();
        std::fs::write(sess.join("session.v3.jsonl.zstd"), b"v3").unwrap();
        std::fs::write(sess.join("session.lock"), b"lock").unwrap();
        std::fs::write(sess.join("notes.txt"), b"x").unwrap();
        let picked = select_generation(&sess).unwrap();
        assert!(picked.file_name().unwrap() == "session.v3.jsonl.zstd");
        // 只有 v0 时选 v0
        std::fs::remove_file(sess.join("session.v3.jsonl.zstd")).unwrap();
        let picked = select_generation(&sess).unwrap();
        assert!(picked.file_name().unwrap() == "session.jsonl.zstd");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- 冻结夹具端到端（官方 projcache 对账） ----------

    fn fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/collector/fixtures/dsh/session-closed.v3.jsonl.zstd")
    }

    /// 完整单会话流水线：帧解压 → 逐行解析 → 批聚合。
    /// 官方数字源：$DSH_HOME/storages/session_projcache/sessions/session-bdcf4e24….json
    /// 的 tokenUsage totals（DSH 官方投影自己算的答案）。
    fn run_fixture() -> (Batch, DshCursor) {
        let path = fixture_path();
        let mut cursor = DshCursor::fresh();
        let consume = advance_dsh_file(&path, true, &cursor).expect("fixture readable");
        let mut batch = Batch::default();
        for line in &consume.lines {
            match parse_line(line) {
                DshLine::Header { inherited } => cursor.inherited = inherited,
                DshLine::UserInput { seq } => {
                    if seq > cursor.last_seq && seq >= cursor.inherited {
                        cursor.pending_turn = true;
                    }
                }
                DshLine::Usage { seq, day, hour, model, input, output, total } => {
                    if seq > cursor.last_seq {
                        cursor.last_seq = seq;
                        if seq >= cursor.inherited {
                            let turns = if cursor.pending_turn {
                                cursor.pending_turn = false;
                                1
                            } else {
                                0
                            };
                            batch.add_hour(&day, Some(hour), META.id, &model, input, output, total, turns);
                        }
                    }
                }
                DshLine::None => {}
            }
        }
        cursor.offset = consume.new_offset;
        (batch, cursor)
    }

    #[test]
    fn fixture_closed_session_matches_official_projcache() {
        let (batch, cursor) = run_fixture();
        // 官方 projcache：uncachedInput=13003, output=91, cacheRead=16384
        assert_eq!(batch.events, 2);
        let mut input = 0;
        let mut output = 0;
        let mut total = 0;
        let mut turns = 0;
        for v in batch.entries.values() {
            input += v[0];
            output += v[1];
            total += v[2];
            turns += v[3];
        }
        assert_eq!(input, 13003);
        assert_eq!(output, 91);
        assert_eq!(total, 29478); // 13003 + 91 + 16384
        assert_eq!(turns, 2); // 两次测试对话 = 2 轮
        // 模型归一化：日志 id deepseek-v4-flash-ga-260731 → 通行名
        assert!(batch.entries.keys().any(|(_, _, m)| m == "deepseek-v4-flash"));
        assert!(!batch.entries.keys().any(|(_, _, m)| m.ends_with("-ga-260731")));
        // 闭合会话：无种子前缀,水位推进到末条 usage
        assert_eq!(cursor.inherited, 0);
        assert_eq!(cursor.last_seq, 24);
        assert!(!cursor.pending_turn);
        // 小时维与日维同入
        assert_eq!(batch.hourly.len(), batch.entries.len());
    }

    #[test]
    fn watermark_dedup_on_regenerated_log() {
        // 代际重编码场景：同一份日志重读（如代际切换 offset 归零）→ 水位去重,零重计。
        let (batch, cursor) = run_fixture();
        assert_eq!(batch.events, 2);
        let mut cursor = cursor;
        cursor.offset = 0; // 模拟代际切换归零
        let consume = advance_dsh_file(&fixture_path(), true, &cursor).unwrap();
        let mut batch2 = Batch::default();
        for line in &consume.lines {
            match parse_line(line) {
                DshLine::Header { inherited } => cursor.inherited = inherited,
                DshLine::UserInput { seq } => {
                    if seq > cursor.last_seq && seq >= cursor.inherited {
                        cursor.pending_turn = true;
                    }
                }
                DshLine::Usage { seq, day, hour, model, input, output, total } => {
                    if seq > cursor.last_seq {
                        cursor.last_seq = seq;
                        if seq >= cursor.inherited {
                            let turns = if cursor.pending_turn { cursor.pending_turn = false; 1 } else { 0 };
                            batch2.add_hour(&day, Some(hour), META.id, &model, input, output, total, turns);
                        }
                    }
                }
                DshLine::None => {}
            }
        }
        assert_eq!(batch2.events, 0, "水位去重:重读不得重计");
    }

    #[test]
    fn seeded_inherited_prefix_skipped() {
        // 种子（分叉）会话：inheritedEventCount=2,前缀 seq 1-2 是父会话副本
        // （含 usage 行与用户行）——不计数不置位；只有 own 事件（seq>=2）参与。
        let header = r#"{"type":"session","version":3,"id":"session-fork","createdAt":1,"isSeeded":true,"inheritedEventCount":2}"#;
        let inherited_usage = r#"{"type":"assistant/message","seq":1,"time":1788901890263,"data":{"turn":1,"step":1,"message":{"source":{"model":"glm-5-3-flash"}},"usage":{"inputTokens":100,"outputTokens":5,"totalTokens":105}}}"#;
        let inherited_user = r#"{"type":"user/message","seq":2,"time":1788901890264,"data":{"source":{"kind":"user"}}}"#;
        let own_user = r#"{"type":"user/message","seq":3,"time":1788901890265,"data":{"source":{"kind":"user"}}}"#;
        let own_usage = r#"{"type":"assistant/message","seq":4,"time":1788901890266,"data":{"turn":2,"step":1,"message":{"source":{"model":"glm-5-3-flash"}},"usage":{"inputTokens":200,"outputTokens":10,"totalTokens":210}}}"#;

        let mut cursor = DshCursor::fresh();
        let mut batch = Batch::default();
        for line in [header, inherited_usage, inherited_user, own_user, own_usage] {
            match parse_line(line) {
                DshLine::Header { inherited } => cursor.inherited = inherited,
                DshLine::UserInput { seq } => {
                    if seq > cursor.last_seq && seq >= cursor.inherited {
                        cursor.pending_turn = true;
                    }
                }
                DshLine::Usage { seq, day, hour, model, input, output, total } => {
                    if seq > cursor.last_seq {
                        cursor.last_seq = seq;
                        if seq >= cursor.inherited {
                            let turns = if cursor.pending_turn { cursor.pending_turn = false; 1 } else { 0 };
                            batch.add_hour(&day, Some(hour), META.id, &model, input, output, total, turns);
                        }
                    }
                }
                DshLine::None => {}
            }
        }
        assert_eq!(cursor.inherited, 2);
        // 继承前缀的 usage(100/5/105) 不计；own usage 计 1 轮
        assert_eq!(batch.events, 1);
        let v = batch.entries.values().next().unwrap();
        assert_eq!(v[0], 200);
        assert_eq!(v[1], 10);
        assert_eq!(v[3], 1); // own_user 置位 → own_usage 计 1 turn（继承 user 不置位）
        assert!(batch.entries.keys().any(|(_, _, m)| m == "glm-5.3-flash"));
    }
}
