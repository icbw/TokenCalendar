//! 按源自己的存储结构定项目。
//!
//! Claude Code（`~/.claude/projects/<编码目录>/`）与 WorkBuddy（`~/.workbuddy/projects/<编码目录>/`）
//! 都把会话文件归档在「启动目录编码后的文件夹」下——续聊 / fork 副本、子代理目录都在同一文件夹里,
//! 源自己的会话列表也按这个文件夹分组。**项目身份 = 文件夹**;行内 `cwd` 只用来还原可读路径
//! （Claude 的 `cwd` 是 Bash 当前目录,`cd` 之后会漂进子目录,不能当身份用,
//! _project-key-drift）。
//!
//! 可读路径的解析顺序：本次采集已解析 → 库内持久化映射（`source_cursor` scope `folder:<文件夹>`）→
//! 文件里首个 `cwd` 且编码后与文件夹一致 → 旧游标里的目录且编码一致 → 文件夹名本身（无 cwd 行时的兜底,
//! 不持久化）。编码比较只看字母数字（大小写不敏感）：Claude 把非字母数字换成 `-`,WorkBuddy 再折叠连续
//! `-` 并小写盘符,两家都在此判据下一致。

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use super::store::{Batch, Store};
use super::turns::normalize_project;

const SCOPE_PREFIX: &str = "folder:";
/// 找首个 cwd 最多读的行数（真实文件首行就带 cwd;上限防止无 cwd 的巨型文件逐行解析）。
const CWD_SCAN_LINES: usize = 256;

/// 只保留字母数字并小写：编码目录名与原始路径的公共比较键。
fn alnum_key(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// 原始路径编码后是否等于该文件夹名。
pub fn folder_matches(dir: &str, folder: &str) -> bool {
    !folder.is_empty() && alnum_key(dir) == alnum_key(folder)
}

/// 是否像一条真实路径（含盘符或分隔符）。文件夹名兜底值（`E--Work-Demo`）不含二者——
/// 它编码后与自己的文件夹「匹配」,不加此判据会被当成路径持久化。
pub fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || s.contains(':')
}

/// 文件相对源根目录的第一层文件夹名（直接放在根目录下的文件 → None）。
pub fn folder_of(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut comps = rel.components();
    let first = comps.next()?;
    comps.next()?; // 至少还有文件名一层
    Some(first.as_os_str().to_string_lossy().into_owned())
}

/// 文件行里首个 `cwd` 字段（原始字符串,未归一化）。
pub fn first_cwd(lines: &[String]) -> Option<String> {
    lines.iter().take(CWD_SCAN_LINES).find_map(|l| {
        serde_json::from_str::<Value>(l).ok()?.get("cwd")?.as_str().filter(|s| !s.trim().is_empty()).map(str::to_string)
    })
}

/// 一次 collect 内的文件夹 → 项目键缓存（跨采集经 `source_cursor` 持久化）。
pub struct FolderProjects {
    agent: &'static str,
    resolved: HashMap<String, String>,
    dirty: Vec<(String, String)>,
}

impl FolderProjects {
    pub fn new(agent: &'static str) -> Self {
        FolderProjects { agent, resolved: HashMap::new(), dirty: Vec::new() }
    }

    /// 解析该文件所属项目键。`lines` = 本次读到的行;`hint` = 旧游标里的目录（可能为空）。
    pub fn resolve(&mut self, store: &Store, folder: &str, lines: &[String], hint: &str) -> String {
        if let Some(k) = self.resolved.get(folder) {
            return k.clone();
        }
        // 库内映射与旧游标提示都只在「像路径」时采纳:兜底写回的文件夹名不能自我印证（迁移会正旧值）。
        if let Some(k) = store.get_cursor(self.agent, &format!("{SCOPE_PREFIX}{folder}")).filter(|k| looks_like_path(k)) {
            self.resolved.insert(folder.to_string(), k.clone());
            return k;
        }
        let candidate = first_cwd(lines)
            .filter(|c| folder_matches(c, folder))
            .map(|c| normalize_project(&c))
            .or_else(|| (looks_like_path(hint) && folder_matches(hint, folder)).then(|| hint.to_string()));
        match candidate {
            Some(k) => {
                self.resolved.insert(folder.to_string(), k.clone());
                self.dirty.push((format!("{SCOPE_PREFIX}{folder}"), k.clone()));
                k
            }
            // 兜底:文件夹名本身作键（不缓存:同批稍后的文件可能带 cwd 而给出真实路径）
            None => folder.to_string(),
        }
    }

