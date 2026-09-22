//! 源文本解码:UTF-8 优先,非法字节段按系统 ANSI 代码页回退。
//!
//! 部分 Agent 在 Windows 上把工作目录等路径字段按系统代码页（中文系统 = GBK / CP936）写出,
//! 若按 UTF-8 lossy 解码会落成 U+FFFD 替换符,再经路径归一化就成了乱码 project_key。
//! JSONL 在 JSON 解析**之前**就要解码成字符串,所以回退发生在行级,但只作用于非法段:
//!
//! 1. 整行 `from_utf8` 成功 → 原样返回（正常路径,零拷贝）;
//! 2. 否则逐段处理：从非法字节向前退到本段非 ASCII 字节的起点,再按双字节代码页成对前进
//!    （lead ≥ 0x80 时连同下一字节一起取,GBK 的 trail 可落在 ASCII 区 0x40〜0x7E）,
//!    该段交给 `MultiByteToWideChar（CP_ACP)`;段外合法的 UTF-8 保持不变。
//!
//! 非 Windows 平台没有系统 ANSI 代码页的概念,回退为 UTF-8 lossy。

use std::borrow::Cow;

/// 字节 → 字符串:合法 UTF-8 原样;非法段按系统 ANSI 代码页解码。
pub fn decode_bytes(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => Cow::Owned(decode_mixed(bytes, ansi_decode)),
    }
}

/// 同 `decode_bytes`,但回退代码页由调用方给定（测试用固定 936,不依赖本机区域设置）。
#[cfg(all(test, windows))]
pub fn decode_bytes_with_codepage(bytes: &[u8], codepage: u32) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => decode_mixed(bytes, |seg| codepage_decode(seg, codepage)),
    }
}

fn decode_mixed(bytes: &[u8], fallback: impl Fn(&[u8]) -> String) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                return out;
            }
            Err(e) => {
                let bad = e.valid_up_to();
                // 退到非 ASCII 段起点（段内前面可能有「碰巧合法」的 UTF-8 片段,如 GBK「目」C4 BF）
                let mut start = bad;
                while start > 0 && rest[start - 1] >= 0x80 {
                    start -= 1;
                }
                // SAFETY 等价:[0, start) 是 [0, valid_up_to) 的前缀且止于 ASCII 边界,必为合法 UTF-8
                out.push_str(std::str::from_utf8(&rest[..start]).unwrap_or_default());
                let mut end = start;
                while end < rest.len() && rest[end] >= 0x80 {
                    end = (end + 2).min(rest.len());
                }
                out.push_str(&fallback(&rest[start..end]));
                rest = &rest[end..];
            }
        }
    }
}

#[cfg(windows)]
fn ansi_decode(seg: &[u8]) -> String {
    codepage_decode(seg, windows_sys::Win32::Globalization::CP_ACP)
}

#[cfg(windows)]
fn codepage_decode(seg: &[u8], codepage: u32) -> String {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;
    if seg.is_empty() {
        return String::new();
    }
    let Ok(len) = i32::try_from(seg.len()) else { return String::from_utf8_lossy(seg).into_owned() };
    // SAFETY: 输入指针 / 长度来自同一切片;第一次调用只取所需宽字符数,第二次写入等长缓冲。
    unsafe {
        let need = MultiByteToWideChar(codepage, 0, seg.as_ptr(), len, std::ptr::null_mut(), 0);
        if need <= 0 {
            return String::from_utf8_lossy(seg).into_owned();
        }
        let mut wide = vec![0u16; need as usize];
        let got = MultiByteToWideChar(codepage, 0, seg.as_ptr(), len, wide.as_mut_ptr(), need);
        if got <= 0 {
            return String::from_utf8_lossy(seg).into_owned();
        }
        String::from_utf16_lossy(&wide[..got as usize])
    }
}

#[cfg(not(windows))]
fn ansi_decode(seg: &[u8]) -> String {
    String::from_utf8_lossy(seg).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 冻结样本 1:UTF-8 中文路径（JSON 转义反斜杠）原样通过。
    const UTF8_LINE: &str = r#"{"cwd":"D:\\OneDrive\\文档\\knowledge","type":"session_meta"}"#;

    /// 冻结样本 2:同一路径的 GBK 字节（文 = CE C4,档 = B5 B5）。
    fn gbk_line() -> Vec<u8> {
        let mut v = br#"{"cwd":"D:\\OneDrive\\"#.to_vec();
        v.extend_from_slice(&[0xCE, 0xC4, 0xB5, 0xB5]);
        v.extend_from_slice(br#"\\knowledge","type":"session_meta"}"#);
        v
    }

    #[test]
    fn valid_utf8_is_borrowed_unchanged() {
        assert!(matches!(decode_bytes(UTF8_LINE.as_bytes()), Cow::Borrowed(s) if s == UTF8_LINE));
    }

    #[cfg(windows)]
    #[test]
    fn gbk_segment_decodes_with_codepage_936() {
        let s = decode_bytes_with_codepage(&gbk_line(), 936);
        assert_eq!(s, UTF8_LINE);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["cwd"].as_str(), Some(r"D:\OneDrive\文档\knowledge"));
    }

    #[cfg(windows)]
    #[test]
    fn mixed_line_keeps_utf8_and_decodes_gbk_only() {
        // UTF-8「项目」+ ASCII + GBK「目」(C4 BF,碰巧是合法 UTF-8 片段)「录」(C2 BC) + GBK trail 在 ASCII 区的「丂」(81 40)
        let mut v = "项目/".as_bytes().to_vec();
        v.extend_from_slice(&[0xC4, 0xBF, 0xC2, 0xBC, 0x81, 0x40]);
        v.extend_from_slice(b"/x");
        assert_eq!(decode_bytes_with_codepage(&v, 936), "项目/目录丂/x");
    }

    /// 本机系统代码页为 936 时,生产路径（CP_ACP）同样解出中文;其它代码页只要求不产出替换符以外的崩溃。
    #[cfg(windows)]
    #[test]
    fn system_ansi_codepage_path() {
        let acp = unsafe { windows_sys::Win32::Globalization::GetACP() };
        let s = decode_bytes(&gbk_line()).into_owned();
        if acp == 936 {
            assert_eq!(s, UTF8_LINE);
        } else {
            assert!(s.starts_with(r#"{"cwd":"D:\\OneDrive\\"#) && s.ends_with(r#""type":"session_meta"}"#));
        }
    }
}
