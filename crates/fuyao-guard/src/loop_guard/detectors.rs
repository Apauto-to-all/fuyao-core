//! 循环检测器（无状态纯函数）
//!
//! 工具两层检测（精确重复 + 循环序列）、文本自相似度检测。
//! 直接比对原始参数字符串，支持所有工具。

use std::collections::VecDeque;

use crate::loop_guard::types::ToolCallRecord;

/// 检测工具调用是否连续重复
///
/// 从后往前检查历史记录，直接比对工具名和参数字符串。
/// 连续 N 次相同操作（含当前调用）时返回检测描述。
/// 与 Python 版一致：当前调用纳入计数，threshold=4 表示连续 4 次即触发。
pub fn detect_tool_repetition(
    records: &VecDeque<ToolCallRecord>,
    new_call: &ToolCallRecord,
    threshold: usize,
) -> Option<String> {
    // 当前调用计入计数
    let mut count = 1;
    for record in records.iter().rev() {
        if record.tool_name == new_call.tool_name
            && record.canonical_args == new_call.canonical_args
        {
            count += 1;
        } else {
            break;
        }
    }

    if count >= threshold {
        Some(format!("连续 {} 次相同操作 {}", count, new_call.tool_name))
    } else {
        None
    }
}

/// 检测工具调用是否存在循环序列模式 (如 A->B->A->B)
///
/// 在窗口内寻找最短重复模式，至少完整重复 2 次。
pub fn detect_tool_sequence_pattern(
    records: &VecDeque<ToolCallRecord>,
    window: usize,
) -> Option<String> {
    let recent: Vec<_> = records
        .iter()
        .rev()
        .take(window)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if recent.len() < 4 {
        return None;
    }

    let signatures: Vec<(&str, &str)> = recent
        .iter()
        .map(|r| (r.tool_name.as_str(), r.canonical_args.as_str()))
        .collect();

    let unique: std::collections::HashSet<_> = signatures.iter().collect();
    if unique.len() <= 1 {
        return None;
    }

    let max_pattern_len = signatures.len() / 2;
    for pattern_len in 2..=max_pattern_len {
        let pattern = &signatures[..pattern_len];
        let mut repetitions = 0;
        let mut i = 0;

        while i + pattern_len <= signatures.len() {
            let chunk = &signatures[i..i + pattern_len];
            if chunk == pattern {
                repetitions += 1;
                i += pattern_len;
            } else {
                break;
            }
        }

        let remainder_len = signatures.len() - repetitions * pattern_len;
        if repetitions >= 2 && remainder_len == 0 {
            let pattern_str: Vec<&str> = pattern.iter().map(|(name, _)| *name).collect();
            return Some(format!(
                "检测到循环序列: {} (完整重复 {} 次)",
                pattern_str.join("→"),
                repetitions
            ));
        }
    }

    None
}

/// 生成 n-gram 列表（按字符边界）
fn ngrams(text: &str, n: usize) -> Vec<&str> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    if chars.len() < n {
        return Vec::new();
    }
    chars
        .windows(n)
        .map(|w| {
            let start = w[0].0;
            let end = w.last().map(|(i, c)| i + c.len_utf8()).unwrap_or(start);
            &text[start..end]
        })
        .collect()
}

