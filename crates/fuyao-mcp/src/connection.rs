//! MCP 连接管理
//!
//! 管理单个 MCP Server 的完整生命周期：连接、发现、断开。
//! 每个 MCPConnection 实例对应一个 MCP Server。
//!
//! 长连接 Task 模式：在 tokio 事件循环上作为 Task 运行，
//! 支持自动重连、Auth/Session 恢复、RPC 并发锁。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, ListToolsResult, Tool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::ConfigureCommandExt;
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify, watch};

use fuyao_api::MCPServerConfig;

use crate::security::build_safe_env;

/// MCP 连接错误
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("MCP server '{server}' 未连接")]
    NotConnected { server: String },

    #[error("MCP server '{server}' 初始连接失败: {reason}")]
    InitialConnectFailed { server: String, reason: String },

    #[error("MCP server '{server}' 连接超时")]
    ConnectTimeout { server: String },

    #[error("stdio 传输需要 'command' 配置")]
    MissingCommand,

    #[error("HTTP 传输需要 'url' 配置")]
    MissingUrl,

    #[error("MCP 调用失败: {0}")]
    CallFailed(String),
}

/// MCP 工具信息（从 Server 发现的工具）
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

/// 单个 MCP Server 的连接管理
///
/// 管理连接生命周期，提供 call_tool / list_tools 接口。
/// 内部运行长连接 Task，支持自动重连。
pub struct MCPConnection {
    /// Server 名称
    pub server_name: String,
    /// Server 配置
    pub config: MCPServerConfig,
    /// 已发现的工具列表
    pub tools: Vec<McpToolInfo>,
    /// 是否已连接
    connected: Arc<std::sync::atomic::AtomicBool>,
    /// RPC 并发锁
    rpc_lock: Arc<Mutex<()>>,
    /// 生命周期通知（shutdown / reconnect）
    lifecycle_notify: Arc<Notify>,
    /// shutdown 信号
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    /// 连接 Task 句柄
    task_handle: Option<tokio::task::JoinHandle<()>>,
    /// MCP 客户端（RunningService，保持连接存活）
    client: Arc<Mutex<Option<RunningService<RoleClient, ()>>>>,
}

