//! MCP 类型定义

use std::collections::HashMap;

/// MCP Server 配置
///
/// 支持 stdio（command + args）和 HTTP（url）两种传输方式。
/// 通过字段判断传输类型：有 url → HTTP，有 command → stdio。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct MCPServerConfig {
    /// stdio 传输：启动命令
    pub command: Option<String>,

    /// stdio 传输：命令参数
    pub args: Option<Vec<String>>,

    /// stdio 传输：环境变量
    pub env: Option<HashMap<String, String>>,

    /// HTTP 传输：MCP endpoint URL
    pub url: Option<String>,

    /// HTTP 传输：请求头
    pub headers: Option<HashMap<String, String>>,

    /// 是否启用
    pub enabled: bool,

    /// 工具调用超时（秒）
    pub timeout: u32,

    /// 连接超时（秒）
    pub connect_timeout: u32,

    /// 工具开关，key=工具名，value=是否启用。未列出的工具默认启用
    pub tools: HashMap<String, bool>,
}

impl Default for MCPServerConfig {
    fn default() -> Self {
        Self {
            command: None,
            args: None,
            env: None,
            url: None,
            headers: None,
            enabled: true,
            timeout: 120,
            connect_timeout: 60,
            tools: HashMap::new(),
        }
    }
}

impl MCPServerConfig {
    pub fn is_http(&self) -> bool {
        self.url.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_server_config_default_is_enabled() {
        let config = MCPServerConfig::default();
        assert!(config.enabled);
        assert_eq!(config.timeout, 120);
        assert_eq!(config.connect_timeout, 60);
    }

    #[test]
    fn mcp_server_config_is_http_when_url_present() {
        let config = MCPServerConfig {
            url: Some("http://localhost:8080".to_string()),
            ..Default::default()
        };
        assert!(config.is_http());
    }

    #[test]
    fn mcp_server_config_is_not_http_when_command_present() {
        let config = MCPServerConfig {
            command: Some("node".to_string()),
            ..Default::default()
        };
        assert!(!config.is_http());
    }
}
