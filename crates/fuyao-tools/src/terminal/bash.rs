//! bash 工具实现
//!
//! 提供命令执行的完整功能。

use crate::redact::redact_sensitive_text;
use crate::terminal::execute::{execute_command, format_result};
use crate::terminal::safety::{check_command_safety, validate_workdir};
use crate::terminal::shell::find_shell;
use fuyao_api::ToolCallContext;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

/// bash 工具实现
pub(crate) async fn bash_impl(args: Value, ctx: &ToolCallContext) -> String {
    // 1. 参数校验
    let raw_command = match args.get("command").and_then(|v| v.as_str()) {
        Some(cmd) => cmd.trim().to_string(),
        None => return crate::common::tool_error("缺少 command 参数"),
    };

    if raw_command.is_empty() {
        return crate::common::tool_error("命令不能为空");
    }

    // 2. 安全检查
    let security_result = check_command_safety(&raw_command);
    if security_result.blocked {
        return crate::common::tool_error(&security_result.reason);
    }

    // 3. 提取超时时间（默认/上限从全局配置 get_config().tools.limits 读取）
    let limits = fuyao_api::get_config().tools.limits.clone();
    let timeout_secs = args
        .get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(limits.terminal_default_timeout_secs)
        .min(limits.terminal_max_timeout_secs);
    let timeout = Duration::from_secs(timeout_secs);

    // 4. 确定工作目录（优先显式 workdir，否则使用 workspace）
    let explicit_workdir: Option<String> = args
        .get("workdir")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let workspace_dir = ctx.workspace().map(|p| p.to_string_lossy().to_string());
    let workdir = explicit_workdir.as_deref().or(workspace_dir.as_deref());

    // 5. 工作目录校验
    if let Some(dir) = workdir
        && let Some(err) = validate_workdir(dir)
    {
        return crate::common::tool_error(&err);
    }

    // 6. Shell 自动选择
    let shell_info = find_shell();

    // 7. 执行命令
    let workdir_path = workdir.map(PathBuf::from);
    let mut result =
        execute_command(&raw_command, workdir_path.as_deref(), timeout, shell_info).await;

    // 8. 输出脱敏（防止 env/printenv 等命令泄漏 API key）
    if !result.output.is_empty() {
        result.output = redact_sensitive_text(&result.output);
    }

    // 9. 格式化结果
    format_result(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bash_impl_missing_command() {
        let args = serde_json::json!({});
        let ctx = ToolCallContext::default();
        let result = bash_impl(args, &ctx).await;
        assert!(result.contains("缺少 command 参数"));
    }

    #[tokio::test]
    async fn bash_impl_empty_command() {
        let args = serde_json::json!({"command": ""});
        let ctx = ToolCallContext::default();
        let result = bash_impl(args, &ctx).await;
        assert!(result.contains("命令不能为空"));
    }

    #[tokio::test]
    async fn bash_impl_blocked_command() {
        let args = serde_json::json!({"command": "mkfs.ext4 /dev/sda1"});
        let ctx = ToolCallContext::default();
        let result = bash_impl(args, &ctx).await;
        assert!(result.contains("被阻止"));
    }

    #[tokio::test]
    async fn bash_impl_echo_command() {
        let args = serde_json::json!({"command": "echo hello_world_test"});
        let ctx = ToolCallContext::default();
        let result = bash_impl(args, &ctx).await;
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(
            parsed["output"]
                .as_str()
                .unwrap()
                .contains("hello_world_test")
        );
        assert_eq!(parsed["success"], true);
    }

    #[tokio::test]
    async fn bash_impl_workdir_param() {
        let args = serde_json::json!({
            "command": "echo test_workdir",
            "workdir": if cfg!(windows) { "C:\\" } else { "/tmp" }
        });
        let ctx = ToolCallContext::default();
        let result = bash_impl(args, &ctx).await;
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["output"].as_str().unwrap().contains("test_workdir"));
    }
}
