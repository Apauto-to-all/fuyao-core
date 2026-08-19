//! bash 工具实现
//!
//! 提供命令执行的完整功能。

use crate::redact::redact_sensitive_text;
use crate::terminal::execute::{execute_command, format_result};
use crate::terminal::safety::{check_command_safety, validate_workdir};
use crate::terminal::shell::find_shell;
use crate::terminal::types::BashArgs;
use fuyao_api::{CancellationToken, ToolCallContext, ToolOutput, parse_args};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

/// bash 工具实现
pub(crate) async fn bash_impl(
    args: Value,
    ctx: ToolCallContext,
    cancel: CancellationToken,
) -> ToolOutput {
    // 1. 参数解析
    let BashArgs {
        command,
        timeout,
        workdir,
    } = match parse_args(args) {
        Ok(a) => a,
        Err(e) => return ToolOutput::Err(e),
    };
    let raw_command = command.trim().to_string();

    if raw_command.is_empty() {
        return ToolOutput::error("命令不能为空");
    }

    // 2. 安全检查
    let security_result = check_command_safety(&raw_command);
    if security_result.blocked {
        tracing::warn!(command = %raw_command, reason = %security_result.reason, "阻止执行危险命令");
        return ToolOutput::error(security_result.reason);
    }

    // 3. 提取超时时间（默认/上限从全局配置 get_config().tools.limits 读取）
    let limits = fuyao_api::get_config().tools.limits.clone();
    let timeout_secs = timeout
        .unwrap_or(limits.terminal_default_timeout_secs)
        .min(limits.terminal_max_timeout_secs);
    let timeout = Duration::from_secs(timeout_secs);

    // 4. 工作目录校验（优先显式 workdir，否则使用 workspace）
    let workspace_dir = ctx.workspace().map(|p| p.to_string_lossy().to_string());
    let workdir = workdir.as_deref().or(workspace_dir.as_deref());
    if let Some(dir) = workdir
        && let Some(err) = validate_workdir(dir)
    {
        return ToolOutput::error(err);
    }

    // 5. Shell 自动选择
    let shell_info = find_shell();

    // 6. 执行命令
    let workdir_path = workdir.map(PathBuf::from);
    let mut result = execute_command(
        &raw_command,
        workdir_path.as_deref(),
        timeout,
        shell_info,
        cancel,
    )
    .await;

    // 7. 输出脱敏（防止 env/printenv 等命令泄漏 API key）
    if !result.output.is_empty() {
        result.output = redact_sensitive_text(&result.output);
    }

    // 8. 格式化结果
    format_result(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bash_impl_missing_command() {
        let args = serde_json::json!({});
        let ctx = ToolCallContext::default();
        let result = bash_impl(args, ctx, CancellationToken::new())
            .await
            .to_wire();
        // 类型化解析：缺 command 字段在反序列化时即报错
        assert!(result.contains("缺少必填参数"), "实际：{result}");
        assert!(result.contains("command"), "实际：{result}");
    }

    #[tokio::test]
    async fn bash_impl_empty_command() {
        let args = serde_json::json!({"command": ""});
        let ctx = ToolCallContext::default();
        let result = bash_impl(args, ctx, CancellationToken::new())
            .await
            .to_wire();
        assert!(result.contains("命令不能为空"));
    }

    #[tokio::test]
    async fn bash_impl_blocked_command() {
        let args = serde_json::json!({"command": "mkfs.ext4 /dev/sda1"});
        let ctx = ToolCallContext::default();
        let result = bash_impl(args, ctx, CancellationToken::new())
            .await
            .to_wire();
        assert!(result.contains("被阻止"));
    }

    #[tokio::test]
    async fn bash_impl_echo_command() {
        let args = serde_json::json!({"command": "echo hello_world_test"});
        let ctx = ToolCallContext::default();
        let result = bash_impl(args, ctx, CancellationToken::new())
            .await
            .to_wire();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(
            parsed["output"]
                .as_str()
                .unwrap()
                .contains("hello_world_test")
        );
        assert_eq!(parsed["exit_code"], 0);
    }
    #[tokio::test]
    async fn bash_impl_workdir_param() {
        let args = serde_json::json!({
            "command": "echo test_workdir",
            "workdir": if cfg!(windows) { "C:\\" } else { "/tmp" }
        });
        let ctx = ToolCallContext::default();
        let result = bash_impl(args, ctx, CancellationToken::new())
            .await
            .to_wire();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["output"].as_str().unwrap().contains("test_workdir"));
    }
}
