//! 文件编辑后端
//!
//! 提供文件编辑的核心逻辑，包含安全检查、外部编辑检测、差异生成。
//!
//! ## 核心函数
//!
//! - `apply_replace`: 替换模式，调用 fuzzy_find_and_replace 进行模糊匹配
//! - `apply_v4a_patch`: V4A 补丁模式，解析补丁后逐操作执行
//!
//! ## V4A 补丁操作
//!
//! 支持四种操作类型：
//! - **Add**: 创建新文件
//! - **Update**: 更新现有文件（通过 hunk 上下文匹配）
//! - **Delete**: 删除文件
//! - **Move**: 移动/重命名文件

use crate::file::edit::filelock::with_file_lock;
use crate::file::edit::fuzzy::fuzzy_find_and_replace;
use crate::file::edit::patch::{OperationType, PatchOperation, parse_v4a_patch};
use crate::file::edit::textutil::{
    detect_line_ending, join_bom, normalize_line_endings, split_bom, trim_common_indent,
};
use crate::file::edit::types::{EditPatchResult, EditReplaceResult};
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

            let (new_content, match_count, strategy, error) =
                fuzzy_find_and_replace(content, old_string, new_string, replace_all);

            if error.is_some() {
                return (String::new(), String::new(), 0, None, error);
            }

            // 写入前保真：把 fuzzy 替换后的内容转回原文件行尾风格，并还原 BOM
            let to_write = ending.apply(&new_content);
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
                new_content,
                content.to_string(),
                match_count,
                strategy,
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

/// 解析并应用 V4A 补丁
///
/// 完整流程：解析补丁 → 安全检查（每个操作的文件路径） → 逐操作执行。
///
/// # 参数
///
/// - `patch_content`: V4A 格式补丁内容
/// - `workspace`: 工作区根路径（用于解析补丁中的相对路径）
/// - `task_id`: 任务 ID（用于文件追踪）
pub fn apply_v4a_patch(patch_content: &str, workspace: &Path, task_id: &str) -> EditPatchResult {
    let (operations, error) = parse_v4a_patch(patch_content);

    if let Some(err) = error {
        return EditPatchResult {
            success: false,
            files_modified: Vec::new(),
            files_created: Vec::new(),
            files_deleted: Vec::new(),
            diff: String::new(),
            warning: None,
            error: Some(err),
        };
    }

    if operations.is_empty() {
        return EditPatchResult {
            success: false,
            files_modified: Vec::new(),
            files_created: Vec::new(),
            files_deleted: Vec::new(),
            diff: String::new(),
            warning: None,
            error: Some("补丁解析失败：未找到有效的 V4A 操作".to_string()),
        };
    }

    for op in &operations {
        if let Some(safety_error) = check_edit_safety(&op.file_path) {
            return EditPatchResult {
                success: false,
                files_modified: Vec::new(),
                files_created: Vec::new(),
                files_deleted: Vec::new(),
                diff: String::new(),
                warning: None,
                error: Some(safety_error),
            };
        }

        if let Some(ref new_path) = op.new_path
            && let Some(safety_error) = check_edit_safety(new_path)
        {
            return EditPatchResult {
                success: false,
                files_modified: Vec::new(),
                files_created: Vec::new(),
                files_deleted: Vec::new(),
                diff: String::new(),
                warning: None,
                error: Some(safety_error),
            };
        }
    }

    apply_patch(&operations, workspace, task_id)
}

