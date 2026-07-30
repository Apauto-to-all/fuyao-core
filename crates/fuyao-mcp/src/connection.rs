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

    /// 启动连接（非阻塞，后台 Task 保活）
    ///
    /// 单连接设计：主流程负责建立一次连接（serve + list_tools），后台 Task 只负责
    /// 保活监听（shutdown / reconnect 信号）与重连，不再重复 `serve()`。
    /// 这样避免「后台 Task 与主流程各 serve 一次、产生两个 RunningService 争抢写
    /// self.client」的双连接竞争——HTTP 场景下两个 serve 共享 session，一个被取消
    /// 会拖垮另一个，最终残留的 cancelled 连接会让后续 call_tool 永久挂起。
    pub async fn start(&mut self) -> Result<(), ConnectionError> {
        // 1. 主流程建立一次性初始连接（serve + list_tools + 写入 self.client）
        let tools = self.try_connect_once().await?;
        self.tools = tools;
        self.connected
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // 2. 启动后台保活 Task：只监听信号做重连，不重复 serve
        let server_name = self.server_name.clone();
        let config = self.config.clone();
        let connected = self.connected.clone();
        let rpc_lock = self.rpc_lock.clone();
        let lifecycle_notify = self.lifecycle_notify.clone();
        let mut shutdown_rx = self.shutdown_rx.clone();
        let client = self.client.clone();

        let handle = tokio::spawn(async move {
            let mcp_cfg = fuyao_api::get_config();
            let mcp = &mcp_cfg.mcp;
            let mut retries: u32 = 0;
            let mut backoff: u64 = 1;

            loop {
                // 已建立连接：只等信号，不重建连接
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = lifecycle_notify.notified() => {
                        // 重连信号：标记断开后进入重连循环
                        connected.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                }

                // 重连循环：仅在未连接时尝试重建（连接已由主流程建立，此处处理断线恢复）
                if connected.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }

                if *shutdown_rx.borrow() {
                    break;
                }

                match run_transport(&server_name, &config, &connected, &client, &rpc_lock).await {
                    Ok(_) => {
                        // 重连成功：重置退避，回到信号等待
                        retries = 0;
                        backoff = 1;
                    }
                    Err(e) => {
                        if *shutdown_rx.borrow() {
                            tracing::warn!(
                                server = %server_name,
                                cause = %e,
                                "MCP server 重连因 shutdown 中止"
                            );
                            break;
                        }

                        retries += 1;
                        if retries > mcp.max_reconnect_retries {
                            tracing::error!(
                                server = %server_name,
                                attempts = retries,
                                cause = %e,
                                "MCP server 重连耗尽，已放弃"
                            );
                            break;
                        }

                        tracing::warn!(
                            server = %server_name,
                            attempt = retries,
                            backoff_secs = backoff,
                            cause = %e,
                            "MCP server 重连中"
                        );
                        tokio::time::sleep(Duration::from_secs(backoff)).await;
                        backoff = (backoff * 2).min(mcp.max_backoff_secs);
                    }
                }
            }
        });

        self.task_handle = Some(handle);
        Ok(())
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

        let mut cmd = Command::new(resolve_command(command));
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

    /// 断开连接（优雅关闭）
    ///
    /// 先通知后台 Task 退出，再显式调用 rmcp 的 `close_with_timeout` 做有界优雅关闭：
    /// rmcp 会先关 transport（给 server 自行退出的机会），等后台清理完成，并确保
    /// 子进程被回收；总超时 10 秒。单个 server 关闭失败仅记 WARN，不阻断其他 server。
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

        // 显式优雅关闭 RunningService：rmcp 先关 transport、等 server 退出、超时 kill 子进程
        let svc = { self.client.lock().await.take() };
        if let Some(mut svc) = svc
            && let Err(e) = svc.close_with_timeout(Duration::from_secs(10)).await
        {
            tracing::warn!(
                server = %self.server_name,
                cause = %e,
                "MCP 连接关闭时后台任务 join 失败（已尽力清理）"
            );
        }
    }
}

/// 解析 stdio 命令为 Windows 兼容的可执行程序名
///
/// Windows 上 npm 安装的命令（npx / node / pnpm 等）是 `.cmd` / `.bat` 批处理脚本，
/// 而 `tokio::process::Command::new("npx")` 不带后缀时会 `program not found`
/// （Windows 的 CreateProcess 不自动补 `.cmd`/`.bat` 后缀）。
///
/// 本函数在 Windows 上：若命令名不含路径分隔符、也无已知可执行后缀，则按 PATHEXT
/// 在 PATH 中查找其实际文件名（如 `npx` → `npx.cmd`），让 Command 能命中。
/// 非 Windows 平台原样返回（系统 shell 会自行解析）。
fn resolve_command(command: &str) -> String {
    // 非 Windows 直接返回，交由系统 PATH 解析
    if !cfg!(windows) {
        return command.to_string();
    }

    // 已含路径分隔符或可执行后缀：视为用户已写全，原样返回
    let has_separator = command.contains('/') || command.contains('\\');
    let has_exec_ext = ["exe", "cmd", "bat", "com", "ps1"]
        .iter()
        .any(|ext| command.to_lowercase().ends_with(&format!(".{ext}")));
    if has_separator || has_exec_ext {
        return command.to_string();
    }

    // 在 PATH 中查找实际可执行文件名（npx → npx.cmd）
    // PATHEXT 含系统支持的后缀列表（如 .COM;.EXE;.BAT;.CMD;...）
    let path_exts = std::env::var("PATHEXT").unwrap_or_default();
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        for ext in path_exts.split(';') {
            let candidate = dir.join(format!("{command}{ext}"));
            if candidate.is_file() {
                // 返回找到的实际文件名（含后缀），不含目录——让系统按 PATH 命中
                return format!("{command}{ext}");
            }
        }
    }

    // PATH 中未找到，原样返回（让系统给出标准的 program not found 错误）
    command.to_string()
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

        let mut cmd = Command::new(resolve_command(command));
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

    #[test]
    fn resolve_command_keeps_explicit_extension() {
        // 已带可执行后缀：原样返回，不二次处理
        assert_eq!(resolve_command("npx.cmd"), "npx.cmd");
        assert_eq!(resolve_command("node.exe"), "node.exe");
    }

    #[test]
    fn resolve_command_keeps_path_separated_command() {
        // 含路径分隔符：视为用户写全，原样返回
        assert_eq!(resolve_command("./bin/run"), "./bin/run");
        assert_eq!(resolve_command("C:\\tools\\srv"), "C:\\tools\\srv");
    }
}
