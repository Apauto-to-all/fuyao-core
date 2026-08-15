//! MCP 模块入口
//!
//! MCPManager 是顶层编排器，管理多个 MCP Server 的连接生命周期，
//! 提供工具发现、注册、调用的统一接口。

mod bridge;
mod circuit_breaker;
mod connection;
mod recovery;
mod schema;
mod security;

use std::collections::HashMap;
use std::sync::Arc;

use fuyao_api::{MCPServerConfig, ToolOutput};
use tokio::sync::Mutex;

use crate::circuit_breaker::CircuitBreaker;

use connection::{MCPConnection, McpToolInfo};

/// MCP 管理器错误
#[derive(Debug, thiserror::Error)]
pub enum MCPManagerError {
    #[error("MCP server '{0}' 不存在")]
    ServerNotFound(String),

    #[error("MCP server '{0}' 未连接")]
    ServerNotConnected(String),

    #[error("MCP server '{server}' 启动失败: {reason}")]
    ServerStartFailed { server: String, reason: String },

    #[error("MCP server '{server}' 工具 '{tool}' 不存在")]
    ToolNotFound { server: String, tool: String },

    #[error("MCP 调用失败: {0}")]
    CallFailed(String),
}

/// MCP 工具注册信息
#[derive(Debug, Clone)]
pub struct RegisteredTool {
    /// 前缀名（mcp_server_tool）
    pub prefixed_name: String,
    /// 原始工具名
    pub original_name: String,
    /// 所属 server
    pub server_name: String,
    /// 工具描述
    pub description: String,
    /// 工具 schema 定义（中立形态）
    pub schema: fuyao_api::ToolDefinition,
}

/// MCP 管理器
///
/// Server 连接类型别名
type ConnectionEntry = Arc<Mutex<Option<MCPConnection>>>;

/// 管理多个 MCP Server 的连接、工具发现和调用。
/// 通过 `Arc<Mutex>` 实现内部可变性，支持并发访问。
pub struct MCPManager {
    /// Server 连接映射
    connections: Arc<Mutex<HashMap<String, ConnectionEntry>>>,
    /// 已注册的工具映射（prefixed_name → RegisteredTool）
    registered_tools: Arc<Mutex<HashMap<String, RegisteredTool>>>,
    /// Server 配置映射
    configs: HashMap<String, MCPServerConfig>,
    /// 熔断器（实例级状态，跨 manager 隔离）
    breaker: Arc<CircuitBreaker>,
}

