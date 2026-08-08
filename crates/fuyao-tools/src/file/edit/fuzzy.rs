//! 模糊替换策略
//!
//! 提供多种文本匹配策略，处理空白、缩进、换行等差异。
//!
//! ## 核心设计：策略返回「原串字节偏移 + 匹配长度」，统一替换
//!
//! 每个匹配策略（`MatchStrategy`）无论内部用什么归一化方式去**判断**是否匹配，
//! 最终必须返回**原始 content 中真实存在的若干段匹配**，每段用 `(字节偏移, 字节长度)` 表示。
//!
//! 关键约束：偏移量与长度的计算**只基于原始 content 的行结构**（`split('\n')` 后按行号累加，
//! 每行加 1 个换行符）。因为：
//! - 行首永远是 `\n` 的下一个字符，天然落在字符边界（`\n` 是单字节 ASCII）；
//! - 匹配长度取自【原始】行块的字节长度，归一化导致的字节长度变化不会影响这里的长度计算。
//!
//! 这从架构上规避了"归一化坐标 → 原始坐标映射"这一层（变长归一化下逐字节对齐极易 panic）。
//! 多字节字符（如中文）永远不会被切到字符内部，因为所有偏移都锚定在行首或精确子串位置。
//!
//! ## 策略列表（按优先级）
//!
//! 1. **exact**: 精确匹配
//! 2. **newline_normalized**: 换行符归一化（\r\n → \n）
//! 3. **line_trimmed**: 行首尾空白去除后匹配
//! 4. **whitespace_normalized**: 连续空白归一化为单空格
//! 5. **indentation_flexible**: 忽略前导缩进差异
//! 6. **escape_normalized**: 转义字符归一化（\\n → \n）
//! 7. **trimmed_boundary**: 首尾行空白处理后匹配
//! 8. **unicode_normalized**: Unicode 标准化（智能引号、全角字符）
//! 9. **block_anchor**: 首尾行锚定 + 中间内容相似度匹配

use regex::Regex;
use similar::TextDiff;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;

/// 策略 4 用的连续空白归一化正则
///
/// 模式为编译期常量，提升为进程级静态量只编译一次，避免每次模糊匹配重复编译。
static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+").expect("无效的空白正则"));

/// 一次匹配：原始 content 中的字节偏移与匹配段的字节长度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Match(usize, usize);

/// 匹配策略函数类型
///
/// 返回原始 content 中真实存在的若干段匹配，每段用 `(字节偏移, 字节长度)` 表示。
/// 偏移量锚定在行首或精确子串位置，天然是字符边界。
type MatchStrategy = fn(&str, &str) -> Vec<Match>;

/// 模糊查找并替换
///
/// 使用多种策略尝试匹配，处理空白、缩进等差异。
/// 按策略优先级依次尝试，首个有匹配的策略生效。
///
/// # 返回
///
/// `(新内容, 匹配数, 使用的策略名称, 错误信息)`
pub fn fuzzy_find_and_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> (String, usize, Option<String>, Option<String>) {
    if old_string.is_empty() {
        return (
            content.to_string(),
            0,
            None,
            Some("old_string 不能为空".to_string()),
        );
    }

    if old_string == new_string {
        return (
            content.to_string(),
            0,
            None,
            Some("old_string 和 new_string 相同".to_string()),
        );
    }

    let strategies: Vec<(&str, MatchStrategy)> = vec![
        ("exact", strategy_exact),
        ("newline_normalized", strategy_newline_normalized),
        ("line_trimmed", strategy_line_trimmed),
        ("whitespace_normalized", strategy_whitespace_normalized),
        ("indentation_flexible", strategy_indentation_flexible),
        ("escape_normalized", strategy_escape_normalized),
        ("trimmed_boundary", strategy_trimmed_boundary),
        ("unicode_normalized", strategy_unicode_normalized),
        ("block_anchor", strategy_block_anchor),
    ];

    for (strategy_name, strategy_fn) in strategies {
        let matches = strategy_fn(content, old_string);

        if !matches.is_empty() {
            if matches.len() > 1 && !replace_all {
                return (
                    content.to_string(),
                    0,
                    None,
                    Some(format!(
                        "找到 {} 处匹配。请提供更多上下文使其唯一，或使用 replace_all=true。",
                        matches.len()
                    )),
                );
            }

            // 护栏：宽容策略（尤以 block_anchor）可能圈出远大于 old_string 的匹配块，
            // 此时若放行会用 new_string 覆盖掉 AI 未声明要改的大量内容。
            // 只比较「实际匹配块」与「old_string」，与 new_string 无关，
            // 因此「3 行 old 改成 100 行 new」不会被误拦。
            for &Match(start, len) in &matches {
                let search = &content[start..start + len];
                if is_disproportionate_match(search, old_string) {
                    return (
                        content.to_string(),
                        0,
                        None,
                        Some(
                            "匹配范围远大于 old_string，可能匹配到无关内容。\
                             请重新读取文件，提供完整的 old_string 以精确匹配。"
                                .to_string(),
                        ),
                    );
                }
            }

            let new_content = apply_replacements(content, &matches, new_string);
            return (
                new_content,
                matches.len(),
                Some(strategy_name.to_string()),
                None,
            );
        }
    }

    (
        content.to_string(),
        0,
        None,
        Some("未找到匹配的文本".to_string()),
    )
}

