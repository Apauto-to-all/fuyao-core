//! 文件编辑后端
//!
//! 提供文件编辑的核心逻辑，包含安全检查、外部编辑检测、差异生成。
//!
//! ## 核心函数
//!
//! - `apply_replace`: 查找替换，调用 fuzzy_find_and_replace 进行模糊匹配
//! - `generate_unified_diff`: 生成 unified diff 格式的差异文本
//! - `check_edit_safety`: 检查编辑操作的物理安全（敏感路径检查）

use crate::file::edit::filelock::with_file_lock;
use crate::file::edit::fuzzy::fuzzy_find_and_replace;
use crate::file::edit::textutil::{
    detect_line_ending, join_bom, normalize_line_endings, split_bom, trim_common_indent,
};
use crate::file::edit::types::EditReplaceResult;
use crate::file::safety::check_sensitive_path;
use crate::file::tracker::{check_file_staleness, update_read_timestamp};
use similar::TextDiff;
use std::path::Path;

/// 应用替换操作
///
/// 完整流程：安全检查 → 文件存在性 → 外部编辑检测 → 读取 → 模糊替换 → 写入 → 生成 diff。
///
/// # 参数
///
/// - `file_path`: 文件绝对路径
/// - `old_string`: 要查找的文本
/// - `new_string`: 替换文本
/// - `replace_all`: 是否替换所有匹配
/// - `path_display`: 显示路径（相对路径，用于错误信息和 diff）
/// - `task_id`: 任务 ID（用于文件追踪）
pub fn apply_replace(
    file_path: &Path,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    path_display: &str,
    task_id: &str,
) -> EditReplaceResult {
    if let Some(safety_error) = check_edit_safety(path_display) {
        return EditReplaceResult {
            success: false,
            path: path_display.to_string(),
            matches: 0,
            strategy: None,
            diff: String::new(),
            warning: None,
            error: Some(safety_error),
        };
    }

    if !file_path.exists() {
        return EditReplaceResult {
            success: false,
            path: path_display.to_string(),
            matches: 0,
            strategy: None,
            diff: String::new(),
            warning: None,
            error: Some(format!("文件不存在: {path_display}")),
        };
    }

    let stale_warning = check_file_staleness(path_display, task_id);

    // 「读-改-写」整段包进 per-path 文件锁，防止同一文件并发编辑覆盖
    let (new_content, original_content, match_count, strategy, error) =
        with_file_lock(file_path, || {
            // 读取时分离 BOM（原文件可能带 \u{FEFF}，需记录并写回时还原）
            let raw = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(e) => {
                    return (
                        String::new(),
                        String::new(),
                        0,
                        None,
                        Some(format!("无权限访问文件: {e}")),
                    );
                }
            };

            let (content, has_bom) = split_bom(&raw);
            // 检测原文件行尾风格，用于写入前把 new_content 转回该风格（保真）
            let ending = detect_line_ending(content);

            let outcome = fuzzy_find_and_replace(content, old_string, new_string, replace_all);

            if outcome.error.is_some() {
                return (String::new(), String::new(), 0, None, outcome.error);
            }

            // 写入前保真：把 fuzzy 替换后的内容转回原文件行尾风格，并还原 BOM
            let to_write = ending.apply(&outcome.content);
            let to_write = join_bom(&to_write, has_bom);

            if let Err(e) = std::fs::write(file_path, to_write.as_bytes()) {
                return (
                    String::new(),
                    String::new(),
                    0,
                    None,
                    Some(format!("写入文件失败: {e}")),
                );
            }

            (
                outcome.content,
                content.to_string(),
                outcome.replacements,
                outcome.strategy,
                None,
            )
        });

    if let Some(err) = error {
        return EditReplaceResult {
            success: false,
            path: path_display.to_string(),
            matches: 0,
            strategy: None,
            diff: String::new(),
            warning: None,
            error: Some(err),
        };
    }

    let diff = generate_unified_diff(&original_content, &new_content, path_display, path_display);

    update_read_timestamp(path_display, task_id);

    EditReplaceResult {
        success: true,
        path: path_display.to_string(),
        matches: match_count,
        strategy,
        diff,
        warning: stale_warning,
        error: None,
    }
}

/// 生成 unified diff 格式的差异文本
///
/// 两步处理：
/// 1. 生成前把换行符归一化为 LF——`similar` 的 `from_lines` 会把 `\r` 当作行内容保留，
///    导致 CRLF 文件的 diff 每行末尾残留 `\r`，给 LLM 展示时显得杂乱。
/// 2. 生成后压缩公共前导缩进——深嵌套代码的 diff 保留大量公共缩进会浪费 token。
///
/// diff 仅用于展示改动，不影响磁盘文件（磁盘写入走行尾保真 + BOM 还原）。
fn generate_unified_diff(old: &str, new: &str, from_file: &str, to_file: &str) -> String {
    let old_norm = normalize_line_endings(old);
    let new_norm = normalize_line_endings(new);
    let diff = TextDiff::from_lines(&old_norm, &new_norm);
    let mut output = String::new();

    for hunk in diff
        .unified_diff()
        .header(&format!("a/{from_file}"), &format!("b/{to_file}"))
        .iter_hunks()
    {
        output.push_str(&hunk.to_string());
    }

    trim_common_indent(&output)
}

/// 检查编辑操作的物理安全（敏感路径检查）
fn check_edit_safety(path: &str) -> Option<String> {
    let err = check_sensitive_path(path, "修改");
    if let Some(ref e) = err {
        tracing::warn!(path = %path, action = "修改", reason = %e, "拒绝操作敏感路径");
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_replace_success() {
        let dir = std::env::temp_dir().join("fuyao_test_edit_replace");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "hello world\nfoo bar\n").unwrap();

        let result = apply_replace(
            &file_path,
            "hello world",
            "hi world",
            false,
            &file_path.to_string_lossy(),
            "test_task",
        );

        assert!(result.success);
        assert_eq!(result.matches, 1);
        assert!(result.strategy.is_some());

        let new_content = std::fs::read_to_string(&file_path).unwrap();
        assert!(new_content.contains("hi world"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_replace_file_not_found() {
        let result = apply_replace(
            Path::new("/nonexistent/file.txt"),
            "old",
            "new",
            false,
            "/nonexistent/file.txt",
            "test_task",
        );

        assert!(!result.success);
        assert!(result.error.unwrap().contains("文件不存在"));
    }

    #[test]
    fn apply_replace_sensitive_path() {
        let result = apply_replace(
            Path::new("/etc/passwd"),
            "old",
            "new",
            false,
            "/etc/passwd",
            "test_task",
        );

        assert!(!result.success);
    }

    #[test]
    fn diff_crlf_stripped() {
        // CRLF 文件生成的 diff 不应残留 \r（避免给 LLM 展示杂乱的 \r）
        let old = "enabled = false\r\nurl = \"x\"\r\n";
        let new = "enabled = true\r\nurl = \"x\"\r\n";
        let diff = generate_unified_diff(old, new, "f.toml", "f.toml");
        assert!(
            !diff.contains('\r'),
            "diff 输出不应包含 \\r，实际：{diff:?}"
        );
        assert!(diff.contains("enabled = true"));
    }
}