    /// 把本次新解析的映射随批次落库。
    pub fn persist(self, batch: &mut Batch) {
        batch.cursors.extend(self.dirty);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_comparison_covers_both_sources() {
        assert!(folder_matches(r"E:\Work\Demo", "E--Work-Demo"));
        assert!(folder_matches(r"E:\Work\Teen", "e--Work-Teen"), "盘符大小写不敏感");
        assert!(folder_matches(r"D:\Space\WorkBuddy\通用对话空间", "d-Space-WorkBuddy-通用对话空间"), "WorkBuddy 折叠 --");
        assert!(!folder_matches(r"E:\Work\Demo\src-tauri", "E--Work-Demo"), "子目录不匹配");
        assert!(!folder_matches(r"E:\Work\Demo", ""));
        assert!(folder_matches("/e/Work/Demo", "e--Work-Demo"), "Git Bash 形式 cwd");
    }

    #[test]
    fn folder_name_never_self_certifies() {
        let mut store = Store::open_in_memory().unwrap();
        let mut fp = FolderProjects::new("claude-code");
        let drifted = vec![r#"{"cwd":"E:\\W\\Demo\\src"}"#.to_string()];
        // 旧游标里是上次兜底写回的文件夹名:编码后与文件夹一致,但不是路径 → 仍兜底、不持久化
        assert_eq!(fp.resolve(&store, "E--W-Demo", &drifted, "E--W-Demo"), "E--W-Demo");
        let mut batch = Batch::default();
        fp.persist(&mut batch);
        assert!(batch.cursors.is_empty(), "文件夹名不得持久化为映射");
        // 库里已被写坏的映射（v13 漏洞遗留）视为不存在,由真实 cwd 覆盖
        let mut b2 = Batch::default();
        b2.cursors.push(("folder:E--W-Demo".to_string(), "E--W-Demo".to_string()));
        store.commit("claude-code", &b2).unwrap();
        let mut fp = FolderProjects::new("claude-code");
        let good = vec![r#"{"cwd":"E:\\W\\Demo"}"#.to_string()];
        assert_eq!(fp.resolve(&store, "E--W-Demo", &good, ""), "e:/W/Demo");
        let mut batch = Batch::default();
        fp.persist(&mut batch);
        assert_eq!(batch.cursors, vec![("folder:E--W-Demo".to_string(), "e:/W/Demo".to_string())], "覆盖坏映射");
    }

    #[test]
    fn folder_is_first_component_under_root() {
        let root = Path::new(r"C:\u\.claude\projects");
        assert_eq!(folder_of(root, Path::new(r"C:\u\.claude\projects\E--P\s.jsonl")).as_deref(), Some("E--P"));
        assert_eq!(folder_of(root, Path::new(r"C:\u\.claude\projects\E--P\s\subagents\a.jsonl")).as_deref(), Some("E--P"));
        assert_eq!(folder_of(root, Path::new(r"C:\u\.claude\projects\loose.jsonl")), None);
    }

    #[test]
    fn resolve_prefers_matching_cwd_then_hint_then_folder_name() {
        let store = Store::open_in_memory().unwrap();
        let mut fp = FolderProjects::new("claude-code");
        let drifted = vec![r#"{"cwd":"E:\\W\\Demo\\src"}"#.to_string()];
        // 首个 cwd 已漂进子目录、无提示 → 文件夹名兜底（不缓存）
        assert_eq!(fp.resolve(&store, "E--W-Demo", &drifted, ""), "E--W-Demo");
        // 旧游标提示匹配 → 采用并缓存
        assert_eq!(fp.resolve(&store, "E--W-Demo", &drifted, "e:/W/Demo"), "e:/W/Demo");
        assert_eq!(fp.resolve(&store, "E--W-Demo", &[], ""), "e:/W/Demo", "同批复用缓存");
        let good = vec![r#"{"cwd":"e:\\X\\Y"}"#.to_string()];
        assert_eq!(fp.resolve(&store, "E--X-Y", &good, ""), "e:/X/Y");
        let mut batch = Batch::default();
        fp.persist(&mut batch);
        assert_eq!(batch.cursors.len(), 2, "两个新解析的文件夹持久化");
    }
}
