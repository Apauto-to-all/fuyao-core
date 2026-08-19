//! 终端工具模块
//!
//! 提供 bash 命令执行功能，含安全检查、输出脱敏等。
//!
//! ## 工具列表
//!
//! - **bash**: 执行 shell 命令，支持超时、输出截断、Shell 选择等
//!
//! ## 功能特性
//!
//! - Shell 选择（默认 auto 自动探测：Windows: Git Bash > PowerShell > cmd，Unix: bash > sh；
//!   可经 `[tools.terminal].shell` 显式指定，非法配置启动期报错）
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

// 供引擎启动校验挂载（fuyao-app init）调用
pub use shell::validate_shell_name;

use bash::bash_impl;
use fuyao_api::{ToolDefinition, ToolEntry, insert_tool, tool_handler};
use serde_json::json;
use std::collections::HashMap;

/// bash 工具基础描述（固定文案：用途 + 搜索路由指引；shell 语法提示按需追加）
///
/// 搜索路由指引：文件 / 内容查找有专用工具（glob / grep），比在终端执行
/// find / grep 更安全、结构化、跨平台一致，模型应优先选用专用工具。
const BASH_DESCRIPTION_BASE: &str = "在本地终端执行 shell 命令（构建、测试、git、进程管理等）。文件或内容搜索应使用 glob / grep 专用工具，而非在终端执行 find / grep 等命令。";

/// 注册 bash 工具
pub fn register(map: &mut HashMap<String, ToolEntry>) {
    // 超时默认/上限从全局配置读取，反映到工具参数描述
    let limits = fuyao_api::get_config().tools.limits.clone();
    let terminal_default = limits.terminal_default_timeout_secs;
    let terminal_max = limits.terminal_max_timeout_secs;
    // shell 语法提示按实际解析结果动态披露：模型需知道命令将经哪个 shell 执行，
    // 才能选用正确的命令语法（路径写法、内建命令等）
    let description = bash_description(shell::find_shell().shell_type);

    insert_tool(
        map,
        ToolEntry::new(
            ToolDefinition::builder("bash", description)
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

/// 组装 bash 工具描述：基础描述 + 按 shell 类型追加语法提示行
///
/// 语法提示只对与 Unix 假设不一致的 shell 追加（见 `shell_syntax_hint`），
/// 追加在末尾且不动基础文案。纯函数便于单测。
fn bash_description(shell_type: &str) -> String {
    match shell::shell_syntax_hint(shell_type) {
        Some(hint) => format!("{BASH_DESCRIPTION_BASE}{hint}"),
        None => BASH_DESCRIPTION_BASE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bash_description：Windows 系 shell 在末尾追加语法提示，基础文案（含搜索路由指引）保持原样
    #[test]
    fn bash_description_appends_hint_for_windows_shells() {
        let d = bash_description("git_bash");
        assert!(d.starts_with("在本地终端执行 shell 命令（构建、测试、git、进程管理等）"));
        assert!(d.ends_with(
            "命令经 Git Bash 执行，使用 Unix 语法（路径用 /，不支持 dir/del 等 CMD 命令）。"
        ));

        assert_eq!(
            bash_description("powershell"),
            "在本地终端执行 shell 命令（构建、测试、git、进程管理等）。文件或内容搜索应使用 glob / grep 专用工具，而非在终端执行 find / grep 等命令。命令经 PowerShell 执行。"
        );
        assert_eq!(
            bash_description("cmd"),
            "在本地终端执行 shell 命令（构建、测试、git、进程管理等）。文件或内容搜索应使用 glob / grep 专用工具，而非在终端执行 find / grep 等命令。命令经 CMD 执行（路径用 \\）。"
        );
    }

    /// bash_description：bash / sh 与 Unix 原生假设一致，描述即基础文案（含搜索路由指引）
    #[test]
    fn bash_description_unchanged_for_unix_shells() {
        for shell_type in ["bash", "sh"] {
            assert_eq!(
                bash_description(shell_type),
                "在本地终端执行 shell 命令（构建、测试、git、进程管理等）。文件或内容搜索应使用 glob / grep 专用工具，而非在终端执行 find / grep 等命令。"
            );
        }
    }
}
