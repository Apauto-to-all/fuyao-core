//! MCP 协议常量
//!
//! 可调参数（超时/重连/熔断等）已迁移至 `fuyao_api::config::McpGlobalConfig`，
//! 通过 `[mcp]` 配置段读取。此处仅保留协议固定值。

/// 最新 MCP 协议版本（协议固定值，不纳入配置）
// TODO: 当前未被使用，保留供未来协议版本协商使用
#[allow(dead_code)]
pub const LATEST_PROTOCOL_VERSION: &str = "2025-03-26";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_not_empty() {
        assert!(!LATEST_PROTOCOL_VERSION.is_empty());
    }
}