/// 判断实际匹配块是否远大于 old_string（防误改护栏）
///
/// 宽容策略（尤以 block_anchor：首尾行锚定 + 中间相似度）可能在原文件里圈出
/// 比 old_string 大很多的块——首尾行对上了，中间跨过了 AI 未声明要改的无关行。
/// 若放行，AI 写的简短 new_string 会覆盖掉这大段无辜内容。
///
/// **只比较 `search`（实际匹配块）与 `old_string`，完全不看 `new_string`**。
/// 因此「old 3 行 → new 100 行」这种正常的扩写不会被误拦（search 仍是 3 行）；
/// 只有当 search（被圈中的旧块）远超 old_string 才触发——此时是匹配器过度放宽。
///
/// 判定规则（行数维度与字符维度双重判定，取严）：
/// - 行数维度：匹配块行数 ≥ max(oldLines+3, oldLines×2) → 拒绝
/// - 字符维度（单行 old 跳过，否则单行匹配总被误拒）：
///   匹配块 trim 后长度 > max(oldTrim+500, oldTrim×4) → 拒绝
fn is_disproportionate_match(search: &str, old_string: &str) -> bool {
    let old_lines = old_string.lines().count().max(1);
    let search_lines = search.lines().count().max(1);

    // 行数维度：匹配块行数远超声明
    if search_lines >= old_lines + 3 && search_lines >= old_lines * 2 {
        return true;
    }

    // 单行 old 不做字符维度判断（否则单行 old 的小扩写总被误拒）
    if old_lines == 1 {
        return false;
    }

    // 字符维度：匹配块字符数远超声明
    let old_trim_len = old_string.trim().len();
    let search_trim_len = search.trim().len();
    search_trim_len > old_trim_len + 500 && search_trim_len > old_trim_len * 4
}

/// 应用替换（从后向前替换，避免位置偏移）
///
/// 防御性处理：`Match` 的偏移来自行首累加或精确子串位置，天然是字符边界。
/// 这里仍用 `floor_char_boundary` 做一层兜底保护（理论上 no-op）。
fn apply_replacements(content: &str, matches: &[Match], new_string: &str) -> String {
    let mut sorted: Vec<Match> = matches.to_vec();
    // 从后向前：前面的替换不会移动后面替换的位置
    sorted.sort_by_key(|m| std::cmp::Reverse(m.0));

    let mut result = content.to_string();
    for Match(start, len) in sorted {
        let end = (start + len).min(result.len());
        let safe_start = floor_char_boundary(&result, start);
        let safe_end = floor_char_boundary(&result, end);
        if safe_start >= safe_end {
            continue;
        }
        result = format!(
            "{}{}{}",
            &result[..safe_start],
            new_string,
            &result[safe_end..]
        );
    }
    result
}

/// 将字节索引回退到最近的字符边界（floor 方向）
///
/// 给定任意字节索引，返回 <= 它的最大字符边界。
/// 用于修正非字符边界的切片索引，避免 panic。
fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    // s.is_char_boundary(idx) 为 true 当 idx 是 UTF-8 字符的起始字节
    while !s.is_char_boundary(idx) && idx > 0 {
        idx -= 1;
    }
    idx
}

// ============================================================================
// 辅助：把行号区间换算为原串中的字节偏移区间
// ============================================================================

/// 将「前若干行的字节长度累加」得到指定行在原串中的起始字节偏移
///
/// 用于按行匹配的策略：判断用归一化行，定位时累加【原始行】长度重建原串偏移，
/// 从而直接得到原串匹配区间，绕开"归一化坐标 → 原始坐标映射"。
fn lines_to_byte_offset(lines: &[&str], line_idx: usize) -> usize {
    lines[..line_idx].iter().map(|l| l.len() + 1).sum()
}

/// 去掉行尾的 '\r'（\r\n → \n 后 split('\n') 会在行尾留下 '\r'）
fn trim_cr(s: &str) -> &str {
    s.strip_suffix('\r').unwrap_or(s)
}

