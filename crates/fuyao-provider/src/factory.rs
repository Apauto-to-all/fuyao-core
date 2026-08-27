//! 供应商实例构造工厂（API 协议分派单点）
//!
//! [`build_provider`] 是全部构造路径（启动期批量注册、运行时 reload）的唯一
//! 分派入口：按 Provider 配置的 `api_protocol` 选定 wire 实现。新增协议实现时
//! 在此 match 补一支即可，调用方接线不动。
//!
//! 构造失败的容错粒度：单供应商失败由调用方 WARN + 跳过（不拖垮其余
//! 供应商），错误消息明确到可以照着改（见 ADR-0002）。

use std::sync::Arc;

use fuyao_api::{AgentPaths, ApiProtocol, Provider as ProviderConfig};
use thiserror::Error;

use crate::anthropic::AnthropicProvider;
use crate::openai::OpenAIProvider;
use crate::provider::Provider as ProviderTrait;

/// 供应商实例构造错误
#[derive(Debug, Error)]
pub enum BuildProviderError {
    /// OpenAI 兼容实例构造失败（API Key 未解析到 / HTTP 客户端构建失败，细节已记 WARN 日志）
    #[error("openai-completions 实例构造失败（通常是 API Key 未配置或 HTTP 客户端构建失败）")]
    OpenaiConstruction,
    /// Anthropic 实例构造失败（API Key 未解析到 / HTTP 客户端构建失败，细节已记 WARN 日志）
    #[error("anthropic-messages 实例构造失败（通常是 API Key 未配置或 HTTP 客户端构建失败）")]
    AnthropicConstruction,
}

/// 按 Provider 配置的 `api_protocol` 构造供应商实例
///
/// `provider` 为进程级缓存中的配置（含协议声明），`provider_id` 用于 API Key /
/// base_url 解析链。两支分派穷尽枚举：`openai-completions` /
/// `anthropic-messages` 各走自身 wire 实现，构造失败落各自错误分支。
pub fn build_provider(
    provider_id: &str,
    provider: &ProviderConfig,
    agent_paths: &AgentPaths,
) -> Result<Arc<dyn ProviderTrait>, BuildProviderError> {
    match provider.api_protocol {
        ApiProtocol::OpenaiCompletions => OpenAIProvider::new(provider_id, agent_paths)
            .map(|instance| Arc::new(instance) as Arc<dyn ProviderTrait>)
            .ok_or(BuildProviderError::OpenaiConstruction),
        ApiProtocol::AnthropicMessages => AnthropicProvider::new(provider_id, agent_paths)
            .map(|instance| Arc::new(instance) as Arc<dyn ProviderTrait>)
            .ok_or(BuildProviderError::AnthropicConstruction),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{clear_cache, register_provider};

    /// 构造测试用 Provider 配置（协议可注入，其余字段取最小值）
    fn test_provider(api_protocol: ApiProtocol, api_key: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            name: "test".to_string(),
            api_protocol,
            models: std::collections::HashMap::new(),
            options: fuyao_api::ProviderOptions {
                api_key: api_key.map(str::to_string),
                base_url: None,
            },
            api_key_env_vars: Vec::new(),
        }
    }

    /// 唯一化 AgentPaths（隔离进程级注册缓存）
    fn unique_paths(test_name: &str) -> AgentPaths {
        AgentPaths {
            agent_id: Some(format!("global/{test_name}")),
            workspace: None,
            ..Default::default()
        }
    }

    /// openai-completions 分派成功：key 齐备时构造出实例
    #[test]
    fn openai_completions_builds_instance() {
        let paths = unique_paths("factory_openai_ok");
        let key = crate::registry::agent_paths_cache_key(&paths);
        let provider = test_provider(ApiProtocol::OpenaiCompletions, Some("sk-test"));
        register_provider("vendor", provider, &key);

        let instance = build_provider(
            "vendor",
            &test_provider(ApiProtocol::OpenaiCompletions, Some("sk-test")),
            &paths,
        );
        assert!(instance.is_ok(), "key 齐备的 openai-completions 应构造成功");

        clear_cache(&paths);
    }

    /// openai-completions 构造失败（API Key 未解析到）：错误落在构造失败分支
    #[test]
    fn openai_completions_without_key_reports_construction_error() {
        let paths = unique_paths("factory_openai_nokey");
        let provider = test_provider(ApiProtocol::OpenaiCompletions, None);
        // 未注册任何缓存配置：Key 解析必然落空

        let err = build_provider("ghost", &provider, &paths)
            .err()
            .expect("key 缺失应报构造失败");
        assert!(
            matches!(err, BuildProviderError::OpenaiConstruction),
            "应落在 OpenAI 构造失败分支"
        );

        clear_cache(&paths);
    }

    /// anthropic-messages 分派成功：key 齐备时构造出实例
    #[test]
    fn anthropic_messages_builds_instance() {
        let paths = unique_paths("factory_anthropic_ok");
        let key = crate::registry::agent_paths_cache_key(&paths);
        let provider = test_provider(ApiProtocol::AnthropicMessages, Some("sk-test"));
        register_provider("vendor", provider, &key);

        let instance = build_provider(
            "vendor",
            &test_provider(ApiProtocol::AnthropicMessages, Some("sk-test")),
            &paths,
        );
        assert!(instance.is_ok(), "key 齐备的 anthropic-messages 应构造成功");

        clear_cache(&paths);
    }

    /// anthropic-messages 构造失败（API Key 未解析到）：错误落在构造失败分支
    #[test]
    fn anthropic_messages_without_key_reports_construction_error() {
        let paths = unique_paths("factory_anthropic_nokey");
        let provider = test_provider(ApiProtocol::AnthropicMessages, None);
        // 未注册任何缓存配置：Key 解析必然落空

        let err = build_provider("ghost", &provider, &paths)
            .err()
            .expect("key 缺失应报构造失败");
        assert!(
            matches!(err, BuildProviderError::AnthropicConstruction),
            "应落在 Anthropic 构造失败分支"
        );

        clear_cache(&paths);
    }
}
