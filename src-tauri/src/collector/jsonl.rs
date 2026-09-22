//! JSONL 公共设施：发现（mtime 升序）、generation（size-mtime）、增量 Tail。
//!
//! 增量语义：
//! - offset 永远停在「完整行边界」；半行留待下次（写到 EOF 才是行尾才算消费）。
//! - 文件变小（size < offset）= 截断/重写 → `Truncated`，调用方以 offset 0 重读
//!   （已聚合数据不回滚,属已知限制）。
//! - generation = （size, mtime_millis)：未变化直接跳过读取。

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// 单轮 Tail 结果。
pub enum Tail {
    /// 从 offset 正常续读到 EOF。
    Advanced { new_offset: u64, lines: Vec<String> },
    /// 文件比上次消费位置还小（截断/重写）。
    Truncated,
}

/// （size, mtime_millis)。文件不存在返回 None。
pub fn generation(path: &Path) -> Option<(u64, i64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some((meta.len(), mtime))
}

/// 递归收集 dir 下所有 *.jsonl（recursive=false 只看一层），按 mtime 升序。
/// 单个不可读目录静默跳过（容错铁律：发现失败不拖垮其他目录）。
pub fn discover(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    discover_named(dir, recursive, "jsonl", out);
}

/// 同 discover,但按「扩展名或完整文件名」匹配（如 CodeBuddy 的 index.json）。
pub fn discover_named(dir: &Path, recursive: bool, name: &str, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() && recursive {
            discover_named(&path, recursive, name, out);
        } else if ft.is_file() {
            let matches = path.extension().and_then(|e| e.to_str()) == Some(name)
                || path.file_name().and_then(|e| e.to_str()) == Some(name);
            if matches {
                out.push(path);
            }
        }
    }
}

/// 按 mtime 升序排序（旧文件先消费,让新数据后到、事件时间更接近提交时刻）。
pub fn sort_by_mtime(files: &mut [PathBuf]) {
    files.sort_by_key(|p| generation(p).map(|(_, m)| m).unwrap_or(0));
}

/// Claude 会话族排序键：（首个带 `uuid` 行的顶层 `timestamp`, 尾部最后一条带顶层 `timestamp` 行的时间),
/// 毫秒;找不到 → 0。只读头 1MB / 尾 256KB：头部是 custom-title / mode / file-history-snapshot 等
/// 无 uuid 的元数据行,首个 uuid 行偏移在几十 KB 以内。
pub fn head_tail_stamp(path: &Path) -> (i64, i64) {
    const HEAD: u64 = 1024 * 1024;
    const TAIL: u64 = 256 * 1024;
    let Ok(mut file) = fs::File::open(path) else { return (0, 0) };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let parse = |line: &[u8]| serde_json::from_slice::<serde_json::Value>(line).ok();
    let stamp = |v: &serde_json::Value| super::rfc3339_to_millis(v.get("timestamp")?.as_str()?);
    let mut head = Vec::new();
    if (&mut file).take(HEAD).read_to_end(&mut head).is_err() {
        return (0, 0);
    }
    let first = head
        .split(|&b| b == b'\n')
        .filter(|l| l.windows(6).any(|w| w == b"\"uuid\"")) // 便宜预筛,再按顶层键确认
        .filter_map(parse)
        .filter(|v| v.get("uuid").is_some())
        .find_map(|v| stamp(&v))
        .unwrap_or(0);
    let start = size.saturating_sub(TAIL);
    let mut tail = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut tail).is_err() {
        return (first, 0);
    }
    let mut lines: Vec<&[u8]> = tail.split(|&b| b == b'\n').collect();
    if start > 0 {
        lines.remove(0); // 起点落在行中间:首段是残行
    }
    let last = lines.iter().rev().filter_map(|l| parse(l)).find_map(|v| stamp(&v)).unwrap_or(0);
    (first, last)
}

