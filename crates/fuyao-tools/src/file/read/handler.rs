//! 文件读取处理逻辑
//!
//! 提供文件读取功能，支持分页、行号显示、设备文件保护、相似文件名建议、
//! 文件去重、外部编辑检测、循环检测、敏感信息脱敏。
//!
//! ## 文件读取
//!
//! 返回带行号的内容，使用 offset/limit 分页。自动遵守以下安全规则：
//! - 设备文件（/dev/zero 等）→ 拒绝
//! - 框架内部路径（.fuyao/.env）→ 拒绝
//! - 二进制文件（.exe、.png 等）→ 拒绝
//! - 文件不存在 → 建议相似文件名
//! - 重复读取未修改文件 → 返回去重提示
//! - 内容超限（> MAX_READ_CHARS 字符）→ 拒绝并提示分页
//! - 读取结果自动脱敏 API Key 等敏感信息
//!
//! ## 目录读取
//!
//! 列出目录下的文件和子目录，按修改时间排序（目录优先）。
//! 自动跳过排除目录（.venv、node_modules、__pycache__ 等）。

use crate::common::{self, resolve_path};
use crate::config::{LARGE_FILE_HINT_BYTES, MAX_READ_CHARS, SEARCH_EXCLUDE_DIRS};
use crate::file::helpers::suggest_similar_files;
use crate::file::read::types::{DirectoryEntry, DirectoryResult, ReadResult};
use crate::file::safety::{has_binary_extension, is_blocked_device, is_internal_path};
use crate::file::tracker::{check_dedup, record_read};
use crate::redact::redact_sensitive_text;
use serde_json::Value;
use std::path::Path;

const DEFAULT_LIMIT: i64 = 500;
const MAX_LIMIT: i64 = 2000;