/// 检测文本内容是否自相似（重复）
///
/// 使用 trigram Jaccard 相似度比对文本末尾两个窗口。
pub fn detect_text_self_similarity(
    text: &str,
    threshold: f64,
    window_ratio: f64,
) -> Option<String> {
    if text.is_empty() {
        return None;
    }

    let char_count = text.chars().count();
    if char_count < 20 {
        return None;
    }

    let window = ((char_count as f64) * window_ratio).max(20.0) as usize;
    let (first, second) = if char_count < 2 * window {
        let mid = char_count / 2;
        let mid_byte = text.char_indices().nth(mid).map(|(i, _)| i).unwrap_or(0);
        (&text[..mid_byte], &text[mid_byte..])
    } else {
        let start_idx = char_count - 2 * window;
        let mid_idx = char_count - window;
        let start_byte = text
            .char_indices()
            .nth(start_idx)
            .map(|(i, _)| i)
            .unwrap_or(0);
        let mid_byte = text
            .char_indices()
            .nth(mid_idx)
            .map(|(i, _)| i)
            .unwrap_or(0);
        (&text[start_byte..mid_byte], &text[mid_byte..])
    };

    let first_trigrams: std::collections::HashSet<&str> = ngrams(first, 3).into_iter().collect();
    let second_trigrams: std::collections::HashSet<&str> = ngrams(second, 3).into_iter().collect();

    if first_trigrams.is_empty() || second_trigrams.is_empty() {
        return None;
    }

    let intersection = first_trigrams.intersection(&second_trigrams).count();
    let union = first_trigrams.union(&second_trigrams).count();
    let similarity = intersection as f64 / union as f64;

    if similarity >= threshold {
        Some(format!("内容重复率 {:.0}%", similarity * 100.0))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(name: &str, args: &str) -> ToolCallRecord {
        ToolCallRecord {
            tool_name: name.to_string(),
            canonical_args: args.to_string(),
        }
    }

    #[test]
    fn detect_repetition_below_threshold() {
        let mut records: VecDeque<ToolCallRecord> = VecDeque::new();
        for _ in 0..2 {
            records.push_back(make_record("bash", "ls"));
        }
        let new = make_record("bash", "ls");
        // 2 条历史 + 1 当前 = 3 次，threshold=4，不触发
        assert!(detect_tool_repetition(&records, &new, 4).is_none());
    }

    #[test]
    fn detect_repetition_at_threshold() {
        let mut records: VecDeque<ToolCallRecord> = VecDeque::new();
        for _ in 0..3 {
            records.push_back(make_record("bash", "ls"));
        }
        let new = make_record("bash", "ls");
        // 3 条历史 + 1 当前 = 4 次，threshold=4，触发
        let result = detect_tool_repetition(&records, &new, 4);
        assert!(result.is_some());
        assert!(result.unwrap().contains("4 次相同操作"));
    }

    #[test]
    fn detect_repetition_different_args_resets() {
        let mut records: VecDeque<ToolCallRecord> = VecDeque::new();
        for _ in 0..3 {
            records.push_back(make_record("bash", "ls"));
        }
        records.push_back(make_record("bash", "pwd"));
        let new = make_record("bash", "pwd");
        assert!(detect_tool_repetition(&records, &new, 4).is_none());
    }

    #[test]
    fn detect_sequence_pattern_abab() {
        let mut records: VecDeque<ToolCallRecord> = VecDeque::new();
        records.push_back(make_record("read", "a.rs"));
        records.push_back(make_record("edit", "a.rs"));
        records.push_back(make_record("read", "a.rs"));
        records.push_back(make_record("edit", "a.rs"));
        let result = detect_tool_sequence_pattern(&records, 6);
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.contains("循环序列"));
        assert!(msg.contains("read→edit"));
    }

    #[test]
    fn detect_sequence_no_pattern() {
        let mut records: VecDeque<ToolCallRecord> = VecDeque::new();
        records.push_back(make_record("read", "a.rs"));
        records.push_back(make_record("edit", "a.rs"));
        records.push_back(make_record("bash", "ls"));
        records.push_back(make_record("grep", "pattern"));
        let result = detect_tool_sequence_pattern(&records, 6);
        assert!(result.is_none());
    }

    #[test]
    fn detect_sequence_too_few() {
        let mut records: VecDeque<ToolCallRecord> = VecDeque::new();
        records.push_back(make_record("read", "a.rs"));
        records.push_back(make_record("edit", "a.rs"));
        let result = detect_tool_sequence_pattern(&records, 6);
        assert!(result.is_none());
    }

    #[test]
    fn detect_text_similarity_high() {
        let text = "这是一段重复的内容这是一段重复的内容这是一段重复的内容这是一段重复的内容";
        let result = detect_text_self_similarity(text, 0.6, 0.2);
        assert!(result.is_some());
    }

    #[test]
    fn detect_text_similarity_low() {
        let text = "第一段内容完全不同，第二段也是全新的描述，没有重复的trigram出现";
        let result = detect_text_self_similarity(text, 0.6, 0.2);
        assert!(result.is_none());
    }

    #[test]
    fn detect_text_too_short() {
        let result = detect_text_self_similarity("短文本", 0.6, 0.2);
        assert!(result.is_none());
    }

    #[test]
    fn detect_text_empty() {
        let result = detect_text_self_similarity("", 0.6, 0.2);
        assert!(result.is_none());
    }

    #[test]
    fn ngrams_basic() {
        let result = ngrams("abcde", 3);
        assert_eq!(result, vec!["abc", "bcd", "cde"]);
    }

    #[test]
    fn ngrams_shorter_than_n() {
        let result = ngrams("ab", 3);
        assert!(result.is_empty());
    }
}
