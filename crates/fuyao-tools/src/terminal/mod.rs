//! 终端工具模块
//!
//! 提供 bash 命令执行功能，含安全检查、输出脱敏等。
//!
//! ## 工具列表
//!
//! - **bash**: 执行 shell 命令，支持超时、输出截断、Shell 自动选择等
//!
//! ## 功能特性
//!
//! - Shell 自动选择（Windows: Git Bash > PowerShell > cmd，Unix: bash > sh）
//! - 多编码解码（UTF-16/UTF-8/GBK）
//! - ANSI 转义清理
//! - 输出截断（头40% + 尾60%）
//! - 结构化结果返回
//! - workdir 参数
//! - 输出脱敏
//! - 退出码解读
//! - 执行耗时统计
//! - 环境变量屏蔽
//! - 超时处理 + 进程组杀死

mod bash;
mod execute;
mod exit_code;
mod output;
mod safety;
mod shell;
mod types;

use crate::config::{TERMINAL_DEFAULT_TIMEOUT, TERMINAL_MAX_TIMEOUT};
use crate::registry::ToolEntry;
use bash::bash_impl;
use fuyao_api::{ToolDefinition, ToolFn, ToolParameterProperty, ToolParameters};
use std::collections::HashMap;

/// 注册 bash 工具
pub fn register(map: &mut HashMap<&'static str, ToolEntry>) {
    let handler: ToolFn =
        std::sync::Arc::new(|args, ctx| Box::pin(async move { bash_impl(args, &ctx).await }));

    let mut properties = HashMap::new();
    properties.insert(
        "command".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "要执行的 shell 命令".to_string(),
            default: None,
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "timeout".to_string(),
        ToolParameterProperty {
            kind: "integer".to_string(),
            description: format!(
                "超时时间(秒, 默认: {TERMINAL_DEFAULT_TIMEOUT}, 最大: {TERMINAL_MAX_TIMEOUT})"
            ),
            default: Some(serde_json::json!(TERMINAL_DEFAULT_TIMEOUT)),
            enum_values: None,
            items: None,
        },
    );
    properties.insert(
        "workdir".to_string(),
        ToolParameterProperty {
            kind: "string".to_string(),
            description: "工作目录。不传则默认使用 Agent workspace。".to_string(),
            default: None,
            enum_values: None,
            items: None,
        },
    );

    let definition = ToolDefinition {
        kind: "function".to_string(),
        function: fuyao_api::ToolSchema {
            name: "bash".to_string(),
            description: "在本地终端执行 shell 命令。\n\n\
                适用场景：构建、安装、git 操作、进程管理、运行脚本、包管理器等需要 shell 的操作。\n\n\
                不要用 bash 做以下操作（已有专用工具）：\n\
                - 读文件 → 用 read\n\
                - 写文件 → 用 write\n\
                - 搜索文件内容 → 用 search\n\
                - 编辑文件 → 用 edit\n\n\
                Shell 自动选择（每次执行结果返回 shell_type 字段）：\n\
                - Windows: git_bash（优先）> powershell > cmd\n\
                - Linux/Mac: bash > sh\n\n\
                Git Bash 使用 Unix 语法（路径用 /，不支持 CMD 命令如 dir/del）。\n\
                PowerShell 使用 PowerShell 语法（路径用 \\ 或 /）。\n\
                CMD 使用 Windows CMD 语法（路径用 \\）。\n\n\
                命令在超时时间内同步执行，完成后返回完整输出。\
                设置合理的 timeout（长任务用 300，短命令用默认 120）。\
                命令执行会被安全检查，危险操作会被阻止。"
                .to_string(),
            parameters: ToolParameters {
                kind: "object".to_string(),
                properties,
                required: vec!["command".to_string()],
            },
        },
    };

    map.insert(
        "bash",
        ToolEntry {
            definition,
            handler,
        },
    );
}
