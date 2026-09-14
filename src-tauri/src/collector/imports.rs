//! CodeBuddy 官网「用量导出」xlsx 导入。
//!
//! 背景：本地 index.json 的 `requests[]` **没有模型字段**（全文件关键词扫描零命中）,
//! 模型维度只能靠官网服务端账本补。官网导出表（Usage Details,字符串单元格）：
//! `RequestID | 积分消耗 | 模型 | 客户端 | 时间`,与本地 `requests[].id`
//! 同一 ID 空间（本机 354/359 = 98.6% 命中）。导出按月选择,重叠月份按
//! RequestID upsert 幂等（重叠行模型/时间零冲突,仅个别积分结算差异）。
//!
//! 机制：`<数据根>/imports/` 下的 `*.xlsx` 由采集线程每轮检查——解析、
//! 入 request_model 表、成功后改名 `.xlsx.done`（保留原始文件,重复导入靠
//! done 后缀天然跳过）。旧版 home 目录（`~/.tokencalendar/imports`）由
//! `migrate_legacy_dir` 在采集线程首轮自动搬入新根,搬完删空壳目录。
//!
//! **客户端过滤在此层不做的原则**：全量行入表,WorkBuddy
//! 行（共享积分账本混入）标注 client 保留,归属过滤责任在 codebuddy 适配器。
//!
//! 关联源失效策略：新增或模型正 > 0 → codebuddy daily_usage+游标清空重扫
//! （纯积分刷新不算变更,不触发重扫）。index.json 全量重读为秒级,代价可接受;
//! 其他四源不受影响。

use std::path::{Path, PathBuf};

use calamine::{Reader, Xlsx};

use super::store::Store;

/// 导入目录：`<数据根>/imports`。官网手动导出的 xlsx 丢进来即可。
pub fn imports_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    crate::data_root::current(app).ok().map(|r| r.imports_dir())
}

/// 旧版导入目录（home 下,发布数据架构前使用）。
fn legacy_imports_dir() -> Option<PathBuf> {
    super::home_dir().map(|h| h.join(".tokencalendar").join("imports"))
}

/// 旧位置一次性搬迁：`~/.tokencalendar/imports` 整体移入数据根（含 .done）,
/// 成功后删除旧父目录壳。旧目录不存在/搬迁失败都静默跳过（下轮再试或放弃,
/// 不阻塞采集）。采集线程启动时调用一次。
pub fn migrate_legacy_dir(new_root: &Path) {
    let Some(old) = legacy_imports_dir() else { return };
    if !old.is_dir() {
        return;
    }
    if std::fs::rename(&old, new_root).is_ok() {
        // 顺带清掉 ~/.tokencalendar 空壳（非空则留着,用户自管）
        if let Some(parent) = old.parent() {
            let _ = std::fs::remove_dir(parent);
        }
        eprintln!("[imports] legacy dir migrated: {} -> {}", old.display(), new_root.display());
    } else {
        // rename 失败（跨盘等）→ 逐文件拷贝兜底
        if std::fs::create_dir_all(new_root).is_err() {
            return;
        }
        let mut copied = false;
        if let Ok(entries) = std::fs::read_dir(&old) {
            for e in entries.flatten() {
                let dest = new_root.join(e.file_name());
                if std::fs::copy(e.path(), &dest).is_ok() {
                    let _ = std::fs::remove_file(e.path());
                    copied = true;
                }
            }
        }
        if copied {
            let _ = std::fs::remove_dir_all(old.parent().unwrap_or(&old));
            eprintln!("[imports] legacy dir migrated (copy-fallback) into {}", new_root.display());
        }
    }
}

pub struct ImportOutcome {
    /// 新增映射数（之前表里没有的 RequestID）。
    pub added: usize,
    /// 模型被正的既有映射数。
    pub corrected: usize,
    /// 本次处理的文件数。
    pub files: usize,
}

/// 处理 imports 目录：解析全部 `*.xlsx` → 入库 → 改名 `.done`。
/// 目录不存在 = 没启用对账,静默返回（零事件）。
pub fn process_imports(store: &mut Store, app: &tauri::AppHandle) -> ImportOutcome {
    let mut out = ImportOutcome { added: 0, corrected: 0, files: 0 };
    let Some(dir) = imports_dir(app) else { return out };
    let _ = std::fs::create_dir_all(&dir);
    let Ok(entries) = std::fs::read_dir(&dir) else { return out };

    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("xlsx"))
        .collect();
    files.sort();
    for path in files {
        match import_export_file(store, &path) {
            Ok((added, corrected)) => {
                out.added += added;
                out.corrected += corrected;
                out.files += 1;
                let done = path.with_extension("xlsx.done");
                if let Err(e) = std::fs::rename(&path, &done) {
                    eprintln!("[imports] rename {} failed: {}", path.display(), e);
                }
            }
            Err(e) => {
                // 单文件失败不拖垮整体（容错铁律）:留在原地,下轮重试。
                eprintln!("[imports] {} skipped: {}", path.display(), e);
            }
        }
    }
    out
}