impl MCPManager {
    /// 创建新的 MCP 管理器
    pub fn new(configs: HashMap<String, MCPServerConfig>) -> Self {
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            registered_tools: Arc::new(Mutex::new(HashMap::new())),
            configs,
            breaker: Arc::new(CircuitBreaker::new()),
        }
    }

    /// 从全局配置创建 MCP 管理器
    ///
    /// 统一从 `get_config().mcp_servers` 读取（已由加载层完成 `${VAR}` 插值）。
    /// 调用方需先经应用装配（fuyao-app 的 `init_engine` / `start`，或显式 `set_config`）注入配置。
    pub fn from_config() -> Self {
        let configs = fuyao_api::get_config().mcp_servers.clone();
        Self::new(configs)
    }

    /// 启动所有 MCP Server 连接
    ///
    /// 对每个配置的 server 尝试连接，失败的 server 记录错误但不阻塞其他 server。
    /// `enabled = false` 的 server 跳过（不连接、不发现工具）。
    /// 返回 (成功数, 失败数, 失败信息列表)。
    pub async fn start_all(&self) -> (usize, usize, Vec<(String, String)>) {
        let mut success = 0;
        let mut failures = Vec::new();

        for (name, cfg) in &self.configs {
            if !cfg.enabled {
                tracing::info!(server = %name, "MCP server 被配置禁用，跳过启动");
                continue;
            }
            match self.start_server(name, cfg.clone()).await {
                Ok(()) => success += 1,
                Err(e) => failures.push((name.clone(), e.to_string())),
            }
        }

        let failure_count = failures.len();
        (success, failure_count, failures)
    }

    /// 启动单个 MCP Server 连接
    pub async fn start_server(
        &self,
        server_name: &str,
        config: MCPServerConfig,
    ) -> Result<(), MCPManagerError> {
        let mut conn = MCPConnection::new(server_name.to_string(), config.clone());

        if let Err(e) = conn.start().await {
            tracing::error!(server = %server_name, cause = %e, "MCP server 启动失败");
            return Err(MCPManagerError::ServerStartFailed {
                server: server_name.to_string(),
                reason: e.to_string(),
            });
        }

        // 发现工具并注册
        let tools = conn.tools.clone();
        let registered = register_server_tools(server_name, &config, &tools);

        // 保存连接
        {
            let mut connections = self.connections.lock().await;
            connections.insert(server_name.to_string(), Arc::new(Mutex::new(Some(conn))));
        }

        // 保存注册信息
        {
            let mut reg = self.registered_tools.lock().await;
            for tool in registered {
                reg.insert(tool.prefixed_name.clone(), tool);
            }
        }

        tracing::info!(name = %server_name, tools = tools.len(), "MCP server 启动");

        Ok(())
    }

    /// 停止所有 MCP Server 连接
    pub async fn stop_all(&self) {
        let mut connections = self.connections.lock().await;
        for (_, conn_arc) in connections.drain() {
            let mut guard = conn_arc.lock().await;
            if let Some(ref mut conn) = *guard {
                conn.disconnect().await;
            }
        }

        let mut reg = self.registered_tools.lock().await;
        reg.clear();
    }

    /// 停止单个 MCP Server 连接
    pub async fn stop_server(&self, server_name: &str) -> Result<(), MCPManagerError> {
        let mut connections = self.connections.lock().await;
        if let Some(conn_arc) = connections.remove(server_name) {
            let mut guard = conn_arc.lock().await;
            if let Some(ref mut conn) = *guard {
                conn.disconnect().await;
            }
        }

        // 移除该 server 的注册工具
        {
            let mut reg = self.registered_tools.lock().await;
            reg.retain(|_, tool| tool.server_name != server_name);
        }

        Ok(())
    }

    /// 获取所有已注册的工具
    pub async fn get_registered_tools(&self) -> Vec<RegisteredTool> {
        let reg = self.registered_tools.lock().await;
        reg.values().cloned().collect()
    }

    /// 获取所有已注册工具的 ToolDefinition 列表
    pub async fn get_tool_definitions(&self) -> Vec<fuyao_api::ToolDefinition> {
        let reg = self.registered_tools.lock().await;
        reg.values().map(|t| t.schema.clone()).collect()
    }

    /// 获取所有工具的注册条目
    ///
    /// 返回可直接注入引擎工具注册表的 [`ToolEntry`] 列表——
    /// schema 是强类型 [`fuyao_api::ToolDefinition`]（name 即 prefixed_name），
    /// handler 通过 make_tool_call_handler 生成，绑定了 MCPConnection 引用。
    pub async fn get_tool_entries(&self) -> Vec<fuyao_api::ToolEntry> {
        let reg = self.registered_tools.lock().await;
        let connections = self.connections.lock().await;

        let mut entries = Vec::new();
        for tool in reg.values() {
            let Some(conn) = connections.get(&tool.server_name) else {
                continue;
            };

            let mcp_cfg = fuyao_api::get_config();
            let timeout = self
                .configs
                .get(&tool.server_name)
                .map(|c| c.timeout)
                .unwrap_or(mcp_cfg.mcp.tool_timeout_secs as u32);

            let handler = bridge::make_tool_call_handler(
                conn.clone(),
                self.breaker.clone(),
                tool.original_name.clone(),
                tool.server_name.clone(),
                timeout,
            );

            entries.push(fuyao_api::ToolEntry::new(
                tool.schema.clone(),
                handler,
                false,
            ));
        }

        entries
    }

    /// 调用 MCP 工具
    ///
    /// 通过前缀名（mcp_server_tool）调用工具。
    pub async fn call_tool(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, MCPManagerError> {
        // 查找工具注册信息
        let (server_name, original_name) = {
            let reg = self.registered_tools.lock().await;
            let tool = reg
                .get(prefixed_name)
                .ok_or_else(|| MCPManagerError::ToolNotFound {
                    server: String::new(),
                    tool: prefixed_name.to_string(),
                })?;
            (tool.server_name.clone(), tool.original_name.clone())
        };

        // 查找连接
        let conn_arc = {
            let connections = self.connections.lock().await;
            connections
                .get(&server_name)
                .cloned()
                .ok_or_else(|| MCPManagerError::ServerNotFound(server_name.clone()))?
        };

        // 调用工具
        let guard = conn_arc.lock().await;
        let conn = guard
            .as_ref()
            .ok_or_else(|| MCPManagerError::ServerNotConnected(server_name.clone()))?;

        let result = match conn.call_tool(&original_name, arguments).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    server = %server_name,
                    tool = %original_name,
                    cause = %e,
                    "MCP 工具调用失败"
                );
                return Err(MCPManagerError::CallFailed(e.to_string()));
            }
        };

        // 处理结果
        if result.is_error.unwrap_or(false) {
            let error_text = bridge::extract_error_text(&result);
            tracing::warn!(
                server = %server_name,
                tool = %original_name,
                cause = %error_text,
                "MCP 工具调用失败"
            );
            return Err(MCPManagerError::CallFailed(security::sanitize_error(
                &error_text,
            )));
        }

        Ok(ToolOutput::ok(bridge::extract_call_output(&result)))
    }

    /// 刷新工具列表
    ///
    /// 重新发现指定 server 的工具并更新注册信息。
    pub async fn refresh_tools(&self, server_name: &str) -> Result<(), MCPManagerError> {
        let conn_arc = {
            let connections = self.connections.lock().await;
            connections
                .get(server_name)
                .cloned()
                .ok_or_else(|| MCPManagerError::ServerNotFound(server_name.to_string()))?
        };

        let (new_tools, config) = {
            let guard = conn_arc.lock().await;
            let conn = guard
                .as_ref()
                .ok_or_else(|| MCPManagerError::ServerNotConnected(server_name.to_string()))?;

            let list_result = conn
                .list_tools()
                .await
                .map_err(|e| MCPManagerError::CallFailed(e.to_string()))?;

            let tools: Vec<McpToolInfo> = list_result
                .tools
                .iter()
                .map(|tool| McpToolInfo {
                    name: tool.name.to_string(),
                    description: tool.description.as_ref().map(|d| d.to_string()),
                    input_schema: tool.schema_as_json_value(),
                })
                .collect();

            let config = self.configs.get(server_name).cloned().unwrap_or_default();

            (tools, config)
        };

        // 移除旧工具
        {
            let mut reg = self.registered_tools.lock().await;
            reg.retain(|_, tool| tool.server_name != server_name);
        }

        // 注册新工具
        let registered = register_server_tools(server_name, &config, &new_tools);
        {
            let mut reg = self.registered_tools.lock().await;
            for tool in registered {
                reg.insert(tool.prefixed_name.clone(), tool);
            }
        }

        Ok(())
    }

    /// 获取连接状态
    pub async fn get_server_status(&self) -> HashMap<String, bool> {
        let connections = self.connections.lock().await;
        let mut status = HashMap::new();
        for (name, conn_arc) in connections.iter() {
            let guard = conn_arc.lock().await;
            let connected = guard.as_ref().map(|c| c.is_connected()).unwrap_or(false);
            status.insert(name.clone(), connected);
        }
        status
    }

    /// 获取配置的 server 列表
    pub fn get_configured_servers(&self) -> Vec<String> {
        self.configs.keys().cloned().collect()
    }
}