/// 列出目录内容
///
/// 遍历目录下的文件和子目录，返回 JSON 格式的结果。
/// 自动跳过排除目录（.venv、node_modules 等），按目录优先 + 名称排序。
///
/// # 参数
///
/// - `dir_path`: 目录的绝对路径
/// - `original_path`: 用户传入的原始路径（用于显示）
/// - `offset`: 起始索引（从 1 开始）
/// - `limit`: 最大返回条目数
///
/// # 返回
///
/// JSON 字符串，包含 entries 数组、total_count、truncated 等字段。
fn list_directory(dir_path: &Path, original_path: &str, offset: usize, limit: usize) -> String {
    let mut entries: Vec<DirectoryEntry> = Vec::new();

    let read_dir = match std::fs::read_dir(dir_path) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return common::tool_error(&format!("无权限访问目录: {original_path}"));
        }
        Err(e) => {
            return common::tool_error(&format!(
                "读取目录失败: {}: {e}",
                std::any::type_name_of_val(&e)
                    .split("::")
                    .last()
                    .unwrap_or("Error")
            ));
        }
    };

    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();

        if SEARCH_EXCLUDE_DIRS.iter().any(|d| *d == name) {
            continue;
        }

        let file_type = entry.file_type();
        let is_dir = file_type.map(|t| t.is_dir()).unwrap_or(false);

        let size = if !is_dir {
            entry.metadata().ok().map(|m| m.len())
        } else {
            None
        };

        entries.push(DirectoryEntry {
            name,
            entry_type: if is_dir {
                "dir".to_string()
            } else {
                "file".to_string()
            },
            size,
        });
    }

    entries.sort_by(|a, b| {
        let a_is_file = a.entry_type == "file";
        let b_is_file = b.entry_type == "file";
        b_is_file
            .cmp(&a_is_file)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let total_count = entries.len();
    let start_idx = offset - 1;
    let end_idx = std::cmp::min(start_idx + limit, total_count);
    let selected: Vec<_> = entries
        .into_iter()
        .skip(start_idx)
        .take(end_idx - start_idx)
        .collect();

    let truncated = end_idx < total_count;
    let result = DirectoryResult {
        result: selected,
        path: original_path.to_string(),
        total_count,
        truncated: if truncated { Some(true) } else { None },
        hint: if truncated {
            Some(format!(
                "使用 offset={} 继续读取（显示第 {offset}-{end_idx} 个条目，共 {total_count} 个）",
                end_idx + 1
            ))
        } else {
            None
        },
    };

    common::tool_result(serde_json::to_value(result).unwrap_or_default())
}

/// 读取文件内容的核心实现
///
/// 处理完整的读取流程：路径解析 → 安全检查 → 去重检查 → 读取 → 行号格式化 → 脱敏 → 返回。
///
/// # 安全检查顺序
///
/// 1. 设备文件检查（/dev/zero 等）
/// 2. 框架内部路径检查（.fuyao/.env）
/// 3. 文件存在性检查（不存在则建议相似文件名）
/// 4. 二进制文件检查
/// 5. 内容大小检查（> MAX_READ_CHARS 则拒绝）
pub fn read_file_impl(args: Value, ctx: &fuyao_api::ToolCallContext) -> String {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let offset = args
        .get("offset")
        .and_then(|v| v.as_i64())
        .unwrap_or(1)
        .max(1) as usize;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT) as usize;
    let task_id = ctx.task_id().to_string();
    let workspace = ctx.workspace().map(Path::to_path_buf);

    let resolved_path_obj = resolve_path(path, workspace.as_deref());
    let resolved_path = resolved_path_obj.to_string_lossy().to_string();

    if is_blocked_device(path) {
        return common::tool_error(&format!(
            "无法读取设备文件: {path}。该文件会产生无限输出或阻塞输入。"
        ));
    }

    if is_internal_path(path) {
        return common::tool_error(&format!(
            "拒绝读取框架内部路径: {path}。此路径包含框架内部数据，不允许直接访问。"
        ));
    }

    if !resolved_path_obj.exists() {
        let suggestions = suggest_similar_files(path, 5);
        let mut error_msg = format!("文件不存在: {path}");
        if !suggestions.is_empty() {
            error_msg.push_str("\n\n您是否想要以下文件之一？\n");
            for s in &suggestions {
                error_msg.push_str(&format!("  • {s}\n"));
            }
        }
        let mut err = serde_json::json!({"error": error_msg, "path": path});
        if !suggestions.is_empty() {
            err["suggestions"] = serde_json::json!(suggestions);
        }
        return common::tool_error_with(err);
    }

    if resolved_path_obj.is_dir() {
        return list_directory(&resolved_path_obj, path, offset, limit);
    }

    if !resolved_path_obj.is_file() {
        return common::tool_error(&format!("路径不是文件或目录: {path}"));
    }

    if has_binary_extension(&resolved_path) {
        let ext = resolved_path_obj
            .extension()
            .unwrap_or_default()
            .to_string_lossy();
        return common::tool_error(&format!("无法读取二进制文件: {path} ({ext})"));
    }

    if let Some(dedup) = check_dedup(&resolved_path, offset, limit, &task_id) {
        return common::tool_result(dedup);
    }

    record_read(path, &resolved_path, offset, limit, &task_id);

    let file_size = match resolved_path_obj.metadata() {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return common::tool_error(&format!("无权限读取文件: {path}"));
        }
        Err(_) => 0,
    };

    let content = match std::fs::read_to_string(&resolved_path_obj) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return common::tool_error(&format!("无权限读取文件: {path}"));
        }
        // 对齐 Python UnicodeDecodeError：区分 UTF-8 编码错误和其他 IO 错误
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return common::tool_error(&format!(
                "文件编码无法解析: {path}。文件可能包含非 UTF-8 字节，请用二进制编辑器查看。"
            ));
        }
        Err(e) => {
            return common::tool_error(&format!("读取文件失败: {e}"));
        }
    };

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    let start_idx = offset - 1;
    let end_idx = std::cmp::min(start_idx + limit, total_lines);

    let mut content_lines = Vec::with_capacity(end_idx - start_idx);
    for (idx, line) in lines
        .iter()
        .enumerate()
        .skip(start_idx)
        .take(end_idx - start_idx)
    {
        content_lines.push(format!("{:>6}\t{line}", idx + 1));
    }

    let output = content_lines.join("\n");

    if output.len() > MAX_READ_CHARS {
        return common::tool_error(&format!(
            "读取内容超过安全限制 ({} > {} 字符)。请使用 offset 和 limit 参数读取更小的范围。文件共 {} 行。",
            output.len(),
            MAX_READ_CHARS,
            total_lines
        ));
    }

    let output = redact_sensitive_text(&output);
    let truncated = end_idx < total_lines;

    let result = ReadResult {
        result: output,
        path: path.to_string(),
        total_lines,
        file_size,
        offset,
        limit,
        truncated: if truncated { Some(true) } else { None },
        hint: if truncated {
            Some(format!(
                "使用 offset={} 继续读取（显示第 {offset}-{end_idx} 行，共 {total_lines} 行）",
                end_idx + 1
            ))
        } else {
            None
        },
        _hint: if file_size > LARGE_FILE_HINT_BYTES && limit > 200 && truncated {
            Some(format!(
                "此文件较大 ({} 字节)。建议使用 offset 和 limit 只读取需要的部分，以节省上下文。",
                file_size
            ))
        } else {
            None
        },
    };

    common::tool_result(serde_json::to_value(result).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_existing_file() {
        let dir = std::env::temp_dir().join("fuyao_test_read_full");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string()
        });
        let result = read_file_impl(args, &fuyao_api::ToolCallContext::default());
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
        assert!(result.contains("line3"));
        assert!(result.contains("total_lines"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_with_offset_and_limit() {
        let dir = std::env::temp_dir().join("fuyao_test_read_offset_full");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "line1\nline2\nline3\nline4\nline5\n").unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "offset": 2,
            "limit": 2
        });
        let result = read_file_impl(args, &fuyao_api::ToolCallContext::default());
        assert!(result.contains("line2"));
        assert!(result.contains("line3"));
        assert!(!result.contains("line1"));
        assert!(!result.contains("line4"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_nonexistent_file() {
        let args = serde_json::json!({ "path": "/nonexistent/file.txt" });
        let result = read_file_impl(args, &fuyao_api::ToolCallContext::default());
        assert!(result.contains("文件不存在"));
    }

    #[test]
    fn read_binary_file_rejected() {
        let dir = std::env::temp_dir().join("fuyao_test_read_binary_full");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.png");
        std::fs::write(&file_path, b"\x89PNG").unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string()
        });
        let result = read_file_impl(args, &fuyao_api::ToolCallContext::default());
        assert!(result.contains("二进制文件"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_directory_contents() {
        let dir = std::env::temp_dir().join("fuyao_test_read_dir_full");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(dir.join("subdir")).unwrap();

        let result = list_directory(&dir, &dir.to_string_lossy(), 1, 100);
        assert!(result.contains("a.txt"));
        assert!(result.contains("subdir"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
