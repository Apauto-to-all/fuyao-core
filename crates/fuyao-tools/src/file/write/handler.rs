//! 文件写入处理逻辑
//!
//! 提供文件写入功能，支持创建、覆盖文件，包含敏感路径保护、外部编辑检测。
//!
//! ## 功能
//!
//! - 文件不存在则创建，父目录不存在则自动创建
//! - 文件存在则完全覆盖（非追加）
//! - 敏感路径保护（SSH 密钥、系统配置等拒绝写入）
//! - 外部编辑检测（文件被其他进程修改时发出警告）
//! - 写入后更新追踪器时间戳

use crate::common::{self, resolve_path};
use crate::file::safety::check_sensitive_path;
use crate::file::tracker::{check_file_staleness, update_read_timestamp};
use crate::file::write::types::WriteResult;
use serde_json::Value;
use std::path::Path;

/// 写入文件的核心实现
///
/// 处理完整的写入流程：路径解析 → 安全检查 → 外部编辑检测 → 创建父目录 → 写入 → 更新追踪器。
///
/// # 参数
///
/// - `args`: JSON 对象，包含 `path`（文件路径）和 `content`（写入内容）
///
/// # 返回
///
/// JSON 字符串，包含 success、path、bytes_written、created 等字段。
/// 写入失败时返回 error 和 suggestion。
pub fn write_file_impl(args: Value, ctx: &fuyao_api::ToolCallContext) -> String {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let task_id = ctx.task_id().to_string();
    let workspace = ctx.workspace().map(Path::to_path_buf);

    if path.is_empty() {
        let err = serde_json::json!({
            "error": "路径参数不能为空",
            "suggestion": "请提供有效的文件路径"
        });
        return common::tool_error_with(err);
    }

    let resolved_path_obj = resolve_path(path, workspace.as_deref());

    if let Some(err) = check_sensitive_path(path, "写入") {
        tracing::warn!(path = %path, action = "写入", reason = %err, "拒绝操作敏感路径");
        let err_json = serde_json::json!({
            "error": err,
            "path": path,
            "suggestion": "请选择非敏感路径，或使用项目目录下的文件"
        });
        return common::tool_error_with(err_json);
    }

    let stale_warning: Option<String> = check_file_staleness(path, &task_id);

    let parent = resolved_path_obj.parent();
    if let Some(dir) = parent
        && !dir.exists()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        let err_json = serde_json::json!({
            "error": format!("写入文件失败: {e}"),
            "path": path,
            "suggestion": "请检查路径是否有效，或尝试使用绝对路径"
        });
        return common::tool_error_with(err_json);
    }

    let existed = resolved_path_obj.exists();

    if let Err(e) = std::fs::write(&resolved_path_obj, &content) {
        let msg = if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!("无权限写入文件: {path}")
        } else {
            format!("写入文件失败: {e}")
        };
        let err_json = serde_json::json!({
            "error": msg,
            "path": path,
            "suggestion": "请检查文件权限，或尝试使用不同的文件路径"
        });
        return common::tool_error_with(err_json);
    }

    let bytes_written = content.len();
    update_read_timestamp(path, &task_id);

    let result = WriteResult {
        success: true,
        result: if existed {
            "文件已覆盖".to_string()
        } else {
            "文件已创建".to_string()
        },
        path: path.to_string(),
        bytes_written,
        created: !existed,
        warning: stale_warning,
    };

    common::tool_result(serde_json::to_value(result).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_new_file() {
        let dir = std::env::temp_dir().join("fuyao_test_write_full");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("new.txt");

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "content": "hello world"
        });
        let result = write_file_impl(args, &fuyao_api::ToolCallContext::default());
        assert!(result.contains("文件已创建"));

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "hello world");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn overwrite_existing_file() {
        let dir = std::env::temp_dir().join("fuyao_test_write_overwrite_full");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("existing.txt");
        std::fs::write(&file_path, "old content").unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "content": "new content"
        });
        let result = write_file_impl(args, &fuyao_api::ToolCallContext::default());
        assert!(result.contains("文件已覆盖"));

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "new content");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_creates_parent_dirs() {
        let dir = std::env::temp_dir().join("fuyao_test_write_mkdir_full");
        let file_path = dir.join("sub1/sub2/test.txt");

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "content": "nested"
        });
        let result = write_file_impl(args, &fuyao_api::ToolCallContext::default());
        assert!(result.contains("文件已创建"));

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "nested");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reject_empty_path() {
        let args = serde_json::json!({ "path": "", "content": "test" });
        let result = write_file_impl(args, &fuyao_api::ToolCallContext::default());
        assert!(result.contains("路径参数不能为空"));
    }

    #[test]
    fn reject_sensitive_path() {
        let sensitive_path = if cfg!(windows) {
            "C:\\Windows\\system32\\config"
        } else {
            "/etc/passwd"
        };
        let args = serde_json::json!({
            "path": sensitive_path,
            "content": "hacked"
        });
        let result = write_file_impl(args, &fuyao_api::ToolCallContext::default());
        assert!(result.contains("拒绝"));
    }
}
