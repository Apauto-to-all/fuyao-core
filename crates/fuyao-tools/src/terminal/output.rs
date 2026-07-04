//! 输出处理
//!
//! 多编码解码、ANSI 转义清理、输出截断。

use crate::config::TERMINAL_MAX_OUTPUT_CHARS;
use regex::Regex;
use std::sync::LazyLock;

// =========== 多编码解码 ===========

/// 解码输出字节（尝试多种编码）
///
/// Windows 下可能遇到多种编码：
/// - UTF-8（Git Bash）
/// - GBK / cp936（Windows cmd）
/// - UTF-16 LE（WSL / PowerShell）
pub fn decode_output(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    // UTF-16 LE 检测（BOM 或偶数位置字节都是 \x00）
    let has_bom = bytes.starts_with(&[0xff, 0xfe]);
    let is_ascii_utf16le = bytes.len() >= 4
        && bytes.len().is_multiple_of(2)
        && bytes[1..].iter().step_by(2).all(|&b| b == 0);

    if (has_bom || is_ascii_utf16le)
        && let Ok(decoded) = String::from_utf16(
            &bytes
                .chunks(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk.get(1).copied().unwrap_or(0)]))
                .collect::<Vec<_>>(),
        )
    {
        return decoded.trim_start_matches('\u{feff}').to_string();
    }

    // UTF-8（Git Bash / Unix）
    if let Ok(decoded) = std::str::from_utf8(bytes) {
        return decoded.to_string();
    }

    // Windows cmd / PowerShell 使用 GBK (cp936)
    if cfg!(windows) {
        let (decoded, _, _) = encoding_rs::GBK.decode(bytes);
        if !decoded.is_empty() {
            return decoded.to_string();
        }
    }

    // 最后 fallback
    String::from_utf8_lossy(bytes).to_string()
}

// =========== ANSI 转义清理 ===========

static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").expect("无效的 ANSI 正则表达式"));

/// 移除 ANSI 转义序列
pub fn strip_ansi(text: &str) -> String {
    ANSI_RE.replace_all(text, "").to_string()
}

// =========== 输出截断 ===========

/// 截断过长输出，保留头部 40% + 尾部 60%
pub fn truncate_output(text: &str) -> String {
    if text.len() <= TERMINAL_MAX_OUTPUT_CHARS {
        return text.to_string();
    }

    let head_chars = (TERMINAL_MAX_OUTPUT_CHARS as f64 * 0.4) as usize;
    let tail_chars = TERMINAL_MAX_OUTPUT_CHARS - head_chars;
    let omitted = text.len() - head_chars - tail_chars;

    format!(
        "{}\n\n... [输出已截断 - 省略 {omitted} 字符 (总长 {})] ...\n\n{}",
        &text[..head_chars],
        text.len(),
        &text[text.len() - tail_chars..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_output_utf8() {
        let bytes = "hello world".as_bytes();
        let result = decode_output(bytes);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn decode_output_empty() {
        let result = decode_output(&[]);
        assert_eq!(result, "");
    }

    #[test]
    fn strip_ansi_basic() {
        let text = "\x1b[32mgreen\x1b[0m text";
        let result = strip_ansi(text);
        assert_eq!(result, "green text");
    }

    #[test]
    fn strip_ansi_no_codes() {
        let text = "plain text";
        assert_eq!(strip_ansi(text), "plain text");
    }

    #[test]
    fn truncate_output_short() {
        let text = "hello";
        let result = truncate_output(text);
        assert_eq!(result, "hello");
    }

    #[test]
    fn truncate_output_long() {
        let long = "a".repeat(TERMINAL_MAX_OUTPUT_CHARS + 1000);
        let result = truncate_output(&long);
        assert!(result.contains("输出已截断"));
        assert!(result.starts_with('a'));
        assert!(result.ends_with('a'));
    }
}
