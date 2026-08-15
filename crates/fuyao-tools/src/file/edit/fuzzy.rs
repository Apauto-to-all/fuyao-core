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
//! 4. **indentation_flexible**: 忽略前导缩进差异
//! 5. **block_anchor**: 首尾行锚定 + 中间内容相似度匹配

use similar::TextDiff;

/// 一次匹配：原始 content 中的字节偏移与匹配段的字节长度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Match(usize, usize);

/// 匹配策略函数类型
///
/// 返回原始 content 中真实存在的若干段匹配，每段用 `(字节偏移, 字节长度)` 表示。
/// 偏移量锚定在行首或精确子串位置，天然是字符边界。
type MatchStrategy = fn(&str, &str) -> Vec<Match>;

/// 模糊查找替换的结果
///
/// 命名结构体替代原先的位置 4-tuple `(String, usize, Option<String>, Option<String>)`——
/// 后两个 `Option<String>`（策略名 / 错误信息）类型相同、位置不可区分，调用方解构易错位。
pub struct FuzzyOutcome {
    /// 替换后的新内容（无匹配或出错时为原 content 副本）
    pub content: String,
    /// 实际发生的替换次数（无匹配或出错为 0）
    pub replacements: usize,
    /// 命中的策略名称（仅成功时有值）
    pub strategy: Option<String>,
    /// 错误信息（成功时为 None）
    pub error: Option<String>,
}