/// 执行补丁操作列表
///
/// 逐个执行操作，收集结果。部分失败时不中断，汇总所有错误后返回。
fn apply_patch(operations: &[PatchOperation], workspace: &Path, task_id: &str) -> EditPatchResult {
    let mut files_modified = Vec::new();
    let mut files_created = Vec::new();
    let mut files_deleted = Vec::new();
    let mut all_diffs = Vec::new();
    let mut errors = Vec::new();
    let mut stale_warnings = Vec::new();

    for op in operations {
        let result = apply_single_op(op, workspace, task_id);
        match result {
            Ok(op_result) => {
                match op.operation {
                    OperationType::Add => {
                        files_created.push(op.file_path.clone());
                    }
                    OperationType::Delete => {
                        files_deleted.push(op.file_path.clone());
                    }
                    OperationType::Move => {
                        files_modified.push(format!(
                            "{} -> {}",
                            op.file_path,
                            op.new_path.as_deref().unwrap_or("")
                        ));
                    }
                    OperationType::Update => {
                        files_modified.push(op.file_path.clone());
                    }
                }
                if !op_result.diff.is_empty() {
                    all_diffs.push(op_result.diff);
                }
                if let Some(w) = op_result.warning {
                    stale_warnings.push(w);
                }
            }
            Err(e) => {
                errors.push(format!("{}: {e}", op.file_path));
            }
        }
    }

    if !errors.is_empty() {
        return EditPatchResult {
            success: false,
            files_modified,
            files_created,
            files_deleted,
            diff: all_diffs.join("\n"),
            warning: None,
            error: Some(format!(
                "部分操作失败:\n{}",
                errors
                    .iter()
                    .map(|e| format!("  • {e}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
        };
    }

    EditPatchResult {
        success: true,
        files_modified,
        files_created,
        files_deleted,
        diff: all_diffs.join("\n"),
        warning: if stale_warnings.len() == 1 {
            stale_warnings.into_iter().next()
        } else if stale_warnings.is_empty() {
            None
        } else {
            Some(stale_warnings.join(" | "))
        },
        error: None,
    }
}

struct OpResult {
    diff: String,
    warning: Option<String>,
}

/// 执行单个补丁操作，根据操作类型分发到对应的处理函数
fn apply_single_op(
    op: &PatchOperation,
    workspace: &Path,
    task_id: &str,
) -> Result<OpResult, String> {
    match op.operation {
        OperationType::Add => apply_add(op, workspace),
        OperationType::Delete => apply_delete(op, workspace),
        OperationType::Move => apply_move(op, workspace),
        OperationType::Update => apply_update(op, workspace, task_id),
    }
}

/// Add 操作：从 hunk 的 + 行提取内容，创建新文件
fn apply_add(op: &PatchOperation, workspace: &Path) -> Result<OpResult, String> {
    let mut content_lines = Vec::new();
    for hunk in &op.hunks {
        for line in &hunk.lines {
            if line.prefix == "+" {
                content_lines.push(line.content.clone());
            }
        }
    }

    let content = content_lines.join("\n");
    let file_path = workspace.join(&op.file_path);

    if let Some(parent) = file_path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }

    std::fs::write(&file_path, &content).map_err(|e| format!("写入文件失败: {e}"))?;

    let diff = format!(
        "--- /dev/null\n+++ b/{}\n{}",
        op.file_path,
        content_lines
            .iter()
            .map(|l| format!("+{l}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    Ok(OpResult {
        diff,
        warning: None,
    })
}

/// Delete 操作：读取文件内容用于 diff，然后删除文件
fn apply_delete(op: &PatchOperation, workspace: &Path) -> Result<OpResult, String> {
    let file_path = workspace.join(&op.file_path);

    if !file_path.exists() {
        return Err(format!("文件不存在: {}", op.file_path));
    }

    let old_content =
        std::fs::read_to_string(&file_path).map_err(|e| format!("读取文件失败: {e}"))?;

    let diff = generate_unified_diff(&old_content, "", &op.file_path, "/dev/null");

    std::fs::remove_file(&file_path).map_err(|e| format!("删除文件失败: {e}"))?;

    Ok(OpResult {
        diff,
        warning: None,
    })
}

/// Move 操作：移动/重命名文件，自动创建目标目录
fn apply_move(op: &PatchOperation, workspace: &Path) -> Result<OpResult, String> {
    let new_path = op.new_path.as_deref().ok_or("MOVE 操作缺少 new_path")?;

    let src_path = workspace.join(&op.file_path);
    let dst_path = workspace.join(new_path);

    if !src_path.exists() {
        return Err(format!("源文件不存在: {}", op.file_path));
    }

    if dst_path.exists() {
        return Err(format!("目标文件已存在: {new_path}"));
    }

    if let Some(parent) = dst_path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }

    std::fs::rename(&src_path, &dst_path).map_err(|e| format!("移动文件失败: {e}"))?;

    let diff = format!("# 移动: {} -> {}", op.file_path, new_path);

    Ok(OpResult {
        diff,
        warning: None,
    })
}

/// Update 操作：通过 hunk 上下文匹配替换文件内容
///
/// 每个 hunk 的处理逻辑：
/// - 有搜索行（空格/减号前缀）→ 用 fuzzy_find_and_replace 模糊匹配替换
/// - 无搜索行（纯加号）→ 根据 context_hint 定位插入点，或追加到文件末尾
fn apply_update(op: &PatchOperation, workspace: &Path, task_id: &str) -> Result<OpResult, String> {
    let file_path = workspace.join(&op.file_path);

    if !file_path.exists() {
        return Err(format!("文件不存在: {}", op.file_path));
    }

    let stale_warning = check_file_staleness(&op.file_path, task_id);

    let content = std::fs::read_to_string(&file_path).map_err(|e| format!("读取文件失败: {e}"))?;

    let mut new_content = content.clone();
    let mut hunk_errors = Vec::new();

    for hunk in &op.hunks {
        // 搜索行：空格前缀（上下文）和减号前缀（删除）用于定位匹配位置
        let search_lines: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|l| l.prefix == " " || l.prefix == "-")
            .map(|l| l.content.as_str())
            .collect();
        // 替换行：空格前缀（上下文）和加号前缀（新增）组成替换后的内容
        let replace_lines: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|l| l.prefix == " " || l.prefix == "+")
            .map(|l| l.content.as_str())
            .collect();

        if !search_lines.is_empty() {
            // 有搜索行 → 用模糊匹配定位替换位置
            let search_pattern = search_lines.join("\n");
            let replacement = replace_lines.join("\n");

            let (updated, count, _, match_error) =
                fuzzy_find_and_replace(&new_content, &search_pattern, &replacement, false);

            if match_error.is_some() || count == 0 {
                hunk_errors.push(format!(
                    "hunk 上下文不匹配 - {}",
                    match_error.unwrap_or_else(|| "未找到匹配内容".to_string())
                ));
                continue;
            }

            new_content = updated;
        } else {
            // 无搜索行（纯加号）→ 纯插入模式
            let insert_text = replace_lines.join("\n");
            if let Some(ref hint) = hunk.context_hint {
                // 有上下文提示 → 在提示位置后插入
                if let Some(hint_pos) = new_content.find(hint.as_str()) {
                    if let Some(eol) = new_content[hint_pos..].find('\n') {
                        // 找到提示行的行尾，在行尾后插入
                        let abs_eol = hint_pos + eol + 1;
                        new_content = format!(
                            "{}{}\n{}",
                            &new_content[..abs_eol],
                            insert_text,
                            &new_content[abs_eol..]
                        );
                    } else {
                        new_content = format!("{new_content}\n{insert_text}");
                    }
                } else {
                    new_content =
                        format!("{}\n{insert_text}\n", new_content.trim_end_matches('\n'));
                }
            } else {
                new_content = format!("{}\n{insert_text}\n", new_content.trim_end_matches('\n'));
            }
        }
    }

    if !hunk_errors.is_empty() && new_content == content {
        return Err(hunk_errors.join("; "));
    }

    std::fs::write(&file_path, &new_content).map_err(|e| format!("写入文件失败: {e}"))?;

    let diff = generate_unified_diff(&content, &new_content, &op.file_path, &op.file_path);

    update_read_timestamp(&op.file_path, task_id);

    Ok(OpResult {
        diff,
        warning: stale_warning,
    })
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
    fn apply_v4a_patch_add_file() {
        let dir = std::env::temp_dir().join("fuyao_test_v4a_add");
        std::fs::create_dir_all(&dir).unwrap();

        let patch = "*** Begin Patch\n*** Add File: new_file.txt\n+hello world\n*** End Patch";
        let result = apply_v4a_patch(patch, &dir, "test_task");

        assert!(result.success);
        assert!(result.files_created.contains(&"new_file.txt".to_string()));

        let content = std::fs::read_to_string(dir.join("new_file.txt")).unwrap();
        assert_eq!(content, "hello world");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_v4a_patch_empty() {
        let result = apply_v4a_patch("", Path::new("/tmp"), "test_task");
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
