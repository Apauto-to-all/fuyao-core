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
pub mod types;

use bash::bash_impl;
use fuyao_api::{ToolDefinition, ToolEntry, insert_tool, tool_handler};
use serde_json::json;
use std::collections::HashMap;

/// 注册 bash 工具
pub fn register(map: &mut HashMap<String, ToolEntry>) {
    // 超时默认/上限从全局配置读取，反映到工具参数描述
    let limits = fuyao_api::get_config().tools.limits.clone();
    let terminal_default = limits.terminal_default_timeout_secs;
    let terminal_max = limits.terminal_max_timeout_secs;

    insert_tool(
        map,
        ToolEntry::new(
            ToolDefinition::builder(
                "bash",
                "在本地终端执行 shell 命令。\n\n\
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
                    命令执行会被安全检查，危险操作会被阻止。",
            )
            .string("command", "要执行的 shell 命令")
            .required()
            .integer(
                "timeout",
                format!("超时时间(秒, 默认: {terminal_default}, 最大: {terminal_max})"),
            )
            .default(json!(terminal_default))
            .string("workdir", "工作目录。不传则默认使用 Agent workspace。")
            .build(),
            tool_handler(bash_impl),
            false,
        ),
    );
}
