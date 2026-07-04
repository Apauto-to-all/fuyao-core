//! 文件补丁工具
//!
//! 提供文件编辑功能，支持模糊匹配替换和 V4A 格式补丁，包含外部编辑检测。
//!
//! ## 模式
//!
//! - **replace 模式**: 查找并替换文本，使用 9 种模糊匹配策略处理空白差异
//! - **patch 模式**: 应用 V4A 格式多文件补丁（支持 Add/Update/Delete/Move 操作）
//!
//! ## 安全
//!
//! - 编辑前检查敏感路径（同 write 工具）
//! - 编辑前检测外部编辑并发出警告
//! - 编辑后更新追踪器时间戳

use crate::common::{self, resolve_path};
use crate::file::edit::backend::{apply_replace, apply_v4a_patch};
use crate::file::edit::types::{EditPatchResult, EditReplaceResult};
use serde_json::Value;
use std::path::Path;

/// replace 模式处理器
///
/// 处理查找替换请求，调用 `apply_replace` 执行模糊匹配替换。
/// 失败时在错误信息后附加提示（建议使用 read 验证或 grep 定位）。
fn patch_replace_handler(args: &Value, ctx: &fuyao_api::ToolCallContext) -> String {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let old_string = args
        .get("old_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let new_string = args
        .get("new_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let replace_all = args
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let task_id = ctx.task_id().to_string();
    let workspace = ctx.workspace().map(Path::to_path_buf);

    if path.is_empty() {
        let err = serde_json::json!({
            "error": "path 参数必填",
            "suggestion": "请提供要修改的文件路径"
        });
        return common::tool_error_with(err);
    }

    if args.get("old_string").is_none() || args.get("new_string").is_none() {
        let err = serde_json::json!({
            "error": "old_string 和 new_string 参数必填",
            "suggestion": "请提供要查找和替换的文本内容"
        });
        return common::tool_error_with(err);
    }

    let file_path = resolve_path(path, workspace.as_deref());

    let result = apply_replace(
        &file_path,
        old_string,
        new_string,
        replace_all,
        path,
        &task_id,
    );

    if !result.success {
        let mut hint = String::new();
        if let Some(ref err) = result.error
            && err.contains("未找到")
        {
            hint = "\n\n[提示: old_string 未找到。使用 read 验证当前内容，或使用 grep 定位文本。]"
                .to_string();
        }
        let mut err = serde_json::json!({
            "error": format!("{}{hint}", result.error.unwrap_or_default()),
            "path": path,
            "suggestion": "请提供更多上下文使匹配唯一，或使用 replace_all=true 替换所有匹配"
        });
        if let Some(w) = result.warning {
            err["warning"] = serde_json::json!(w);
        }
        return common::tool_error_with(err);
    }

    let result_json = EditReplaceResult {
        success: true,
        path: result.path,
        matches: result.matches,
        diff: result.diff,
        strategy: result.strategy,
        warning: result.warning,
    };

    common::tool_result(serde_json::to_value(result_json).unwrap_or_default())
}

/// patch 模式处理器
///
/// 处理 V4A 补丁请求，调用 `apply_v4a_patch` 解析并执行补丁。
fn patch_v4a_handler(args: &Value, ctx: &fuyao_api::ToolCallContext) -> String {
    let patch_content = args.get("patch").and_then(|v| v.as_str()).unwrap_or("");
    let task_id = ctx.task_id().to_string();
    let workspace = ctx.workspace().map(Path::to_path_buf);

    if patch_content.is_empty() {
        let err = serde_json::json!({
            "error": "patch 参数必填",
            "suggestion": "请提供 V4A 格式的补丁内容"
        });
        return common::tool_error_with(err);
    }

    let ws_path = workspace.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let result = apply_v4a_patch(patch_content, &ws_path, &task_id);

    let result_json = EditPatchResult {
        success: result.success,
        files_modified: result.files_modified,
        files_created: result.files_created,
        files_deleted: result.files_deleted,
        diff: result.diff,
        warning: result.warning,
        error: result.error,
    };

    common::tool_result(serde_json::to_value(result_json).unwrap_or_default())
}

/// edit 工具入口，根据 mode 参数分发到 replace 或 patch 处理器
pub fn edit_impl(args: Value, ctx: &fuyao_api::ToolCallContext) -> String {
    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("replace");

    match mode {
        "replace" => patch_replace_handler(&args, ctx),
        "patch" => patch_v4a_handler(&args, ctx),
        _ => common::tool_error(&format!("未知模式: {mode}，支持: replace, patch")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_replace_result_serializes_all_fields() {
        let result = EditReplaceResult {
            success: true,
            path: "test.rs".to_string(),
            matches: 1,
            diff: "--- a\n+++ b".to_string(),
            strategy: None,
            warning: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["path"], "test.rs");
        assert_eq!(json["matches"], 1);
        assert_eq!(json["diff"], "--- a\n+++ b");
        assert!(json.get("strategy").is_none());
        assert!(json.get("warning").is_none());
    }

    #[test]
    fn edit_replace_result_serializes_optional_fields() {
        let result = EditReplaceResult {
            success: true,
            path: "test.rs".to_string(),
            matches: 1,
            diff: "".to_string(),
            strategy: Some("fuzzy".to_string()),
            warning: Some("模糊匹配".to_string()),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["strategy"], "fuzzy");
        assert_eq!(json["warning"], "模糊匹配");
    }

    #[test]
    fn edit_patch_result_serializes_all_fields() {
        let result = EditPatchResult {
            success: true,
            files_modified: vec!["a.rs".to_string()],
            files_created: vec![],
            files_deleted: vec![],
            diff: "patch diff".to_string(),
            warning: None,
            error: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["files_modified"].as_array().unwrap().len(), 1);
        assert_eq!(json["files_created"].as_array().unwrap().len(), 0);
        assert_eq!(json["files_deleted"].as_array().unwrap().len(), 0);
        assert_eq!(json["diff"], "patch diff");
        assert!(json.get("warning").is_none());
        assert!(json.get("error").is_none());
    }

    #[test]
    fn edit_patch_result_serializes_optional_fields() {
        let result = EditPatchResult {
            success: false,
            files_modified: vec![],
            files_created: vec![],
            files_deleted: vec![],
            diff: "".to_string(),
            warning: Some("警告".to_string()),
            error: Some("错误".to_string()),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["warning"], "警告");
        assert_eq!(json["error"], "错误");
    }

    #[test]
    fn edit_patch_result_handles_multiple_files() {
        let result = EditPatchResult {
            success: true,
            files_modified: vec!["a.rs".to_string(), "b.rs".to_string()],
            files_created: vec!["c.rs".to_string()],
            files_deleted: vec!["d.rs".to_string()],
            diff: "".to_string(),
            warning: None,
            error: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["files_modified"].as_array().unwrap().len(), 2);
        assert_eq!(json["files_created"].as_array().unwrap().len(), 1);
        assert_eq!(json["files_deleted"].as_array().unwrap().len(), 1);
    }
}
