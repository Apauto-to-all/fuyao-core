//! Provider 工厂函数与模型 ID 解析
//!
//! 提供：
//! - parse_model_id：解析 "provider_id/model_id" 格式
//! - create_provider / create_provider_with_model：根据配置创建 OpenAIProvider

use crate::openai::OpenAIProvider;
use fuyao_api::AgentPaths;

/// 模型 ID 解析错误
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// 模型 ID 格式错误
    #[error("模型 ID 格式错误，应为 provider_id/model_id: {0}")]
    InvalidModelId(String),
}

/// 解析模型 ID
///
/// 将 "provider_id/model_id" 格式拆分为 (provider_id, model_id) 元组。
/// provider_id 会转为小写，model_id 保持原样。
///
/// # Arguments
/// * `model_id` - 模型 ID（如 "aliyun/qwen3.6-plus"）
///
/// # Returns
/// (provider_id, model_id) 元组
pub fn parse_model_id(model_id: &str) -> Result<(String, String), ClientError> {
    let stripped = model_id.trim();
    if stripped.contains('/') {
        let parts: Vec<&str> = stripped.splitn(2, '/').collect();
        let provider_id = parts[0].trim().to_lowercase();
        let model_id = parts[1].trim();
        Ok((provider_id, model_id.to_string()))
    } else {
        Err(ClientError::InvalidModelId(model_id.to_string()))
    }
}

/// 创建 OpenAI 兼容 Provider
///
/// 根据 Provider 配置和 agent_paths 动态解析 API Key 和 base_url，
/// 构建 reqwest Client 并封装为 OpenAIProvider。
///
/// # Arguments
/// * `provider_id` - Provider ID
/// * `agent_paths` - Agent 三层目录的身份证明
///
/// # Returns
/// OpenAIProvider 实例，如果配置不完整返回 None
pub fn create_provider(provider_id: &str, agent_paths: &AgentPaths) -> Option<OpenAIProvider> {
    OpenAIProvider::new(provider_id, agent_paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{clear_cache, register_provider};
    use fuyao_api::{Provider, ProviderOptions};

    fn create_test_provider_with_api_key(
        name: &str,
        api_key: Option<String>,
        base_url: Option<String>,
    ) -> Provider {
        Provider {
            name: name.to_string(),
            models: std::collections::HashMap::new(),
            options: ProviderOptions { api_key, base_url },
            api_key_env_vars: Vec::new(),
        }
    }

    fn unique_paths(test_name: &str) -> AgentPaths {
        AgentPaths {
            agent_id: Some(format!("test/{test_name}")),
            workspace: None,
            ..Default::default()
        }
    }

    #[test]
    fn parse_model_id_valid() {
        let (provider, model) = parse_model_id("aliyun/qwen3.6-plus").unwrap();
        assert_eq!(provider, "aliyun");
        assert_eq!(model, "qwen3.6-plus");
    }

    #[test]
    fn parse_model_id_with_spaces() {
        let (provider, model) = parse_model_id(" Aliyun / Qwen3 ").unwrap();
        assert_eq!(provider, "aliyun");
        assert_eq!(model, "Qwen3");
    }

    #[test]
    fn parse_model_id_invalid_no_slash() {
        let result = parse_model_id("qwen3");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ClientError::InvalidModelId(_)
        ));
    }

    #[test]
    fn create_provider_with_provider_options() {
        let paths = unique_paths("with_opts");
        let key = crate::registry::agent_paths_cache_key(&paths);

        let provider = create_test_provider_with_api_key(
            "test",
            Some("test-key".to_string()),
            Some("https://api.test.com/v1".to_string()),
        );

        register_provider("test", provider, &key);

        let result = create_provider("test", &paths);
        assert!(result.is_some());

        clear_cache(&paths);
    }

    #[test]
    fn create_provider_without_base_url() {
        let paths = unique_paths("no_base_url");
        let key = crate::registry::agent_paths_cache_key(&paths);

        let provider =
            create_test_provider_with_api_key("test", Some("test-key".to_string()), None);

        register_provider("test", provider, &key);

        let result = create_provider("test", &paths);
        assert!(result.is_some());

        clear_cache(&paths);
    }

    #[test]
    fn create_provider_returns_none_without_api_key() {
        let paths = unique_paths("no_api_key");
        let key = crate::registry::agent_paths_cache_key(&paths);

        let provider = create_test_provider_with_api_key("test", None, None);

        register_provider("test", provider, &key);

        let result = create_provider("test", &paths);
        assert!(result.is_none());

        clear_cache(&paths);
    }
}
