//! Fuyao 全局配置
//!
//! 支持三层配置合并加载：全局 → Agent 目录 → 工作目录。
//!
//! 合并规则：
//! - providers：以 provider_id 为单位，同名 provider 完全覆盖
//! - model/tools/mcp_servers：高优先级覆盖低优先级

use crate::error::ConfigError;
use crate::providers::load_providers;
use fuyao_api::{AgentPaths, MCPServerConfig, Provider};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Fuyao 全局配置
///
/// 对应 ~/.fuyao/fuyao.toml 配置文件结构。
/// 模型嵌套在 Provider 下面。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FuyaoConfig {
    /// 默认模型（格式：provider_id/model_id）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// 工具开关，key=工具名，value=是否启用。未列出的工具默认启用
    #[serde(default)]
    pub tools: HashMap<String, bool>,

    /// Provider 配置字典，key 为 Provider ID
    #[serde(default)]
    pub providers: HashMap<String, Provider>,

    /// MCP Server 配置字典，key 为 server 名称
    #[serde(default)]
    pub mcp_servers: HashMap<String, MCPServerConfig>,
}

fn merge_config(base: &mut FuyaoConfig, override_data: &toml::Table) {
    if let Some(model) = override_data.get("model").and_then(|v| v.as_str()) {
        base.model = Some(model.to_string());
    }

    if let Some(tools_table) = override_data.get("tools").and_then(|v| v.as_table()) {
        for (tool_name, enabled) in tools_table {
            if let Some(enabled_val) = enabled.as_bool() {
                base.tools.insert(tool_name.clone(), enabled_val);
            }
        }
    }

    if let Some(providers_val) = override_data.get("providers") {
        let override_providers = load_providers(providers_val);
        for (provider_id, provider) in override_providers {
            base.providers.insert(provider_id, provider);
        }
    }

    if let Some(mcp_table) = override_data.get("mcp_servers").and_then(|v| v.as_table()) {
        for (server_name, server_val) in mcp_table {
            if let Ok(config) = MCPServerConfig::deserialize(server_val.clone()) {
                base.mcp_servers.insert(server_name.clone(), config);
            }
        }
    }
}

/// 三层配置合并加载
///
/// 从低优先级到高优先级逐层合并：全局 → Agent 目录 → 工作目录。
/// 每层配置文件不存在时跳过，全部不存在则返回 None。
///
/// # Arguments
/// * `global_path` - 全局配置文件路径（~/.fuyao/fuyao.toml）
/// * `agent_path` - Agent 目录配置文件路径
/// * `workspace_path` - 工作目录配置文件路径
///
/// # Returns
/// 合并后的 FuyaoConfig，全部层都不存在则返回 None
pub fn load_merged_config(
    global_path: Option<&Path>,
    agent_path: Option<&Path>,
    workspace_path: Option<&Path>,
) -> Result<Option<FuyaoConfig>, ConfigError> {
    let mut merged = FuyaoConfig::default();
    let mut has_any_config = false;

    let paths: Vec<Option<&Path>> = vec![global_path, agent_path, workspace_path];

    for path in paths.into_iter().flatten() {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            let table: toml::Table = toml::from_str(&content)?;
            if !table.is_empty() {
                has_any_config = true;
                merge_config(&mut merged, &table);
            }
        }
    }

    if has_any_config {
        Ok(Some(merged))
    } else {
        Ok(None)
    }
}

/// 从 AgentPaths 加载配置
///
/// 使用 `AgentPaths::config_paths()` 获取三层路径，自动合并加载。
/// 优先级：工作目录 → Agent 目录 → 全局。
///
/// # Arguments
/// * `agent_paths` - Agent 三层路径配置
///
/// # Returns
/// 合并后的 FuyaoConfig，全部层都不存在则返回 None
pub fn load_config(agent_paths: &AgentPaths) -> Result<Option<FuyaoConfig>, ConfigError> {
    let paths = agent_paths.config_paths();
    let all_paths = paths.all();
    load_merged_config(
        all_paths.get(2).copied(),  // global_ (lowest priority)
        all_paths.get(1).copied(),  // agent
        all_paths.first().copied(), // workspace (highest priority)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_empty() {
        let config = FuyaoConfig::default();
        assert!(config.model.is_none());
        assert!(config.tools.is_empty());
        assert!(config.providers.is_empty());
        assert!(config.mcp_servers.is_empty());
    }

    #[test]
    fn load_empty_paths_returns_none() {
        let result = load_merged_config(None, None, None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn merge_model_field() {
        let mut base = FuyaoConfig::default();
        let table: toml::Table = toml::from_str("model = \"aliyun/qwen3.6-plus\"").unwrap();
        merge_config(&mut base, &table);

        assert_eq!(base.model, Some("aliyun/qwen3.6-plus".to_string()));
    }

    #[test]
    fn merge_tools_field() {
        let mut base = FuyaoConfig {
            tools: HashMap::from([("tool_a".to_string(), true)]),
            ..Default::default()
        };
        let table: toml::Table = toml::from_str("[tools]\ntool_b = false\ntool_a = false").unwrap();
        merge_config(&mut base, &table);

        assert_eq!(base.tools["tool_a"], false);
        assert_eq!(base.tools["tool_b"], false);
    }

    #[test]
    fn merge_mcp_servers() {
        let mut base = FuyaoConfig::default();
        let table: toml::Table = toml::from_str(
            r#"
            [mcp_servers.test]
            command = "node"
            args = ["server.js"]
            "#,
        )
        .unwrap();
        merge_config(&mut base, &table);

        assert!(base.mcp_servers.contains_key("test"));
        assert_eq!(base.mcp_servers["test"].command, Some("node".to_string()));
    }

    #[test]
    fn load_config_with_default_agent_paths() {
        let paths = AgentPaths::default();
        // 无配置文件时应返回 None（全局 ~/.fuyao/fuyao.toml 可能不存在）
        let result = load_config(&paths);
        assert!(result.is_ok());
    }
}