type ExportRow = (String, String, Option<String>, String, Option<f64>);

/// 解析并入库单个导出文件，返回 （added, corrected)。
fn import_export_file(store: &mut Store, path: &Path) -> Result<(usize, usize), String> {
    let rows = parse_export(path)?;
    store.import_request_models("codebuddy", &rows)
}

/// 解析 CodeBuddy 官网导出 xlsx。
/// 跳过表头行与任何字段为空的行;不认识的表头视为非导出文件报错（防呆）。
pub fn parse_export(path: &Path) -> Result<Vec<ExportRow>, String> {
    let mut book: Xlsx<_> =
        calamine::open_workbook(path).map_err(|e| format!("open: {}", e))?;
    // 官方导出只有一个工作表;按名找,找不到取第一个。
    let range = match book.worksheet_range("Usage Details") {
        Ok(r) => r,
        Err(_) => book
            .worksheet_range_at(0)
            .ok_or("no worksheet")?
            .map_err(|e| format!("read sheet: {}", e))?,
    };

    let mut rows = Vec::new();
    for row in range.rows() {
        let get = |i: usize| {
            row.get(i)
                .and_then(|c| Some(c.to_string()))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        // day 必须可解析：真实数据行必有合法时间;表头行（"时间"两字)/空行/
        // 缺字段残行全部由此拦下。
        let (Some(id), Some(model), client, Some(day)) =
            (get(0), get(2), get(3), get(4).and_then(parse_export_day))
        else {
            continue;
        };
        let credit = get(1).and_then(|s| s.parse::<f64>().ok());
        rows.push((id, model, client, day, credit));
    }
    if rows.is_empty() {
        return Err("no usable rows (not an export file?)".into());
    }
    Ok(rows)
}

/// 官方「时间」列 = 北京时间 `YYYY-MM-DD HH:MM:SS` 字符串。
/// 取日期部分原样入库（official day 与本地 millis_to_local_day 同日:
/// 本地 startedAt UTC+8 后与该列同一天）,无需再时区换算。
fn parse_export_day(s: String) -> Option<String> {
    let d = s.get(..10)?;
    let mut parts = d.split('-');
    let (y, m, day) = (parts.next()?, parts.next()?, parts.next()?);
    if y.len() == 4 && m.len() == 2 && day.len() == 2 && d[4..5].chars().all(|c| c == '-')
        && d[7..8].chars().all(|c| c == '-')
    {
        Some(d.to_string())
    } else {
        None
    }
}

/// fixture 的 Data 引用（文件即字节,tests 用）。
#[cfg(test)]
pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("collector")
        .join("fixtures")
        .join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::store::Store;

    #[test]
    fn parses_sample_export() {
        let rows = parse_export(&fixture_path("codebuddy_export_sample.xlsx")).unwrap();
        assert_eq!(rows.len(), 4, "表头+空行应跳过,4 条数据行保留");
        let (id, model, client, day, credit) = &rows[0];
        assert_eq!(id, "aaaa1111bbbb2222cccc3333dddd4444");
        assert_eq!(model, "glm-5.3-flash");
        assert_eq!(client.as_deref(), Some("CodeBuddyIDE"));
        assert_eq!(*day, "2026-09-01");
        assert_eq!(*credit, Some(1.5));
        // WorkBuddy 行也入表（client 标注,归属过滤在适配器）
        assert_eq!(rows[2].2.as_deref(), Some("WorkBuddy"));
        // credit 整数"2" 也能解析
        assert_eq!(rows[1].4, Some(2.0));
    }

    #[test]
    fn import_then_noop_reimport() {
        let mut s = Store::open_in_memory().unwrap();
        let rows = parse_export(&fixture_path("codebuddy_export_sample.xlsx")).unwrap();
        let (added, corrected) = s.import_request_models("codebuddy", &rows).unwrap();
        assert_eq!((added, corrected), (4, 0));
        let (added, corrected) = s.import_request_models("codebuddy", &rows).unwrap();
        assert_eq!((added, corrected), (0, 0), "重导幂等");
    }

    #[test]
    fn day_parser_rejects_non_dates() {
        assert_eq!(parse_export_day("2026-09-01 10:00:00".into()).as_deref(), Some("2026-09-01"));
        assert_eq!(parse_export_day("2026-09-01".into()).as_deref(), Some("2026-09-01"));
        assert!(parse_export_day("nonsense".into()).is_none());
        assert!(parse_export_day("2026-9-1 10:00".into()).is_none());
        assert!(parse_export_day("20260901".into()).is_none());
    }
}