/// 按行号区间计算原串中的字节匹配区间 `(起始偏移, 长度)`
fn lines_to_match(
    content: &str,
    content_lines: &[&str],
    start_line: usize,
    line_count: usize,
) -> Match {
    let start = lines_to_byte_offset(content_lines, start_line);
    let end_line = (start_line + line_count).min(content_lines.len());
    let end = lines_to_byte_offset(content_lines, end_line).min(content.len());
    Match(start, end - start)
}

// ============================================================================
// 策略实现：每个策略判断匹配，返回原串字节匹配区间
// ============================================================================

/// 策略 1: 精确匹配
///
/// 返回原串中所有非重叠精确匹配的字节区间。`str::find` 与 `pattern.len()` 都是字符边界。
fn strategy_exact(content: &str, pattern: &str) -> Vec<Match> {
    let mut matches = Vec::new();
    let mut start = 0;
    while let Some(pos) = content[start..].find(pattern) {
        let abs_pos = start + pos;
        matches.push(Match(abs_pos, pattern.len()));
        // 按字符推进：跳过匹配起点的这一个字符，保证 start 落在字符边界
        // （+1 字节在多字节字符上会落到字符中间，下次切片会 panic）
        let next_char_len = content[abs_pos..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
        start = abs_pos + next_char_len;
    }
    matches
}

/// 策略 2: 换行符归一化（\r\n → \n）
///
/// 处理 Windows 和 Unix 换行符差异。归一化比较（去掉行尾 '\r'）后通过行号锚定
/// 回到原串计算字节区间，避免字节坐标映射。
fn strategy_newline_normalized(content: &str, pattern: &str) -> Vec<Match> {
    let content_has_crlf = content.contains("\r\n") || content.contains('\r');
    let pattern_has_crlf = pattern.contains("\r\n") || pattern.contains('\r');
    if !content_has_crlf && !pattern_has_crlf {
        return Vec::new();
    }

    let content_lines: Vec<&str> = content.split('\n').collect();
    let pattern_lines: Vec<&str> = pattern.split('\n').collect();
    let pattern_line_count = pattern_lines.len();
    if pattern_line_count == 0 || pattern_line_count > content_lines.len() {
        return Vec::new();
    }

    // 比较时去掉行尾的 '\r'（\r\n → \n 后 split('\n') 会留下行尾 '\r'）
    let norm_pattern_lines: Vec<&str> = pattern_lines.iter().map(|l| trim_cr(l)).collect();

    let mut matches = Vec::new();
    for i in 0..=content_lines.len().saturating_sub(pattern_line_count) {
        let content_block: Vec<&str> = (0..pattern_line_count)
            .map(|j| trim_cr(content_lines[i + j]))
            .collect();
        if content_block == norm_pattern_lines {
            matches.push(lines_to_match(
                content,
                &content_lines,
                i,
                pattern_line_count,
            ));
        }
    }
    matches
}

/// 策略 3: 行首尾空白去除后匹配
///
/// 去除每行的首尾空白后进行逐行匹配，适用于缩进不一致的场景。
fn strategy_line_trimmed(content: &str, pattern: &str) -> Vec<Match> {
    let pattern_lines: Vec<&str> = pattern.split('\n').collect();
    let pattern_normalized: Vec<&str> = pattern_lines.iter().map(|l| l.trim()).collect();
    let pattern_normalized_joined = pattern_normalized.join("\n");

    let content_lines: Vec<&str> = content.split('\n').collect();
    let content_normalized_lines: Vec<String> =
        content_lines.iter().map(|l| l.trim().to_string()).collect();

    find_normalized_matches(
        content,
        &content_lines,
        &content_normalized_lines,
        &pattern_normalized_joined,
    )
}

/// 策略 4: 连续空白归一化为单空格
///
/// 将多个空格/制表符合并为单个空格，适用于格式化差异。
fn strategy_whitespace_normalized(content: &str, pattern: &str) -> Vec<Match> {
    let normalize = |s: &str| WS_RE.replace_all(s, " ").to_string();

    let content_normalized = normalize(content);
    let pattern_normalized = normalize(pattern);

    if content_normalized == content && pattern_normalized == pattern {
        return Vec::new();
    }

    // 归一化后按行比较，用行号锚定回原串计算字节区间
    let content_lines: Vec<&str> = content.split('\n').collect();
    let content_norm_lines: Vec<String> = content_lines.iter().map(|l| normalize(l)).collect();
    find_normalized_matches(
        content,
        &content_lines,
        &content_norm_lines,
        &pattern_normalized,
    )
}

/// 策略 5: 忽略前导缩进差异
///
/// 只去除行首空白（保留行内空白），适用于缩进级别不同的代码块。
fn strategy_indentation_flexible(content: &str, pattern: &str) -> Vec<Match> {
    let content_lines: Vec<&str> = content.split('\n').collect();
    let content_stripped_lines: Vec<String> = content_lines
        .iter()
        .map(|l| l.trim_start().to_string())
        .collect();
    let pattern_lines: Vec<String> = pattern
        .split('\n')
        .map(|l| l.trim_start().to_string())
        .collect();

    find_normalized_matches(
        content,
        &content_lines,
        &content_stripped_lines,
        &pattern_lines.join("\n"),
    )
}

/// 策略 6: 转义字符归一化
///
/// 处理转义字符差异，如 `\\n` → `\n`, `\\t` → `\t`。
///
/// 转义归一化会破坏行结构（一行里的 `\\n` 反转义成真实换行后跨行），
/// 因此不能按行块比较，改用全文归一化后子串查找 + 字符级偏移映射回原串。
fn strategy_escape_normalized(content: &str, pattern: &str) -> Vec<Match> {
    let normalize_escapes = |s: &str| -> String {
        s.replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\r", "\r")
            .replace("\\\"", "\"")
            .replace("\\'", "'")
            .replace("\\\\", "\\")
    };

    let pattern_normalized = normalize_escapes(pattern);

    find_normalized_substring_matches(content, normalize_escapes, &pattern_normalized)
}

/// 策略 7: 首尾行空白处理后匹配
///
/// 去除首尾行的空白后进行锚定匹配，中间行也去除空白后比较。
fn strategy_trimmed_boundary(content: &str, pattern: &str) -> Vec<Match> {
    let pattern_lines: Vec<&str> = pattern.split('\n').collect();
    let content_lines: Vec<&str> = content.split('\n').collect();

    if pattern_lines.is_empty() {
        return Vec::new();
    }

    let pattern_first = pattern_lines[0].trim();
    let pattern_last = pattern_lines.last().unwrap().trim();

    let mut matches = Vec::new();
    let pattern_len = pattern_lines.len();

    for i in 0..=content_lines.len().saturating_sub(pattern_len) {
        let content_first = content_lines[i].trim();
        let content_last = content_lines[i + pattern_len - 1].trim();

        if content_first == pattern_first && content_last == pattern_last {
            if pattern_len <= 2 {
                matches.push(lines_to_match(content, &content_lines, i, pattern_len));
            } else {
                let content_middle: String = content_lines[i + 1..i + pattern_len - 1]
                    .iter()
                    .map(|l| l.trim())
                    .collect::<Vec<_>>()
                    .join("\n");
                let pattern_middle: String = pattern_lines[1..pattern_len - 1]
                    .iter()
                    .map(|l| l.trim())
                    .collect::<Vec<_>>()
                    .join("\n");

                if content_middle == pattern_middle {
                    matches.push(lines_to_match(content, &content_lines, i, pattern_len));
                }
            }
        }
    }

    matches
}

/// 策略 8: Unicode 标准化
///
/// 处理智能引号（' → '）、全角字符（Ａ → A）、不间断空格等 Unicode 差异。
///
/// 分两路查找：
/// - **整行块匹配**：pattern 对齐若干完整行时，按行号锚定回原串（行首是字符边界，安全）；
/// - **行内子串匹配**：pattern 是某行内的一段时（如全角字符夹在中文中间），
///   全文归一化后子串查找 + 字符级偏移映射回原串（全角→半角在 char 级是 1:1，映射安全）。
fn strategy_unicode_normalized(content: &str, pattern: &str) -> Vec<Match> {
    let normalize_unicode = |s: &str| -> String {
        let mut result: String = s.nfc().collect();

        let replacements = [
            ('\u{2018}', '\''),
            ('\u{2019}', '\''),
            ('\u{201c}', '"'),
            ('\u{201d}', '"'),
            ('\u{2013}', '-'),
            ('\u{2014}', '-'),
            ('\u{2026}', '.'),
            ('\u{00a0}', ' '),
            ('\u{3000}', ' '),
        ];

        for (old, new) in replacements {
            result = result.replace(old, &new.to_string());
        }

        // Fullwidth → ASCII
        result
            .chars()
            .map(|c| {
                let code = c as u32;
                if (0xFF01..=0xFF5E).contains(&code) {
                    char::from_u32(code - 0xFEE0).unwrap_or(c)
                } else {
                    c
                }
            })
            .collect()
    };

    let content_normalized = normalize_unicode(content);
    let pattern_normalized = normalize_unicode(pattern);

    if content_normalized == content && pattern_normalized == pattern {
        return Vec::new();
    }

    // 第一路：整行块匹配（pattern 对齐完整行时命中）
    let content_lines: Vec<&str> = content.split('\n').collect();
    let content_norm_lines: Vec<String> =
        content_lines.iter().map(|l| normalize_unicode(l)).collect();
    let block_matches = find_normalized_matches(
        content,
        &content_lines,
        &content_norm_lines,
        &pattern_normalized,
    );

    if !block_matches.is_empty() {
        return block_matches;
    }

    // 第二路：行内子串匹配（pattern 是某行内的一段）
    find_normalized_substring_matches(content, normalize_unicode, &pattern_normalized)
}

/// 策略 9: 首尾行锚定 + 中间内容相似度匹配
///
/// 用首尾行定位候选位置，中间内容用 TextDiff 计算相似度（阈值 0.50/0.70）。
/// 唯一匹配时阈值较低（0.50），多个候选时阈值较高（0.70）。
fn strategy_block_anchor(content: &str, pattern: &str) -> Vec<Match> {
    let pattern_lines: Vec<&str> = pattern.split('\n').collect();
    if pattern_lines.len() < 2 {
        return Vec::new();
    }

    let first_line = pattern_lines[0].trim();
    let last_line = pattern_lines.last().unwrap().trim();

    let content_lines: Vec<&str> = content.split('\n').collect();
    let pattern_line_count = pattern_lines.len();

    let mut potential_matches = Vec::new();
    for i in 0..=content_lines.len().saturating_sub(pattern_line_count) {
        if content_lines[i].trim() == first_line
            && content_lines[i + pattern_line_count - 1].trim() == last_line
        {
            potential_matches.push(i);
        }
    }

    let mut matches = Vec::new();
    let threshold = if potential_matches.len() == 1 {
        0.50
    } else {
        0.70
    };

    for i in potential_matches {
        let similarity = if pattern_line_count <= 2 {
            1.0
        } else {
            let content_middle: String =
                content_lines[i + 1..i + pattern_line_count - 1].join("\n");
            let pattern_middle: String = pattern_lines[1..pattern_line_count - 1].join("\n");
            let diff = TextDiff::from_lines(&content_middle, &pattern_middle);
            diff.ratio() as f64
        };

        if similarity >= threshold {
            matches.push(lines_to_match(
                content,
                &content_lines,
                i,
                pattern_line_count,
            ));
        }
    }

    matches
}

/// 在归一化后的行序列中查找匹配，命中后用行号锚定回原串计算字节区间
///
/// 判断用归一化后的行（`content_normalized_lines`），但定位时基于【原始行】
/// （`content_lines`）计算字节偏移，从而直接得到原串匹配区间，绕开坐标映射。
fn find_normalized_matches(
    content: &str,
    content_lines: &[&str],
    content_normalized_lines: &[String],
    pattern_normalized: &str,
) -> Vec<Match> {
    let pattern_norm_lines: Vec<&str> = pattern_normalized.split('\n').collect();
    let num_pattern_lines = pattern_norm_lines.len();
    if num_pattern_lines == 0 || num_pattern_lines > content_normalized_lines.len() {
        return Vec::new();
    }

    let mut matches = Vec::new();
    for i in 0..=content_normalized_lines
        .len()
        .saturating_sub(num_pattern_lines)
    {
        let block: String = content_normalized_lines[i..i + num_pattern_lines].join("\n");

        if block == pattern_normalized {
            matches.push(lines_to_match(content, content_lines, i, num_pattern_lines));
        }
    }

    matches
}

/// 在归一化全文中查找子串匹配，命中后用【字符级对齐】把归一化偏移映射回原串字节区间
///
/// 用于归一化会改变字符序列长度（全角→半角、`\\n`→`\n`）的策略。
///
/// 对齐原理：归一化是对 original 逐字符应用变换得到 normalized，因此每个归一化字符
/// 都源自 original 的某个（或某些）字符。这里用 `similar` 计算 original→normalized 的
/// 字符级 diff，从 diff 操作流中精确重建「归一化字符索引 → 原始字符索引」映射，
/// 全程基于字符而非字节，多字节字符永远不会被切到字符内部。
///
/// 这是把 Python（codepoint 级天然安全）算法适配到 Rust（UTF-8 字节存储）的正确做法。
fn find_normalized_substring_matches(
    original: &str,
    normalize: impl Fn(&str) -> String,
    pattern_normalized: &str,
) -> Vec<Match> {
    let normalized = normalize(original);
    if normalized == original {
        return Vec::new();
    }

    let orig_chars: Vec<char> = original.chars().collect();
    let norm_chars: Vec<char> = normalized.chars().collect();

    // 字符级 diff：orig_chars → norm_chars，得到精确的字符对齐关系
    // 用 capture_diff_slices 对 Vec<char> 做非字符串 diff
    use similar::{Algorithm, DiffOp, capture_diff_slices};
    let ops = capture_diff_slices(Algorithm::Myers, &orig_chars, &norm_chars);

    // norm_to_orig[ni] = 归一化第 ni 个字符源自原始第几个字符
    let mut norm_to_orig: Vec<usize> = vec![usize::MAX; norm_chars.len() + 1];
    {
        let mut oi = 0usize; // 原始字符游标
        let mut ni = 0usize; // 归一化字符游标
        for op in ops.iter() {
            match op {
                DiffOp::Equal { len, .. } => {
                    for _ in 0..*len {
                        if norm_to_orig[ni] == usize::MAX {
                            norm_to_orig[ni] = oi;
                        }
                        oi += 1;
                        ni += 1;
                    }
                }
                DiffOp::Delete { old_len, .. } => {
                    // 原始字符被归一化删除（如 `\\n` 的 `\` 被删，保留 `n`→`\n`）
                    oi += old_len;
                }
                DiffOp::Insert { new_len, .. } => {
                    // 归一化新增字符，映射到当前原始位置
                    for _ in 0..*new_len {
                        if norm_to_orig[ni] == usize::MAX {
                            norm_to_orig[ni] = oi;
                        }
                        ni += 1;
                    }
                }
                DiffOp::Replace {
                    old_len, new_len, ..
                } => {
                    // 原始 old_len 个字符被替换为归一化 new_len 个字符（如全角→半角 1:1）
                    for _ in 0..*new_len {
                        if norm_to_orig[ni] == usize::MAX {
                            norm_to_orig[ni] = oi;
                        }
                        ni += 1;
                    }
                    oi += old_len;
                }
            }
        }
        // 末尾哨兵
        norm_to_orig[norm_chars.len()] = oi.min(orig_chars.len());
    }

    let orig_byte_offsets = char_byte_offsets(original);
    let norm_byte_offsets = char_byte_offsets(&normalized);

    // 在归一化全文中查找所有 pattern_normalized 出现位置
    let mut matches = Vec::new();
    let mut search_start = 0;
    while let Some(rel) = normalized[search_start..].find(pattern_normalized) {
        let ns = search_start + rel;
        let ne = ns + pattern_normalized.len();

        // 归一化字节偏移 → 归一化字符索引 → 原始字符索引 → 原始字节偏移
        let ns_char = byte_to_char_idx(&norm_byte_offsets, ns);
        let ne_char = byte_to_char_idx(&norm_byte_offsets, ne);

        let orig_start_char = norm_to_orig.get(ns_char).copied().unwrap_or(usize::MAX);
        let orig_end_char = norm_to_orig.get(ne_char).copied().unwrap_or(usize::MAX);
        if orig_start_char != usize::MAX && orig_end_char != usize::MAX {
            let os_byte = orig_byte_offsets
                .get(orig_start_char)
                .copied()
                .unwrap_or(original.len());
            let oe_byte = orig_byte_offsets
                .get(orig_end_char)
                .copied()
                .unwrap_or(original.len());
            if os_byte < oe_byte {
                matches.push(Match(os_byte, oe_byte - os_byte));
            }
        }

        // 推进避免死循环
        search_start = ne.max(ns + 1);
    }

    matches
}

/// 构建字符索引 → 字节偏移表
///
/// `offsets[i]` 是第 i 个字符在字符串中的起始字节偏移，
/// 末尾追加字符串长度作为哨兵，供 `byte_to_char_idx` 越界回退。
fn char_byte_offsets(s: &str) -> Vec<usize> {
    let mut offsets: Vec<usize> = s.char_indices().map(|(b, _)| b).collect();
    offsets.push(s.len());
    offsets
}

/// 字节偏移 → 字符索引（二分查找）
///
/// 匹配位置天然是字符边界，二分必然命中；非边界索引回退到最近的左侧字符索引。
fn byte_to_char_idx(byte_offsets: &[usize], byte_idx: usize) -> usize {
    match byte_offsets.binary_search(&byte_idx) {
        Ok(ci) => ci,
        Err(ci) => ci,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        let content = "hello world\nfoo bar\n";
        let (new_content, count, strategy, err) =
            fuzzy_find_and_replace(content, "hello world", "hi world", false);
        assert!(err.is_none());
        assert_eq!(count, 1);
        assert_eq!(strategy.as_deref(), Some("exact"));
        assert!(new_content.contains("hi world"));
    }

    #[test]
    fn empty_old_string_error() {
        let content = "hello";
        let (_, _, _, err) = fuzzy_find_and_replace(content, "", "new", false);
        assert!(err.is_some());
    }

    #[test]
    fn same_strings_error() {
        let content = "hello";
        let (_, _, _, err) = fuzzy_find_and_replace(content, "hello", "hello", false);
        assert!(err.is_some());
    }

    #[test]
    fn multiple_matches_without_replace_all() {
        let content = "foo bar foo";
        let (_, count, _, err) = fuzzy_find_and_replace(content, "foo", "baz", false);
        assert_eq!(count, 0);
        assert!(err.is_some());
        assert!(err.unwrap().contains("2 处匹配"));
    }

    #[test]
    fn multiple_matches_with_replace_all() {
        let content = "foo bar foo";
        let (new_content, count, _, err) = fuzzy_find_and_replace(content, "foo", "baz", true);
        assert!(err.is_none());
        assert_eq!(count, 2);
        assert_eq!(new_content, "baz bar baz");
    }

    #[test]
    fn newline_normalized_match() {
        let content = "hello\r\nworld\n";
        let (new_content, count, strategy, err) =
            fuzzy_find_and_replace(content, "hello\nworld", "hi\nworld", false);
        assert!(err.is_none());
        assert_eq!(count, 1);
        assert_eq!(strategy.as_deref(), Some("newline_normalized"));
        assert!(new_content.contains("hi"));
    }

    #[test]
    fn indentation_flexible_match() {
        let content = "    fn main() {\n        let x = 1;\n    }";
        let pattern = "  fn main() {\n        let x = 1;\n  }";
        let (new_content, count, strategy, err) = fuzzy_find_and_replace(
            content,
            pattern,
            "fn main() {\n        let x = 2;\n}",
            false,
        );
        assert!(err.is_none());
        assert_eq!(count, 1);
        assert!(
            strategy.as_deref() == Some("line_trimmed")
                || strategy.as_deref() == Some("indentation_flexible")
        );
        assert!(new_content.contains("let x = 2"));
    }

    #[test]
    fn no_match_returns_error() {
        let content = "hello world";
        let (_, count, _, err) = fuzzy_find_and_replace(content, "xyz", "abc", false);
        assert_eq!(count, 0);
        assert!(err.is_some());
    }

    // ========================================================================
    // 多字节字符（中文）安全测试 —— 回归测试
    // ========================================================================

    #[test]
    fn exact_match_multibyte_single_occurrence() {
        let content = "旧内容\n第二行\n";
        let (new_content, count, _, err) =
            fuzzy_find_and_replace(content, "旧内容", "新内容", false);
        assert!(err.is_none(), "单次替换不应报错：{err:?}");
        assert_eq!(count, 1);
        assert!(new_content.contains("新内容"));
        assert!(!new_content.contains("旧内容"));
    }

    #[test]
    fn exact_match_multibyte_multiple_occurrences_replace_all() {
        let content = "你好世界\n你好朋友\n";
        let (new_content, count, _, err) = fuzzy_find_and_replace(content, "你好", "您好", true);
        assert!(err.is_none());
        assert_eq!(count, 2);
        assert_eq!(new_content, "您好世界\n您好朋友\n");
    }

    #[test]
    fn exact_match_multibyte_multiple_without_replace_all_errors() {
        let content = "你好\n你好\n";
        let (_, count, _, err) = fuzzy_find_and_replace(content, "你好", "您好", false);
        assert_eq!(count, 0);
        assert!(err.is_some());
        assert!(err.unwrap().contains("2 处匹配"));
    }

    #[test]
    fn exact_match_mixed_ascii_and_multibyte() {
        // 混合 ASCII + 中文，验证边界推进在混合场景下正确
        let content = "fn 你好() {}\n你好世界\n";
        let (new_content, count, _, err) = fuzzy_find_and_replace(content, "你好", "Hello", true);
        assert!(err.is_none());
        assert_eq!(count, 2);
        assert!(new_content.contains("fn Hello()"));
        assert!(new_content.contains("Hello世界"));
    }

    // ========================================================================
    // 归一化 + 多字节字符回归测试 —— 验证架构性修复
    // 全角/智能引号/换行收缩/空白折叠使归一化后字节流错位，
    // 旧实现的逐字节对齐会 panic；新架构按行计算字节区间，天然安全。
    // ========================================================================

    #[test]
    fn unicode_normalized_fullwidth_multibyte_no_panic() {
        // 复现 panic 的同类场景：content 用全角、old_string 用半角，
        // exact 不匹配 → 走 unicode_normalized（全角→半角归一化后匹配）→ 按行计算字节区间
        let content = "测试ＡＢＣ内容\n第二行\n";
        let (new_content, count, strategy, err) =
            fuzzy_find_and_replace(content, "ABC", "XYZ", false);
        assert!(err.is_none(), "全角→半角归一化不应报错：{err:?}");
        assert_eq!(count, 1);
        assert_eq!(strategy.as_deref(), Some("unicode_normalized"));
        assert_eq!(new_content, "测试XYZ内容\n第二行\n");
    }

    #[test]
    fn newline_normalized_multibyte_no_panic() {
        // 换行收缩（\r\n → \n）：字节长度变化 + 多字节中文，验证不 panic 且替换正确
        let content = "你好\r\n世界\r\n";
        let (new_content, count, _, err) =
            fuzzy_find_and_replace(content, "你好\n世界", "HI\nWORLD", false);
        assert!(err.is_none(), "换行归一化不应报错：{err:?}");
        assert_eq!(count, 1);
        assert!(new_content.contains("HI"));
        assert!(new_content.contains("WORLD"));
    }

    #[test]
    fn whitespace_normalized_multibyte_no_panic() {
        // 空白折叠（多空格 → 单空格）+ 多字节中文：验证不 panic
        let content = "你好     世界\n第二行\n";
        let (new_content, count, _, err) =
            fuzzy_find_and_replace(content, "你好 世界", "HI WORLD", false);
        assert!(err.is_none(), "空白归一化不应报错：{err:?}");
        assert_eq!(count, 1);
        assert!(new_content.contains("HI WORLD"));
    }

    #[test]
    fn escape_normalized_multibyte_no_panic() {
        // 转义归一化（\\n → \n）+ 多字节中文：验证不 panic
        let content = "你好\\n世界\n第二行\n";
        let (new_content, count, _, err) =
            fuzzy_find_and_replace(content, "你好\n世界", "HI\nWORLD", false);
        assert!(err.is_none(), "转义归一化不应报错：{err:?}");
        assert_eq!(count, 1);
        assert!(new_content.contains("HI"));
        assert!(new_content.contains("WORLD"));
    }

    #[test]
    fn map_positions_fullwidth_replacement_correctness() {
        // 直接验证归一化映射结果正确（不仅不 panic）：
        // 全角「ＡＢ」→ 半角「AB」，中间夹中文，替换为 ASCII
        let content = "前面ＡＢ中间文字";
        let (new_content, _, _, err) = fuzzy_find_and_replace(content, "AB", "XY", false);
        assert!(err.is_none());
        assert_eq!(new_content, "前面XY中间文字");
    }

    #[test]
    fn floor_char_boundary_basic() {
        // '旧' = 3 字节 (E6 97 A7)，'字' = 3 字节 (E5 AD 97)
        let s = "旧字"; // 字节：0,1,2 = 旧；3,4,5 = 字
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 1), 0);
        assert_eq!(floor_char_boundary(s, 2), 0);
        assert_eq!(floor_char_boundary(s, 3), 3);
        assert_eq!(floor_char_boundary(s, 4), 3);
        assert_eq!(floor_char_boundary(s, 6), 6);
        assert_eq!(floor_char_boundary(s, 100), 6);
    }

    // ========================================================================
    // 防误改护栏（is_disproportionate_match）
    // ========================================================================

    #[test]
    fn guard_allows_normal_expansion() {
        // 3 行 old → 100 行 new 不应被拦：只比较 search/old，new 大小无关
        // 这里 search 和 old_string 都是 3 行，等长，必然不触发
        assert!(!is_disproportionate_match("a\nb\nc", "a\nb\nc"));
        // search 略大于 old（1.5 倍行数，且未达 oldLines+3）也不拦
        assert!(!is_disproportionate_match("a\nb\nc\nd", "a\nb\nc"));
    }

    #[test]
    fn guard_blocks_disproportionate_search() {
        // old 3 行，匹配块圈了 8 行（≥ 3+3 且 ≥ 3×2=6）→ 拒绝
        let search = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8";
        assert!(is_disproportionate_match(search, "line1\nline2\nline3"));
    }

    #[test]
    fn guard_single_line_old_not_char_checked() {
        // 单行 old：只看行数维度。search 多行但 old 单行 → old_lines==1 时跳过字符维度
        // 行数维度：search 多行，old 1 行 → search_lines(3) >= 1+3=4? 否，不触发行数
        // 字符维度：old_lines==1 直接 return false
        // 故单行 old 的多行 search 不被拦（避免单行 old 的小扩写被误拒）
        assert!(!is_disproportionate_match("a\nb\nc", "a"));
    }

    #[test]
    fn guard_blocks_char_disproportion() {
        // 多行 old，字符维度触发：search trim 远大于 old trim（×4 且 +500）
        let long = "x".repeat(600);
        let search = format!("{long}\nsecond");
        assert!(is_disproportionate_match(&search, "short\nblock"));
    }
}
