//! 文件编辑工具
//!
//! 提供查找替换式文件编辑，使用模糊匹配处理空白差异，包含外部编辑检测。
//!
//! ## 安全
//!
//! - 编辑前检查敏感路径（同 write 工具）
//! - 编辑前检查写入范围：路径必须落在工作目录或 fuyao_home 子树内，清单外硬拒绝
//! - 编辑前检测外部编辑并发出警告
//! - 编辑后更新追踪器时间戳

use crate::common::{empty_path_error, parse_tool_args, resolve_ctx_path, to_ok_output};
use crate::file::edit::backend::apply_replace;
use crate::file::edit::types::EditArgs;
use crate::file::safety::check_write_scope_ctx;
use fuyao_api::{CancellationToken, ToolCallContext, ToolError, ToolOutput};
use serde_json::Value;

/// edit 工具入口，执行查找替换
///
/// 处理查找替换请求，先做写入范围判定（清单外直接拒绝），再调用
/// `apply_replace` 执行模糊匹配替换。
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

    if let Some(err) = check_write_scope_ctx(&ctx, &file_path) {
        tracing::warn!(path = %path, action = "编辑", reason = %err, "拒绝清单外写入");
        return ToolOutput::Err(ToolError::new(err).with("path", path.as_str()).with(
            "suggestion",
            "请使用工作目录，如确需修改此文件，请将文件路径与修改内容告知用户",
        ));
    }

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

    /// 清单外拒绝：workspace 之外的文件返回限制错误且内容未被修改
    #[tokio::test]
    async fn edit_rejects_out_of_write_scope() {
        let base = std::env::temp_dir().join("fuyao_test_edit_scope");
        std::fs::remove_dir_all(&base).ok();
        let ws = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let file_path = outside.join("target.txt");
        std::fs::write(&file_path, "旧内容\n").unwrap();

        let ctx = fuyao_api::ToolCallContext {
            agent_paths: Some(fuyao_api::AgentPaths {
                workspace: Some(ws),
                fuyao_home: base.join("home"),
                ..fuyao_api::AgentPaths::default()
            }),
            ..fuyao_api::ToolCallContext::default()
        };
        let result = edit_impl(
            serde_json::json!({
                "path": file_path.to_string_lossy().to_string(),
                "old_string": "旧内容",
                "new_string": "新内容"
            }),
            ctx,
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("写入被限制在工作目录"), "实际：{result}");
        assert!(
            result.contains("请将文件路径与修改内容告知用户"),
            "实际：{result}"
        );
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "旧内容\n", "清单外文件不应被修改");

        std::fs::remove_dir_all(&base).ok();
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
