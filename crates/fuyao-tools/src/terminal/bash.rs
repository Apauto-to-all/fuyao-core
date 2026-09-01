//! bash 工具实现
//!
//! 提供命令执行的完整功能。

use crate::common::parse_tool_args;
use crate::redact::redact_sensitive_text;
use crate::terminal::execute::{execute_command, format_result};
use crate::terminal::safety::{DeleteScope, check_command_safety, validate_workdir};
use crate::terminal::shell::find_shell;
use crate::terminal::types::BashArgs;
use fuyao_api::{CancellationToken, ToolCallContext, ToolOutput};
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
    } = match parse_tool_args(args) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let raw_command = command.trim().to_string();

    if raw_command.is_empty() {
        return ToolOutput::error("命令不能为空");
    }

    // 2. 工作目录解析（优先显式 workdir，否则使用 workspace）
    let workspace = ctx.workspace().map(|p| p.to_path_buf());
    let effective_dir: Option<PathBuf> = workdir.map(PathBuf::from).or_else(|| workspace.clone());
    if let Some(dir) = &effective_dir
        && let Some(err) = validate_workdir(&dir.to_string_lossy())
    {
        return ToolOutput::error(err);
    }

    // 3. 安全检查：危险命令正则 + 递归删除范围（目标须落在 workspace 子树内）
    let scope = DeleteScope {
        cwd: effective_dir.clone(),
        allowed_root: workspace,
    };
    let security_result = check_command_safety(&raw_command, &scope);
    if security_result.blocked {
        tracing::warn!(command = %raw_command, reason = %security_result.reason, "阻止执行危险命令");
        return ToolOutput::error(security_result.reason);
    }

    // 4. 提取超时时间（默认/上限从全局配置 get_config().tools.limits 读取）
    let limits = fuyao_api::get_config().tools.limits.clone();
    let timeout_secs = timeout
        .unwrap_or(limits.terminal_default_timeout_secs)
        .min(limits.terminal_max_timeout_secs);
    let timeout = Duration::from_secs(timeout_secs);

    // 5. Shell 自动选择
    let shell_info = find_shell();

    // 6. 执行命令
    let mut result = execute_command(
        &raw_command,
        effective_dir.as_deref(),
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

    /// 构造以指定目录为 workspace 的调用上下文
    fn ctx_with_workspace(ws: &std::path::Path) -> ToolCallContext {
        ToolCallContext {
            agent_paths: Some(fuyao_api::AgentPaths {
                workspace: Some(ws.to_path_buf()),
                fuyao_home: ws.join("fuyao-home"),
                ..fuyao_api::AgentPaths::default()
            }),
            ..ToolCallContext::default()
        }
    }

    /// 递归删除工作区内目标：放行并真实删除（按实际 shell 选命令语法）
    #[tokio::test]
    async fn bash_impl_recursive_rm_inside_workspace() {
        let base = std::env::temp_dir().join("fuyao_test_bash_rm_scope");
        std::fs::remove_dir_all(&base).ok();
        let ws = base.join("ws");
        let target = ws.join("sub");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("f.txt"), "x").unwrap();

        let command = match find_shell().shell_type {
            "powershell" => "Remove-Item -Recurse -Force sub",
            "cmd" => "rd /s /q sub",
            _ => "rm -rf sub",
        };
        let args = serde_json::json!({"command": command});
        let result = bash_impl(args, ctx_with_workspace(&ws), CancellationToken::new())
            .await
            .to_wire();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            parsed["exit_code"],
            0,
            "shell={:?} 实际结果: {result}",
            find_shell().shell_type
        );
        assert!(!target.exists(), "工作区内目标应被真实删除");

        std::fs::remove_dir_all(&base).ok();
    }

    /// 递归删除工作区外目标：安全检查拒绝，磁盘未动
    #[tokio::test]
    async fn bash_impl_recursive_rm_outside_workspace_blocked() {
        let base = std::env::temp_dir().join("fuyao_test_bash_rm_scope_block");
        std::fs::remove_dir_all(&base).ok();
        let ws = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("f.txt"), "keep").unwrap();

        let args = serde_json::json!({
            "command": format!("rm -rf {}", outside.to_string_lossy())
        });
        let result = bash_impl(args, ctx_with_workspace(&ws), CancellationToken::new())
            .await
            .to_wire();
        assert!(
            result.contains("递归删除被限制在工作目录内"),
            "实际结果: {result}"
        );
        assert!(outside.join("f.txt").exists(), "工作区外目标不应被删除");

        std::fs::remove_dir_all(&base).ok();
    }
}
