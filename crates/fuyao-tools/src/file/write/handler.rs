//! 文件写入处理逻辑
//!
//! 提供文件写入功能，支持创建、覆盖文件，包含敏感路径保护、覆写差异账本。
//!
//! ## 功能
//!
//! - 文件不存在则创建，父目录不存在则自动创建
//! - 文件存在则完全覆盖（非追加），不设任何前置读取要求：
//!   覆写结果携带旧内容差异，模型据此知道覆写掉了什么、可据此恢复
//! - 敏感路径保护（SSH 密钥、系统配置等拒绝写入）
//! - 新内容与原文件一致时跳过落盘，磁盘字节保持原样
//! - 写入后更新追踪器时间戳（与 read 同源的 resolve 后绝对路径）

use crate::common::{empty_path_error, parse_tool_args, resolve_ctx_path, to_ok_output};
use crate::file::edit::textutil::{normalize_line_endings, split_bom};
use crate::file::safety::check_sensitive_path;
use crate::file::tracker::update_read_timestamp;
use crate::file::write::diff::render_overwrite_diff;
use crate::file::write::types::{WriteArgs, WriteResult};
use fuyao_api::{CancellationToken, ToolCallContext, ToolError, ToolOutput};
use serde_json::Value;

/// 写入文件的核心实现
///
/// 处理完整的写入流程：参数解析 → 路径解析 → 安全检查 → 一致性比对 →
/// 创建父目录 → 写入 → 差异账本 → 更新追踪器。
///
/// # 返回
///
/// 结果信封：成功时含 path、bytes_written、created、diff、unchanged 字段；
/// 失败时为错误信封（error + suggestion / path 附加字段）。
pub async fn write_file_impl(
    args: Value,
    ctx: ToolCallContext,
    _cancel: CancellationToken,
) -> ToolOutput {
    let WriteArgs { path, content } = match parse_tool_args(args) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let task_id = ctx.task_id().to_string();

    if path.is_empty() {
        return empty_path_error();
    }

    let resolved_path_obj = resolve_ctx_path(&ctx, &path);
    let resolved_path = resolved_path_obj.to_string_lossy().to_string();

    if let Some(err) = check_sensitive_path(&path, "写入") {
        tracing::warn!(path = %path, action = "写入", reason = %err, "拒绝操作敏感路径");
        return ToolOutput::Err(
            ToolError::new(err)
                .with("path", path.as_str())
                .with("suggestion", "请选择非敏感路径，或使用项目目录下的文件"),
        );
    }

    let existed = resolved_path_obj.exists();

    // 读取旧内容用于一致性比对与差异账本（非 UTF-8 或权限不足时读取失败，
    // 写入照常进行，账本折为说明文字）
    let old_content: Option<String> = if existed {
        std::fs::read_to_string(&resolved_path_obj).ok()
    } else {
        None
    };

    // 一致性判定与 diff 展示同基准（剥离 BOM、行尾归一化）：
    // 展示为无差异的内容即视为无变化，跳过落盘以保持磁盘字节原样（含 BOM / CRLF）
    let unchanged = old_content.as_deref().is_some_and(|old| {
        let (old_text, _) = split_bom(old);
        let (new_text, _) = split_bom(&content);
        normalize_line_endings(old_text) == normalize_line_endings(new_text)
    });

    if unchanged {
        let result = WriteResult {
            path: path.clone(),
            bytes_written: 0,
            created: false,
            diff: None,
            unchanged: true,
        };
        return to_ok_output(&result);
    }

    let parent = resolved_path_obj.parent();
    if let Some(dir) = parent
        && !dir.exists()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        return ToolOutput::Err(
            ToolError::new(format!("写入文件失败: {e}"))
                .with("path", path.as_str())
                .with("suggestion", "请检查路径是否有效，或尝试使用绝对路径"),
        );
    }

    if let Err(e) = std::fs::write(&resolved_path_obj, &content) {
        let msg = if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!("无权限写入文件: {path}")
        } else {
            format!("写入文件失败: {e}")
        };
        return ToolOutput::Err(
            ToolError::new(msg)
                .with("path", path.as_str())
                .with("suggestion", "请检查文件权限，或尝试使用不同的文件路径"),
        );
    }

    let bytes_written = content.len();
    update_read_timestamp(&resolved_path, &task_id);

    // 覆写差异账本：旧内容已从磁盘消失，diff 的删除侧是其唯一留存副本；
    // 旧内容不可读（非 UTF-8 / 权限）时如实说明账本缺失
    let diff = match &old_content {
        Some(old) => Some(render_overwrite_diff(old, &content, &path)),
        None => Some(
            "原文件内容无法读取（非 UTF-8 或权限不足），无法生成覆写差异；\
旧内容已不可从本结果恢复"
                .to_string(),
        ),
    };

    let result = WriteResult {
        path: path.clone(),
        bytes_written,
        created: !existed,
        diff,
        unchanged: false,
    };

    to_ok_output(&result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_new_file() {
        let dir = std::env::temp_dir().join("fuyao_test_write_full");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("new.txt");

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "content": "hello world"
        });
        let result = write_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("\"created\":true"));

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "hello world");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn overwrite_without_read_carries_diff() {
        let dir = std::env::temp_dir().join("fuyao_test_write_overwrite_full");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("existing.txt");
        std::fs::write(&file_path, "old content").unwrap();

        // 不做任何前置读取：覆写直接执行，差异账本随结果回喂
        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "content": "new content"
        });
        let result = write_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("\"created\":false"), "实际结果: {result}");
        // 删除侧携带被覆写的旧内容，新增侧携带新内容
        assert!(result.contains("-old content"), "实际结果: {result}");
        assert!(result.contains("+new content"), "实际结果: {result}");

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "new content");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn identical_content_short_circuits() {
        let dir = std::env::temp_dir().join("fuyao_test_write_unchanged");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("same.txt");
        std::fs::write(&file_path, "same old content").unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "content": "same old content"
        });
        let result = write_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("\"unchanged\":true"), "实际结果: {result}");
        assert!(result.contains("\"bytes_written\":0"), "实际结果: {result}");
        assert!(!result.contains("\"diff\""), "实际结果: {result}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn line_ending_only_difference_is_unchanged() {
        let dir = std::env::temp_dir().join("fuyao_test_write_crlf");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("crlf.txt");
        std::fs::write(&file_path, "a\r\nb\r\n").unwrap();

        // 文本相同仅行尾不同：与 diff 展示基准一致，视为无变化并保持磁盘 CRLF 原样
        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "content": "a\nb\n"
        });
        let result = write_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("\"unchanged\":true"), "实际结果: {result}");

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "a\r\nb\r\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let dir = std::env::temp_dir().join("fuyao_test_write_mkdir_full");
        std::fs::remove_dir_all(&dir).ok();
        let file_path = dir.join("sub1/sub2/test.txt");

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "content": "nested"
        });
        let result = write_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("\"created\":true"));

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "nested");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn reject_empty_path() {
        let args = serde_json::json!({ "path": "", "content": "test" });
        let result = write_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("路径参数不能为空"));
    }

    #[tokio::test]
    async fn reject_sensitive_path() {
        let sensitive_path = if cfg!(windows) {
            "C:\\Windows\\system32\\config"
        } else {
            "/etc/passwd"
        };
        let args = serde_json::json!({
            "path": sensitive_path,
            "content": "hacked"
        });
        let result = write_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("拒绝"));
    }
}