/// 从 start_offset 续读一个 JSONL 文件到 EOF。
/// 单轮字节上限保护：超过 max_bytes 停下（offset 停在完整行边界）,下轮续读。
pub fn tail(path: &Path, start_offset: u64, max_bytes: u64) -> std::io::Result<Tail> {
    let size = fs::metadata(path)?.len();
    if size < start_offset {
        return Ok(Tail::Truncated);
    }
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(start_offset))?;
    let limit = size.min(start_offset.saturating_add(max_bytes));

    let mut buf = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 64 * 1024];
    let mut consumed = start_offset;
    loop {
        let want = (limit - consumed).min(chunk.len() as u64) as usize;
        if want == 0 {
            break;
        }
        let n = file.read(&mut chunk[..want])?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        consumed += n as u64;
    }

    // 只保留完整行：最后一段无换行的残行留给下次
    let complete_end = match buf.iter().rposition(|&b| b == b'\n') {
        Some(pos) => pos + 1,
        None => 0,
    };
    let new_offset = start_offset + complete_end as u64;
    // 逐行解码:UTF-8 优先,非法段按系统 ANSI 代码页回退（见 collector/text.rs）。
    // 与 str:lines 同语义:完整行以换行结尾,split 末尾多出的空段丢弃,中间空行保留。
    let mut lines: Vec<String> = buf[..complete_end]
        .split(|&b| b == b'\n')
        .map(|l| super::text::decode_bytes(l).trim_end_matches('\r').to_string())
        .collect();
    lines.pop();
    Ok(Tail::Advanced { new_offset, lines })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempFile(PathBuf);
    impl TempFile {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join("tokencalendar-test-jsonl");
            fs::create_dir_all(&dir).unwrap();
            let p = dir.join(name);
            let _ = fs::remove_file(&p);
            TempFile(p)
        }
        fn write(&self, s: &str) {
            let mut f = fs::OpenOptions::new().create(true).append(true).open(&self.0).unwrap();
            f.write_all(s.as_bytes()).unwrap();
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn tail_incremental_and_partial_line() {
        let t = TempFile::new("inc.jsonl");
        t.write("{\"a\":1}\n{\"a\":2}\n{\"a\":3"); // 第三行不完整
        let (off, lines) = match tail(&t.0, 0, u64::MAX).unwrap() {
            Tail::Advanced { new_offset, lines } => (new_offset, lines),
            _ => panic!("expected advanced"),
        };
        assert_eq!(lines.len(), 2); // 残行不算
        t.write("}\n{\"a\":4}\n");
        match tail(&t.0, off, u64::MAX).unwrap() {
            Tail::Advanced { new_offset, lines } => {
                assert_eq!(lines, vec!["{\"a\":3}", "{\"a\":4}"]);
                assert_eq!(new_offset, off + "{\"a\":3}\n".len() as u64 + "{\"a\":4}\n".len() as u64);
            }
            _ => panic!("expected advanced"),
        }
    }

    #[test]
    fn tail_detects_truncation() {
        let t = TempFile::new("trunc.jsonl");
        t.write("line1\nline2\nline3\n");
        let off = match tail(&t.0, 0, u64::MAX).unwrap() {
            Tail::Advanced { new_offset, .. } => new_offset,
            _ => panic!(),
        };
        // 截断重写：只剩一行
        fs::write(&t.0, "fresh\n").unwrap();
        assert!(matches!(tail(&t.0, off, u64::MAX).unwrap(), Tail::Truncated));
    }

    #[test]
    fn tail_respects_max_bytes() {
        let t = TempFile::new("max.jsonl");
        t.write("aaa\nbbb\nccc\n");
        match tail(&t.0, 0, 4).unwrap() {
            // 上限 4：读到 "aaa\nb"，完整行只有 "aaa"
            Tail::Advanced { new_offset, lines } => {
                assert_eq!(lines, vec!["aaa"]);
                assert_eq!(new_offset, 4);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn head_tail_stamp_skips_metadata_and_partial_lines() {
        let t = TempFile::new("stamp.jsonl");
        t.write("{\"type\":\"custom-title\",\"customTitle\":\"x\"}\n");
        t.write("{\"type\":\"queue-operation\",\"timestamp\":\"2026-09-16T09:00:00.000Z\"}\n");
        t.write("{\"type\":\"user\",\"uuid\":\"u1\",\"timestamp\":\"2026-09-16T09:10:52.759Z\"}\n");
        t.write("{\"type\":\"assistant\",\"uuid\":\"a1\",\"timestamp\":\"2026-09-16T09:11:00.000Z\"}\n");
        t.write("{\"type\":\"file-history-snapshot\",\"snapshot\":{\"timestamp\":\"2026-09-16T12:00:00.000Z\"}}\n");
        t.write("{\"type\":\"last-prompt\"}\n");
        let (first, last) = head_tail_stamp(&t.0);
        assert_eq!(first, super::super::rfc3339_to_millis("2026-09-16T09:10:52.759Z").unwrap(), "首个带 uuid 行,不取 queue-operation");
        assert_eq!(last, super::super::rfc3339_to_millis("2026-09-16T09:11:00.000Z").unwrap(), "尾部只认顶层 timestamp");
        assert_eq!(head_tail_stamp(Path::new("definitely/missing.jsonl")), (0, 0));
    }

    #[test]
    fn discover_finds_jsonl_recursive_and_sorted() {
        let dir = std::env::temp_dir().join("tokencalendar-test-disc");
        let a = dir.join("a.jsonl");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(&a, "x\n").unwrap();
        fs::write(sub.join("b.jsonl"), "y\n").unwrap();
        fs::write(dir.join("c.txt"), "z").unwrap();
        let mut out = Vec::new();
        discover(&dir, true, &mut out);
        assert_eq!(out.len(), 2);
        // recursive=false 只看一层
        let mut out2 = Vec::new();
        discover(&dir, false, &mut out2);
        assert_eq!(out2.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
