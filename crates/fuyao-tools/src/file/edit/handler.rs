//! 文件编辑工具
//!
//! 提供查找替换式文件编辑，使用模糊匹配处理空白差异，包含外部编辑检测。
//!
//! ## 安全
//!
//! - 编辑前检查敏感路径（同 write 工具）
//! - 编辑前检测外部编辑并发出警告
//! - 编辑后更新追踪器时间戳

use crate::common::resolve_path;
use crate::file::edit::backend::apply_replace;
use crate::file::edit::types::EditArgs;
use fuyao_api::{CancellationToken, ToolCallContext, ToolError, ToolOutput, parse_args};
use serde_json::Value;
use std::path::Path;

/// edit 工具入口，执行查找替换
///
/// 处理查找替换请求，调用 `apply_replace` 执行模糊匹配替换。
/// 失败时在错误信息后附加提示（建议使用 read 验证或 grep 定位）。
pub async fn edit_impl(
    args: Value,
    ctx: ToolCallContext,
    _cancel: CancellationToken,
) -> ToolOutput {
    let EditArgs {
        path,
        old_string,
        new_string,
        replace_all,
    } = match parse_args(args) {
        Ok(a) => a,
        Err(e) => return ToolOutput::Err(e),
    };
    let task_id = ctx.task_id().to_string();
    let workspace = ctx.workspace().map(Path::to_path_buf);

    if path.is_empty() {
        return ToolOutput::Err(
            ToolError::new("path 参数必填").with("suggestion", "请提供要修改的文件路径"),
        );
    }

    let file_path = resolve_path(&path, workspace.as_deref());

    let result = apply_replace(
        &file_path,
        &old_string,
        &new_string,
        replace_all,
        &path,
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
        let mut err = ToolError::new(format!("{}{hint}", result.error.unwrap_or_default()))
            .with("path", path.as_str())
            .with(
                "suggestion",
                "请提供更多上下文使匹配唯一，或使用 replace_all=true 替换所有匹配",
            );
        if let Some(w) = result.warning {
            err = err.with("warning", serde_json::json!(w));
        }
        return ToolOutput::Err(err);
    }

    // 成功路径：result.error 必为 None，由 skip_serializing_if 自动省略
    ToolOutput::ok(serde_json::to_value(&result).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use crate::file::edit::types::EditReplaceResult;

    #[test]
    fn edit_replace_result_serializes_all_fields() {
        let result = EditReplaceResult {
            success: true,
            path: "test.rs".to_string(),
            matches: 1,
            diff: "--- a\n+++ b".to_string(),
            strategy: None,
            warning: None,
            error: None,
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
            error: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["strategy"], "fuzzy");
        assert_eq!(json["warning"], "模糊匹配");
    }
}