impl MCPConnection {
    /// 创建新的 MCP 连接实例
    pub fn new(server_name: String, config: MCPServerConfig) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            server_name,
            config,
            tools: Vec::new(),
            connected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            rpc_lock: Arc::new(Mutex::new(())),
            lifecycle_notify: Arc::new(Notify::new()),
            shutdown_tx,
            shutdown_rx,
            task_handle: None,
            client: Arc::new(Mutex::new(None)),
        }
    }

    /// 是否已连接
    pub fn is_connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 启动连接（非阻塞，后台 Task 运行）
    pub async fn start(&mut self) -> Result<(), ConnectionError> {
        let server_name = self.server_name.clone();
        let config = self.config.clone();
        let connected = self.connected.clone();
        let rpc_lock = self.rpc_lock.clone();
        let lifecycle_notify = self.lifecycle_notify.clone();
        let mut shutdown_rx = self.shutdown_rx.clone();
        let client = self.client.clone();

        let error_result: Arc<Mutex<Option<ConnectionError>>> = Arc::new(Mutex::new(None));
        let error_clone = error_result.clone();

        let handle = tokio::spawn(async move {
            let mcp_cfg = fuyao_api::get_config();
            let mcp = &mcp_cfg.mcp;
            let mut retries: u32 = 0;
            let mut initial_retries: u32 = 0;
            let mut backoff: u64 = 1;

            loop {
                match run_transport(&server_name, &config, &connected, &client, &rpc_lock).await {
                    Ok(_discovered_tools) => {
                        tokio::select! {
                            _ = shutdown_rx.changed() => {
                                if *shutdown_rx.borrow() {
                                    break;
                                }
                            }
                            _ = lifecycle_notify.notified() => {
                                connected.store(false, std::sync::atomic::Ordering::Relaxed);
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        connected.store(false, std::sync::atomic::Ordering::Relaxed);

                        if initial_retries < mcp.max_initial_connect_retries {
                            initial_retries += 1;
                            tokio::time::sleep(Duration::from_secs(backoff)).await;
                            backoff = (backoff * 2).min(mcp.max_backoff_secs);

                            if *shutdown_rx.borrow() {
                                let mut err = error_clone.lock().await;
                                *err = Some(e);
                                break;
                            }
                            continue;
                        }

                        if *shutdown_rx.borrow() {
                            break;
                        }

                        retries += 1;
                        if retries > mcp.max_reconnect_retries {
                            break;
                        }

                        tokio::time::sleep(Duration::from_secs(backoff)).await;
                        backoff = (backoff * 2).min(mcp.max_backoff_secs);
                    }
                }
            }
        });

        self.task_handle = Some(handle);

        // 尝试一次性初始连接
        let init_result = self.try_connect_once().await;
        match init_result {
            Ok(tools) => {
                self.tools = tools;
                self.connected
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// 尝试一次性连接
    async fn try_connect_once(&mut self) -> Result<Vec<McpToolInfo>, ConnectionError> {
        let server_name = self.server_name.clone();
        let config = self.config.clone();

        if config.is_http() {
            self.connect_http(&server_name, &config).await
        } else {
            self.connect_stdio(&server_name, &config).await
        }
    }

    /// stdio 传输连接
    async fn connect_stdio(
        &mut self,
        server_name: &str,
        config: &MCPServerConfig,
    ) -> Result<Vec<McpToolInfo>, ConnectionError> {
        let command = config
            .command
            .as_ref()
            .ok_or(ConnectionError::MissingCommand)?;

        let mut cmd = Command::new(command);
        if let Some(args) = &config.args {
            cmd.args(args);
        }

        // 构建安全环境变量
        let safe_env = build_safe_env(config.env.as_ref());
        for (k, v) in &safe_env {
            cmd.env(k, v);
        }

        let transport = TokioChildProcess::new(cmd.configure(|_| {})).map_err(|e| {
            ConnectionError::InitialConnectFailed {
                server: server_name.to_string(),
                reason: e.to_string(),
            }
        })?;

        let client =
            ().serve(transport)
                .await
                .map_err(|e| ConnectionError::InitialConnectFailed {
                    server: server_name.to_string(),
                    reason: e.to_string(),
                })?;

        // 发现工具
        let tools =
            client
                .list_all_tools()
                .await
                .map_err(|e| ConnectionError::InitialConnectFailed {
                    server: server_name.to_string(),
                    reason: e.to_string(),
                })?;

        let tool_infos = convert_tools(&tools);

        // 保存 RunningService（保持连接存活）
        {
            let mut c = self.client.lock().await;
            *c = Some(client);
        }

        Ok(tool_infos)
    }

    /// HTTP 传输连接
    async fn connect_http(
        &mut self,
        server_name: &str,
        config: &MCPServerConfig,
    ) -> Result<Vec<McpToolInfo>, ConnectionError> {
        let url = config.url.as_ref().ok_or(ConnectionError::MissingUrl)?;

        let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
            .reinit_on_expired_session(true);

        // 注入自定义 headers（如 Authorization、mcp-protocol-version 等）
        if let Some(headers) = &config.headers {
            let mut custom_headers = HashMap::new();
            for (name, value) in headers {
                if let (Ok(hn), Ok(hv)) = (
                    http::HeaderName::from_bytes(name.as_bytes()),
                    http::HeaderValue::from_str(value),
                ) {
                    custom_headers.insert(hn, hv);
                }
            }
            if !custom_headers.is_empty() {
                transport_config = transport_config.custom_headers(custom_headers);
            }
        }

        let transport = StreamableHttpClientTransport::from_config(transport_config);

        let client =
            ().serve(transport)
                .await
                .map_err(|e| ConnectionError::InitialConnectFailed {
                    server: server_name.to_string(),
                    reason: e.to_string(),
                })?;

        // 发现工具
        let tools =
            client
                .list_all_tools()
                .await
                .map_err(|e| ConnectionError::InitialConnectFailed {
                    server: server_name.to_string(),
                    reason: e.to_string(),
                })?;

        let tool_infos = convert_tools(&tools);

        // 保存 RunningService（保持连接存活）
        {
            let mut c = self.client.lock().await;
            *c = Some(client);
        }

        Ok(tool_infos)
    }

    /// 调用 MCP 工具
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, ConnectionError> {
        if !self.is_connected() {
            return Err(ConnectionError::NotConnected {
                server: self.server_name.clone(),
            });
        }

        let _lock = self.rpc_lock.lock().await;

        let guard = self.client.lock().await;
        let svc = guard.as_ref().ok_or(ConnectionError::NotConnected {
            server: self.server_name.clone(),
        })?;

        let args: Option<serde_json::Map<String, serde_json::Value>> =
            arguments.as_object().cloned();

        svc.call_tool(
            CallToolRequestParams::new(tool_name.to_string())
                .with_arguments(args.unwrap_or_default()),
        )
        .await
        .map_err(|e| ConnectionError::CallFailed(e.to_string()))
    }

    /// 列出工具
    pub async fn list_tools(&self) -> Result<ListToolsResult, ConnectionError> {
        if !self.is_connected() {
            return Err(ConnectionError::NotConnected {
                server: self.server_name.clone(),
            });
        }

        let _lock = self.rpc_lock.lock().await;

        let guard = self.client.lock().await;
        let svc = guard.as_ref().ok_or(ConnectionError::NotConnected {
            server: self.server_name.clone(),
        })?;

        svc.list_tools(None)
            .await
            .map_err(|e| ConnectionError::CallFailed(e.to_string()))
    }

    /// 触发重连信号
    pub fn notify_reconnect(&self) {
        self.lifecycle_notify.notify_one();
    }

    /// 断开连接
    pub async fn disconnect(&mut self) {
        // 接收端可能已 drop（Task 已结束），忽略发送失败
        let _ = self.shutdown_tx.send(true);
        self.lifecycle_notify.notify_one();

        if let Some(handle) = self.task_handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
        }

        self.connected
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.tools.clear();

        // drop RunningService 关闭连接
        {
            let mut c = self.client.lock().await;
            *c = None;
        }
    }
}

/// 运行传输层连接（长连接 Task 内部使用）
async fn run_transport(
    server_name: &str,
    config: &MCPServerConfig,
    connected: &Arc<std::sync::atomic::AtomicBool>,
    client: &Arc<Mutex<Option<RunningService<RoleClient, ()>>>>,
    _rpc_lock: &Arc<Mutex<()>>,
) -> Result<Vec<McpToolInfo>, ConnectionError> {
    if config.is_http() {
        let url = config.url.as_ref().ok_or(ConnectionError::MissingUrl)?;

        let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
            .reinit_on_expired_session(true);

        if let Some(headers) = &config.headers {
            let mut custom_headers = HashMap::new();
            for (name, value) in headers {
                if let (Ok(hn), Ok(hv)) = (
                    http::HeaderName::from_bytes(name.as_bytes()),
                    http::HeaderValue::from_str(value),
                ) {
                    custom_headers.insert(hn, hv);
                }
            }
            if !custom_headers.is_empty() {
                transport_config = transport_config.custom_headers(custom_headers);
            }
        }

        let transport = StreamableHttpClientTransport::from_config(transport_config);

        let svc =
            ().serve(transport)
                .await
                .map_err(|e| ConnectionError::InitialConnectFailed {
                    server: server_name.to_string(),
                    reason: e.to_string(),
                })?;

        let tools =
            svc.list_all_tools()
                .await
                .map_err(|e| ConnectionError::InitialConnectFailed {
                    server: server_name.to_string(),
                    reason: e.to_string(),
                })?;

        let tool_infos = convert_tools(&tools);
        connected.store(true, std::sync::atomic::Ordering::Relaxed);

        // 保存 RunningService（保持连接存活）
        {
            let mut c = client.lock().await;
            *c = Some(svc);
        }

        Ok(tool_infos)
    } else {
        let command = config
            .command
            .as_ref()
            .ok_or(ConnectionError::MissingCommand)?;

        let mut cmd = Command::new(command);
        if let Some(args) = &config.args {
            cmd.args(args);
        }

        let safe_env = build_safe_env(config.env.as_ref());
        for (k, v) in &safe_env {
            cmd.env(k, v);
        }

        let transport = TokioChildProcess::new(cmd.configure(|_| {})).map_err(|e| {
            ConnectionError::InitialConnectFailed {
                server: server_name.to_string(),
                reason: e.to_string(),
            }
        })?;

        let svc =
            ().serve(transport)
                .await
                .map_err(|e| ConnectionError::InitialConnectFailed {
                    server: server_name.to_string(),
                    reason: e.to_string(),
                })?;

        let tools =
            svc.list_all_tools()
                .await
                .map_err(|e| ConnectionError::InitialConnectFailed {
                    server: server_name.to_string(),
                    reason: e.to_string(),
                })?;

        let tool_infos = convert_tools(&tools);
        connected.store(true, std::sync::atomic::Ordering::Relaxed);

        // 保存 RunningService（保持连接存活）
        {
            let mut c = client.lock().await;
            *c = Some(svc);
        }

        Ok(tool_infos)
    }
}

/// 从 rmcp Tool 列表转换为 McpToolInfo 列表
fn convert_tools(tools: &[Tool]) -> Vec<McpToolInfo> {
    tools
        .iter()
        .map(|tool| McpToolInfo {
            name: tool.name.to_string(),
            description: tool.description.as_ref().map(|d| d.to_string()),
            input_schema: tool.schema_as_json_value(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_new_initializes_state() {
        let config = MCPServerConfig {
            command: Some("npx".to_string()),
            ..Default::default()
        };
        let conn = MCPConnection::new("test-server".to_string(), config);
        assert_eq!(conn.server_name, "test-server");
        assert!(!conn.is_connected());
        assert!(conn.tools.is_empty());
    }

    #[test]
    fn connection_error_messages() {
        let err = ConnectionError::NotConnected {
            server: "my-server".to_string(),
        };
        assert!(err.to_string().contains("my-server"));
        assert!(err.to_string().contains("未连接"));

        let err = ConnectionError::MissingCommand;
        assert!(err.to_string().contains("command"));
    }

    #[test]
    fn mcp_tool_info_fields() {
        let info = McpToolInfo {
            name: "search".to_string(),
            description: Some("搜索文件".to_string()),
            input_schema: serde_json::json!({"type": "object"}),
        };
        assert_eq!(info.name, "search");
        assert_eq!(info.description, Some("搜索文件".to_string()));
    }

    #[test]
    fn convert_tools_handles_empty() {
        let tools: Vec<Tool> = vec![];
        let result = convert_tools(&tools);
        assert!(result.is_empty());
    }
}
