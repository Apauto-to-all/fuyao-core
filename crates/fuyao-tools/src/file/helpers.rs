//! 辅助函数
//!
//! 提供相似文件名建议等辅助功能。
//!
//! ## 相似文件名建议
//!
//! 当文件不存在时，在同级目录中查找相似的文件名，帮助用户修正路径错误。
//! 使用 Jaro-Winkler 相似度算法（阈值 0.5），并有前缀匹配 fallback。

use std::path::Path;

/// 建议相似文件名
///
/// 当文件不存在时，在同级目录中查找相似的文件名，帮助用户修正路径错误。
///
/// # 匹配策略
///
/// 1. **Jaro-Winkler 相似度**（阈值 0.5）：计算文件名的编辑距离相似度
/// 2. **前缀匹配 fallback**（前 3 字符）：相似度无结果时，用文件名前缀匹配
pub fn suggest_similar_files(target_path: &str, max_suggestions: usize) -> Vec<String> {
    let expanded = crate::common::expand_tilde(target_path);
    let parent_dir = expanded.parent().unwrap_or(Path::new(".")).to_path_buf();
    let filename = expanded
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    if !parent_dir.is_dir() || filename.is_empty() {
        return Vec::new();
    }

    let existing_files: Vec<String> = match std::fs::read_dir(&parent_dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| e.file_name().to_string_lossy().to_string().into())
            .collect(),
        Err(_) => return Vec::new(),
    };

    if existing_files.is_empty() {
        return Vec::new();
    }

    // 使用 strsim 计算相似度
    let mut scored: Vec<(f64, String)> = existing_files
        .iter()
        .filter_map(|f| {
            let score = strsim::jaro_winkler(&filename.to_lowercase(), &f.to_lowercase());
            if score > 0.5 {
                Some((score, f.clone()))
            } else {
                None
            }
        })
        .collect();

    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let results: Vec<String> = scored
        .into_iter()
        .take(max_suggestions)
        .map(|(_, f)| parent_dir.join(f).to_string_lossy().to_string())
        .collect();

    if !results.is_empty() {
        return results;
    }

    // Fallback: 前缀匹配
    let base_name = filename
        .rsplit_once('.')
        .map(|(name, _)| name.to_lowercase())
        .unwrap_or(filename.to_lowercase());

    if !base_name.is_empty() && base_name.len() >= 3 {
        let prefix = &base_name[..3];
        let prefix_matches: Vec<String> = existing_files
            .into_iter()
            .filter(|f| f.to_lowercase().starts_with(prefix))
            .take(max_suggestions)
            .map(|f| parent_dir.join(f).to_string_lossy().to_string())
            .collect();
        return prefix_matches;
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggest_similar_files_finds_match() {
        let dir = std::env::temp_dir().join("fuyao_test_similar");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("lib.rs"), "").unwrap();

        let target = dir.join("mai.rs").to_string_lossy().to_string();
        let suggestions = suggest_similar_files(&target, 5);

        assert!(!suggestions.is_empty());
        assert!(suggestions[0].contains("main.rs"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn suggest_similar_files_empty_dir() {
        let dir = std::env::temp_dir().join("fuyao_test_similar_empty");
        std::fs::create_dir_all(&dir).unwrap();

        let target = dir.join("test.txt").to_string_lossy().to_string();
        let suggestions = suggest_similar_files(&target, 5);
        assert!(suggestions.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
