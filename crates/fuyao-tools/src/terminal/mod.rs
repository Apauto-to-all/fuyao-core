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
                "在本地终端执行 shell 命令（构建、测试、git、进程管理等）",
            )
            .string("command", "要执行的 shell 命令")
            .required()
            .integer(
                "timeout",
                format!(
                    "超时时间（秒，默认 {terminal_default}，最大 {terminal_max}）。长任务适当调大"
                ),
            )
            .default(json!(terminal_default))
            .string("workdir", "工作目录")
            .build(),
            tool_handler(bash_impl),
            false,
        ),
    );
}