/// 模糊查找并替换
///
/// 使用多种策略尝试匹配，处理空白、缩进等差异。
/// 按策略优先级依次尝试，首个有匹配的策略生效。
///
/// # 返回
///
/// [`FuzzyOutcome`]：新内容 / 替换次数 / 命中策略名 / 错误信息。
pub fn fuzzy_find_and_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> FuzzyOutcome {
    if old_string.is_empty() {
        return FuzzyOutcome {
            content: content.to_string(),
            replacements: 0,
            strategy: None,
            error: Some("old_string 不能为空".to_string()),
        };
    }

    if old_string == new_string {
        return FuzzyOutcome {
            content: content.to_string(),
            replacements: 0,
            strategy: None,
            error: Some("old_string 和 new_string 相同".to_string()),
        };
    }

    let strategies: Vec<(&str, MatchStrategy)> = vec![
        ("exact", strategy_exact),
        ("newline_normalized", strategy_newline_normalized),
        ("line_trimmed", strategy_line_trimmed),
        ("indentation_flexible", strategy_indentation_flexible),
        ("block_anchor", strategy_block_anchor),
    ];

    for (strategy_name, strategy_fn) in strategies {
        let matches = strategy_fn(content, old_string);

        if !matches.is_empty() {
            if matches.len() > 1 && !replace_all {
                return FuzzyOutcome {
                    content: content.to_string(),
                    replacements: 0,
                    strategy: None,
                    error: Some(format!(
                        "找到 {} 处匹配。请提供更多上下文使其唯一，或使用 replace_all=true。",
                        matches.len()
                    )),
                };
            }

            // 护栏：宽容策略（尤以 block_anchor）可能圈出远大于 old_string 的匹配块，
            // 此时若放行会用 new_string 覆盖掉 AI 未声明要改的大量内容。
            // 只比较「实际匹配块」与「old_string」，与 new_string 无关，
            // 因此「3 行 old 改成 100 行 new」不会被误拦。
            for &Match(start, len) in &matches {
                let search = &content[start..start + len];
                if is_disproportionate_match(search, old_string) {
                    return FuzzyOutcome {
                        content: content.to_string(),
                        replacements: 0,
                        strategy: None,
                        error: Some(
                            "匹配范围远大于 old_string，可能匹配到无关内容。\
                             请重新读取文件，提供完整的 old_string 以精确匹配。"
                                .to_string(),
                        ),
                    };
                }
            }

            let new_content = apply_replacements(content, &matches, new_string);
            return FuzzyOutcome {
                content: new_content,
                replacements: matches.len(),
                strategy: Some(strategy_name.to_string()),
                error: None,
            };
        }
    }

    FuzzyOutcome {
        content: content.to_string(),
        replacements: 0,
        strategy: None,
        error: Some("未找到匹配的文本".to_string()),
    }
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

/// 策略 4: 忽略前导缩进差异
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

/// 策略 5: 首尾行锚定 + 中间内容相似度匹配
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        let content = "hello world\nfoo bar\n";
        let FuzzyOutcome {
            content: new_content,
            replacements: count,
            strategy,
            error: err,
        } = fuzzy_find_and_replace(content, "hello world", "hi world", false);
        assert!(err.is_none());
        assert_eq!(count, 1);
        assert_eq!(strategy.as_deref(), Some("exact"));
        assert!(new_content.contains("hi world"));
    }

    #[test]
    fn empty_old_string_error() {
        let content = "hello";
        let FuzzyOutcome { error: err, .. } = fuzzy_find_and_replace(content, "", "new", false);
        assert!(err.is_some());
    }

    #[test]
    fn same_strings_error() {
        let content = "hello";
        let FuzzyOutcome { error: err, .. } =
            fuzzy_find_and_replace(content, "hello", "hello", false);
        assert!(err.is_some());
    }

    #[test]
    fn multiple_matches_without_replace_all() {
        let content = "foo bar foo";
        let FuzzyOutcome {
            replacements: count,
            error: err,
            ..
        } = fuzzy_find_and_replace(content, "foo", "baz", false);
        assert_eq!(count, 0);
        assert!(err.is_some());
        assert!(err.unwrap().contains("2 处匹配"));
    }

    #[test]
    fn multiple_matches_with_replace_all() {
        let content = "foo bar foo";
        let FuzzyOutcome {
            content: new_content,
            replacements: count,
            error: err,
            ..
        } = fuzzy_find_and_replace(content, "foo", "baz", true);
        assert!(err.is_none());
        assert_eq!(count, 2);
        assert_eq!(new_content, "baz bar baz");
    }

    #[test]
    fn newline_normalized_match() {
        let content = "hello\r\nworld\n";
        let FuzzyOutcome {
            content: new_content,
            replacements: count,
            strategy,
            error: err,
        } = fuzzy_find_and_replace(content, "hello\nworld", "hi\nworld", false);
        assert!(err.is_none());
        assert_eq!(count, 1);
        assert_eq!(strategy.as_deref(), Some("newline_normalized"));
        assert!(new_content.contains("hi"));
    }

    #[test]
    fn indentation_flexible_match() {
        let content = "    fn main() {\n        let x = 1;\n    }";
        let pattern = "  fn main() {\n        let x = 1;\n  }";
        let FuzzyOutcome {
            content: new_content,
            replacements: count,
            strategy,
            error: err,
        } = fuzzy_find_and_replace(
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
        let FuzzyOutcome {
            replacements: count,
            error: err,
            ..
        } = fuzzy_find_and_replace(content, "xyz", "abc", false);
        assert_eq!(count, 0);
        assert!(err.is_some());
    }

    // ========================================================================
    // 多字节字符（中文）安全测试 —— 回归测试
    // ========================================================================

    #[test]
    fn exact_match_multibyte_single_occurrence() {
        let content = "旧内容\n第二行\n";
        let FuzzyOutcome {
            content: new_content,
            replacements: count,
            error: err,
            ..
        } = fuzzy_find_and_replace(content, "旧内容", "新内容", false);
        assert!(err.is_none(), "单次替换不应报错：{err:?}");
        assert_eq!(count, 1);
        assert!(new_content.contains("新内容"));
        assert!(!new_content.contains("旧内容"));
    }

    #[test]
    fn exact_match_multibyte_multiple_occurrences_replace_all() {
        let content = "你好世界\n你好朋友\n";
        let FuzzyOutcome {
            content: new_content,
            replacements: count,
            error: err,
            ..
        } = fuzzy_find_and_replace(content, "你好", "您好", true);
        assert!(err.is_none());
        assert_eq!(count, 2);
        assert_eq!(new_content, "您好世界\n您好朋友\n");
    }

    #[test]
    fn exact_match_multibyte_multiple_without_replace_all_errors() {
        let content = "你好\n你好\n";
        let FuzzyOutcome {
            replacements: count,
            error: err,
            ..
        } = fuzzy_find_and_replace(content, "你好", "您好", false);
        assert_eq!(count, 0);
        assert!(err.is_some());
        assert!(err.unwrap().contains("2 处匹配"));
    }

    #[test]
    fn exact_match_mixed_ascii_and_multibyte() {
        // 混合 ASCII + 中文，验证边界推进在混合场景下正确
        let content = "fn 你好() {}\n你好世界\n";
        let FuzzyOutcome {
            content: new_content,
            replacements: count,
            error: err,
            ..
        } = fuzzy_find_and_replace(content, "你好", "Hello", true);
        assert!(err.is_none());
        assert_eq!(count, 2);
        assert!(new_content.contains("fn Hello()"));
        assert!(new_content.contains("Hello世界"));
    }

    // ========================================================================
    // 归一化 + 多字节字符回归测试 —— 验证架构性修复
    // 换行收缩（\r\n → \n）使归一化后字节流错位，
    // 旧实现的逐字节对齐会 panic；新架构按行计算字节区间，天然安全。
    // ========================================================================

    #[test]
    fn newline_normalized_multibyte_no_panic() {
        // 换行收缩（\r\n → \n）：字节长度变化 + 多字节中文，验证不 panic 且替换正确
        let content = "你好\r\n世界\r\n";
        let FuzzyOutcome {
            content: new_content,
            replacements: count,
            error: err,
            ..
        } = fuzzy_find_and_replace(content, "你好\n世界", "HI\nWORLD", false);
        assert!(err.is_none(), "换行归一化不应报错：{err:?}");
        assert_eq!(count, 1);
        assert!(new_content.contains("HI"));
        assert!(new_content.contains("WORLD"));
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
