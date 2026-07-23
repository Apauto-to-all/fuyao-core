//! 文本工具集
//!
//! 提供编辑相关的纯文本处理函数：行尾符保真、BOM（字节顺序标记）处理、
//! diff 压缩、换行符归一化。所有函数无状态、无副作用，独立于编辑主流程，
//! 便于单独测试与复用。

/// UTF-8 BOM 字符（零宽不换行空格 U+FEFF），Windows 部分编辑器会在文件首字节写入它
const BOM: char = '\u{FEFF}';

// ============================================================================
// 行尾符
// ============================================================================

/// 把所有换行符归一化为 LF（`\r\n` 和单独的 `\r` 都替换为 `\n`）
///
/// 用于生成 diff 等「仅展示」场景：CRLF 文件的 diff 若保留 `\r` 会每行末尾残留杂乱字符。
pub fn normalize_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// 检测文本使用的行尾风格
///
/// 以是否存在 `\r\n` 判定：有则按 CRLF 处理，否则按 LF。
/// 老式 Mac 的单独 `\r` 不单独处理（实际场景已罕见）。
pub fn detect_line_ending(s: &str) -> LineEnding {
    if s.contains("\r\n") {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

/// 行尾风格
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    /// Unix 风格 `\n`
    Lf,
    /// Windows 风格 `\r\n`
    Crlf,
}

impl LineEnding {
    /// 以该风格写出文本：先把所有换行归一化为 LF，再统一替换为目标风格
    pub fn apply(self, s: &str) -> String {
        let lf = normalize_line_endings(s);
        match self {
            LineEnding::Lf => lf,
            LineEnding::Crlf => lf.replace('\n', "\r\n"),
        }
    }
}

// ============================================================================
// BOM（字节顺序标记）
// ============================================================================

/// 剥离可能存在的 UTF-8 BOM
///
/// 返回 `(去除 BOM 后的内容, 是否原本带 BOM)`。
/// 借用原切片，无 BOM 时零拷贝返回原内容。
pub fn split_bom(s: &str) -> (&str, bool) {
    match s.strip_prefix(BOM) {
        Some(rest) => (rest, true),
        None => (s, false),
    }
}

/// 按 BOM 标记把内容写回
///
/// `has_bom` 为真时在内容前补回 `\u{FEFF}`，否则原样返回。
pub fn join_bom(s: &str, has_bom: bool) -> String {
    if has_bom {
        let mut out = String::with_capacity(s.len() + BOM.len_utf8());
        out.push(BOM);
        out.push_str(s);
        out
    } else {
        s.to_string()
    }
}

// ============================================================================
// diff 压缩
// ============================================================================

/// 压缩 unified diff 中各行的公共前导空白
///
/// 计算 hunk 内所有「+/-/空格」内容行的最小缩进，统一砍掉该缩进。
/// 代码通常有深层嵌套缩进，给 LLM 展示的 diff 若保留这些公共缩进会浪费 token 且可读性差。
///
/// 注意：仅砍公共缩进，行间相对缩进不变，diff 语义保持。`---`/`+++` 文件头行不动。
pub fn trim_common_indent(diff: &str) -> String {
    let lines: Vec<&str> = diff.split('\n').collect();

    // 统计 hunk 内容行（+/-/空格 开头，排除 --- / +++ 文件头）的最小前导空白数
    let min_indent = min_leading_indent(&lines);

    if min_indent == 0 {
        return diff.to_string();
    }

    lines
        .iter()
        .map(|line| trim_line_indent(line, min_indent))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 判断一行是否为 diff 内容行（+/-/空格 开头，但非 --- / +++ 文件头）
fn is_diff_content_line(line: &str) -> bool {
    let first = match line.chars().next() {
        Some(c) => c,
        None => return false,
    };
    if first != '+' && first != '-' && first != ' ' {
        return false;
    }
    // 排除 --- / +++ 文件头
    !(line.starts_with("---") || line.starts_with("+++"))
}

/// 计算 diff 内容行的最小前导空白数
fn min_leading_indent(lines: &[&str]) -> usize {
    lines
        .iter()
        .filter(|line| is_diff_content_line(line))
        .map(|line| {
            let after_prefix = &line[1..]; // 去掉 +/-/空格 前缀符号
            // 前导空白 = 总长 - 去掉前导空白后的长度
            after_prefix.len() - after_prefix.trim_start().len()
        })
        .min()
        .unwrap_or(0)
}

/// 砍掉单行的前导公共缩进（仅对 diff 内容行生效）
fn trim_line_indent(line: &str, indent: usize) -> String {
    if !is_diff_content_line(line) {
        return line.to_string();
    }
    let prefix = &line[..1]; // + / - / 空格
    let content = &line[1..];
    // 从 content 砍掉至多 indent 个空白字符
    let cut = content
        .char_indices()
        .take(indent)
        .take_while(|(_, c)| c.is_whitespace())
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{prefix}{}", &content[cut..])
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- 行尾符 ---

    #[test]
    fn normalize_crlf_and_cr() {
        assert_eq!(normalize_line_endings("a\r\nb\rc\n"), "a\nb\nc\n");
        assert_eq!(normalize_line_endings("a\nb\n"), "a\nb\n");
    }

    #[test]
    fn detect_ending() {
        assert_eq!(detect_line_ending("a\r\nb"), LineEnding::Crlf);
        assert_eq!(detect_line_ending("a\nb"), LineEnding::Lf);
        assert_eq!(detect_line_ending(""), LineEnding::Lf);
    }

    #[test]
    fn line_ending_apply_preserves_style() {
        // LF 文件替换后仍 LF
        assert_eq!(LineEnding::Lf.apply("a\nb\n"), "a\nb\n");
        // CRLF 文件：即使 new_content 是 LF，转回 CRLF
        assert_eq!(LineEnding::Crlf.apply("a\nb\n"), "a\r\nb\r\n");
        // 混入的 CRLF 也被先归一再转
        assert_eq!(LineEnding::Crlf.apply("a\r\nb\nc"), "a\r\nb\r\nc");
    }

    #[test]
    fn line_ending_roundtrip_crlf_file() {
        // 模拟 backend 场景：原文件 CRLF，fuzzy 替换得到 LF 的 new_content，写入前转回 CRLF
        let original = "enabled = false\r\nurl = \"x\"\r\n";
        let ending = detect_line_ending(original);
        let new_content_after_fuzzy = "enabled = true\nurl = \"x\"\n";
        let to_write = ending.apply(new_content_after_fuzzy);
        assert_eq!(to_write, "enabled = true\r\nurl = \"x\"\r\n");
    }

    // --- BOM ---

    #[test]
    fn split_bom_present() {
        let s = "\u{FEFF}hello";
        let (content, has_bom) = split_bom(s);
        assert!(has_bom);
        assert_eq!(content, "hello");
    }

    #[test]
    fn split_bom_absent() {
        let (content, has_bom) = split_bom("hello");
        assert!(!has_bom);
        assert_eq!(content, "hello");
    }

    #[test]
    fn join_bom_roundtrip() {
        let original = "\u{FEFF}内容";
        let (content, has_bom) = split_bom(original);
        assert_eq!(join_bom(content, has_bom), original);
        // 无 BOM 文件不加 BOM
        assert_eq!(join_bom("plain", false), "plain");
    }

    // --- trimDiff ---

    #[test]
    fn trim_indent_strips_common_leading_whitespace() {
        // 深嵌套代码的 diff：公共缩进 8 空格应被砍掉
        // 减号行是旧值 1，加号行是新值 2
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n-        let x = 1;\n+        let x = 2;\n         let y = 3;";
        let trimmed = trim_common_indent(diff);
        // 文件头不变
        assert!(trimmed.contains("--- a/f.rs"));
        assert!(trimmed.contains("+++ b/f.rs"));
        // 内容行公共缩进被砍：减号行是旧值 1，加号行是新值 2
        assert!(trimmed.contains("-let x = 1;"));
        assert!(trimmed.contains("+let x = 2;"));
        assert!(!trimmed.contains("        let x = 2;"));
        assert!(!trimmed.contains("        let x = 1;"));
    }

    #[test]
    fn trim_indent_no_change_when_no_common_indent() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1 +1 @@\n-a\n+b";
        let trimmed = trim_common_indent(diff);
        assert_eq!(trimmed, diff);
    }

    #[test]
    fn trim_indent_preserves_relative_indentation() {
        // 公共缩进砍掉，行间相对缩进保留
        let diff = "-    foo\n-        bar\n+    foo\n+        baz";
        let trimmed = trim_common_indent(diff);
        // 最小公共缩进是 4
        assert!(trimmed.contains("-foo"));
        assert!(trimmed.contains("-    bar"));
    }
}
