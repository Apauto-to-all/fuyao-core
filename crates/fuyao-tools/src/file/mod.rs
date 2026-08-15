//! 文件工具模块
//!
//! 提供文件读取、写入、搜索、编辑等操作。
//!
//! ## 工具列表
//!
//! - **read**: 读取文件内容（带行号、分页）或列出目录内容
//! - **write**: 写入文件（创建或覆盖），含敏感路径保护
//! - **glob**: 使用 glob 模式搜索文件名，自动遵守 .gitignore
//! - **grep**: 使用正则表达式搜索文件内容
//! - **edit**: 文件编辑，使用模糊匹配查找替换
//!
//! ## 安全防护
//!
//! - 敏感系统路径保护（/etc/passwd、SSH 密钥等）
//! - 设备文件保护（/dev/zero 等无限输出设备）
//! - 二进制文件检测（拒绝读取 .exe、.png 等）
//! - 框架内部路径保护（.fuyao/.env 等）
//! - API Key 脱敏（读取/搜索结果自动脱敏）
//! - 文件追踪器（外部编辑检测）

mod edit;
mod glob;
mod grep;
mod helpers;
mod read;
mod safety;
mod tracker;
mod write;

pub fn register(map: &mut std::collections::HashMap<String, fuyao_api::ToolEntry>) {
    read::register(map);
    write::register(map);
    glob::register(map);
    grep::register(map);
    edit::register(map);
}