/// 注册 server 工具到注册表
fn register_server_tools(
    server_name: &str,
    config: &MCPServerConfig,
    tools: &[McpToolInfo],
) -> Vec<RegisteredTool> {
    let tools_filter = &config.tools;

    tools
        .iter()
        .filter(|tool| bridge::should_register_tool(&tool.name, tools_filter))
        .map(|tool| {
            let prefixed_name = bridge::build_prefixed_name(server_name, &tool.name);
            let description = tool
                .description
                .clone()
                .unwrap_or_else(|| format!("MCP tool {} from {}", tool.name, server_name));

            let schema =
                build_tool_schema_from_info(&prefixed_name, &description, &tool.input_schema);

            RegisteredTool {
                prefixed_name: prefixed_name.clone(),
                original_name: tool.name.clone(),
                server_name: server_name.to_string(),
                description,
                schema,
            }
        })
        .collect()
}

/// 从工具信息构建 ToolDefinition
fn build_tool_schema_from_info(
    prefixed_name: &str,
    description: &str,
    input_schema: &serde_json::Value,
) -> fuyao_api::ToolDefinition {
    let normalized = schema::normalize_mcp_input_schema(input_schema);

    let mut properties = HashMap::new();
    let mut required = Vec::new();

    if let Some(props) = normalized.get("properties").and_then(|v| v.as_object()) {
        for (name, prop_value) in props {
            let kind = prop_value
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("string")
                .to_string();

            let desc = prop_value
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let default = prop_value.get("default").cloned();

            let enum_values = prop_value
                .get("enum")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                });

            let items = prop_value
                .get("items")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<HashMap<String, serde_json::Value>>()
                });

            properties.insert(
                name.clone(),
                fuyao_api::ToolParameterProperty {
                    kind,
                    description: desc,
                    default,
                    enum_values,
                    items,
                },
            );
        }
    }

    if let Some(req) = normalized.get("required").and_then(|v| v.as_array()) {
        for item in req {
            if let Some(s) = item.as_str() {
                required.push(s.to_string());
            }
        }
    }

    fuyao_api::ToolDefinition {
        name: prefixed_name.to_string(),
        description: description.to_string(),
        parameters: fuyao_api::ToolParameters {
            kind: "object".to_string(),
            properties,
            required,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_manager_new_creates_empty_state() {
        let configs = HashMap::new();
        let manager = MCPManager::new(configs);
        assert!(manager.get_configured_servers().is_empty());
    }

    #[test]
    fn mcp_manager_from_config_empty() {
        // 未 set_config 时 get_config() 返回 default（mcp_servers 空）
        let manager = MCPManager::from_config();
        assert!(manager.get_configured_servers().is_empty());
    }

    #[test]
    fn mcp_manager_new_with_servers() {
        let mut configs = HashMap::new();
        configs.insert(
            "test-server".to_string(),
            MCPServerConfig {
                command: Some("npx".to_string()),
                args: Some(vec!["-y".to_string(), "test-mcp".to_string()]),
                ..Default::default()
            },
        );
        let manager = MCPManager::new(configs);
        assert!(
            manager
                .get_configured_servers()
                .contains(&"test-server".to_string())
        );
    }

    #[tokio::test]
    async fn start_all_with_no_servers() {
        let manager = MCPManager::new(HashMap::new());
        let (success, failures, _) = manager.start_all().await;
        assert_eq!(success, 0);
        assert_eq!(failures, 0);
    }

    #[tokio::test]
    async fn stop_all_cleans_up() {
        let manager = MCPManager::new(HashMap::new());
        manager.stop_all().await;
        let tools = manager.get_registered_tools().await;
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn get_server_status_empty() {
        let manager = MCPManager::new(HashMap::new());
        let status = manager.get_server_status().await;
        assert!(status.is_empty());
    }

    #[test]
    fn register_server_tools_basic() {
        let config = MCPServerConfig::default();
        let tools = vec![McpToolInfo {
            name: "search".to_string(),
            description: Some("搜索".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "关键词"}
                },
                "required": ["query"]
            }),
        }];

        let registered = register_server_tools("my-server", &config, &tools);
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].prefixed_name, "mcp_my_server_search");
        assert_eq!(registered[0].original_name, "search");
        assert_eq!(registered[0].server_name, "my-server");
    }

    #[test]
    fn register_server_tools_with_filter() {
        let mut config = MCPServerConfig::default();
        config.tools.insert("search".to_string(), false);
        config.tools.insert("read".to_string(), true);

        let tools = vec![
            McpToolInfo {
                name: "search".to_string(),
                description: Some("搜索".to_string()),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            McpToolInfo {
                name: "read".to_string(),
                description: Some("读取".to_string()),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
        ];

        let registered = register_server_tools("my-server", &config, &tools);
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].original_name, "read");
    }

    #[test]
    fn build_tool_schema_from_info_normalizes() {
        let schema = build_tool_schema_from_info(
            "mcp_server_tool",
            "描述",
            &serde_json::json!({
                "type": "object",
                "properties": {
                    "input": {"type": "string"}
                }
            }),
        );
        assert_eq!(schema.name, "mcp_server_tool");
        assert!(schema.parameters.properties.contains_key("input"));
    }

    #[test]
    fn mcp_manager_error_messages() {
        let err = MCPManagerError::ServerNotFound("test".to_string());
        assert!(err.to_string().contains("test"));

        let err = MCPManagerError::ToolNotFound {
            server: "s".to_string(),
            tool: "t".to_string(),
        };
        assert!(err.to_string().contains("t"));
    }
}
