//! 文件编辑工具
//!
//! 提供查找替换式文件编辑，使用模糊匹配处理空白差异，包含外部编辑检测。
//!
//! ## 安全
//!
//! - 编辑前检查敏感路径（同 write 工具）
//! - 编辑前检测外部编辑并发出警告
//! - 编辑后更新追踪器时间戳

use crate::common::{empty_path_error, parse_tool_args, resolve_ctx_path, to_ok_output};
use crate::file::edit::backend::apply_replace;
use crate::file::edit::types::EditArgs;
use fuyao_api::{CancellationToken, ToolCallContext, ToolError, ToolOutput};
use serde_json::Value;

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
    } = match parse_tool_args(args) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let task_id = ctx.task_id().to_string();

    if path.is_empty() {
        return empty_path_error();
    }

    let file_path = resolve_ctx_path(&ctx, &path);

    let result = match apply_replace(
        &file_path,
        &old_string,
        &new_string,
        replace_all,
        &path,
        &task_id,
    ) {
        Ok(r) => r,
        Err(e) => {
            let mut hint = String::new();
            if e.contains("未找到") {
                hint =
                    "\n\n[提示: old_string 未找到。使用 read 验证当前内容，或使用 grep 定位文本。]"
                        .to_string();
            }
            let err = ToolError::new(format!("{e}{hint}"))
                .with("path", path.as_str())
                .with(
                    "suggestion",
                    "请提供更多上下文使匹配唯一，或使用 replace_all=true 替换所有匹配",
                );
            return ToolOutput::Err(err);
        }
    };

    to_ok_output(&result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::edit::types::EditReplaceResult;

    /// 空路径：统一文案与 suggestion（与 write 的空路径信封一致）
    #[tokio::test]
    async fn edit_rejects_empty_path() {
        let result = edit_impl(
            serde_json::json!({ "path": "", "old_string": "a", "new_string": "b" }),
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("路径参数不能为空"), "实际：{result}");
        assert!(result.contains("请提供有效的文件路径"), "实际：{result}");
    }

    #[test]
    fn edit_replace_result_serializes_all_fields() {
        let result = EditReplaceResult {
            path: "test.rs".to_string(),
            matches: 1,
            diff: "--- a\n+++ b".to_string(),
            strategy: None,
            warning: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["path"], "test.rs");
        assert_eq!(json["matches"], 1);
        assert_eq!(json["diff"], "--- a\n+++ b");
        assert!(json.get("strategy").is_none());
        assert!(json.get("warning").is_none());
    }

    #[test]
    fn edit_replace_result_serializes_optional_fields() {
        let result = EditReplaceResult {
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
}
