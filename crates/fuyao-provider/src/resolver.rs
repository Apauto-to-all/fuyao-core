//! Provider 字段解析器
//!
//! 从 Provider 配置或环境变量获取 API Key、base_url，以及解析模型 ID 字符串。

use crate::error::ProviderError;
use crate::registry::get_provider;
use fuyao_api::AgentPaths;

/// 解析 Provider 的 API Key
///
/// 优先级：
/// 1. ProviderOptions.api_key 配置（直接提供）
/// 2. 系统环境变量
///
/// # Arguments
/// * `provider_id` - Provider ID
/// * `agent_paths` - Agent 三层目录的身份证明
///
/// # Returns
/// API Key 字符串，如果未找到返回 None
pub fn resolve_api_key(provider_id: &str, agent_paths: &AgentPaths) -> Option<String> {
    let provider = get_provider(provider_id, agent_paths)?;

    // 优先使用 ProviderOptions.api_key 配置
    if let Some(api_key) = &provider.options.api_key {
        return Some(api_key.clone());
    }

    // 从系统环境变量查找
    for env_var in &provider.api_key_env_vars {
        if let Ok(value) = std::env::var(env_var) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    None
}

/// 获取 Provider 的 base_url
///
/// 使用 Provider 配置的 options.base_url。
///
/// # Arguments
/// * `provider_id` - Provider ID
/// * `agent_paths` - Agent 三层目录的身份证明
///
/// # Returns
/// base_url 如果有配置，否则 None
pub fn get_base_url(provider_id: &str, agent_paths: &AgentPaths) -> Option<String> {
    let provider = get_provider(provider_id, agent_paths)?;
    provider.options.base_url.clone()
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
/// (provider_id, model_id) 元组；格式错误返回 [`ProviderError::InvalidModelId`]
pub fn parse_model_id(model_id: &str) -> Result<(String, String), ProviderError> {
    let stripped = model_id.trim();
    if stripped.contains('/') {
        let parts: Vec<&str> = stripped.splitn(2, '/').collect();
        let provider_id = parts[0].trim().to_lowercase();
        let model_id = parts[1].trim();
        Ok((provider_id, model_id.to_string()))
    } else {
        Err(ProviderError::InvalidModelId(model_id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{clear_cache, register_provider};
    use fuyao_api::{Provider, ProviderOptions};

    fn create_test_provider_with_options(
        name: &str,
        api_key: Option<String>,
        base_url: Option<String>,
    ) -> Provider {
        Provider {
            name: name.to_string(),
            api_protocol: fuyao_api::ApiProtocol::OpenaiCompletions,
            models: std::collections::HashMap::new(),
            options: ProviderOptions { api_key, base_url },
            api_key_env_vars: Vec::new(),
        }
    }

    fn create_test_provider_with_env_vars(name: &str, env_vars: Vec<String>) -> Provider {
        Provider {
            name: name.to_string(),
            api_protocol: fuyao_api::ApiProtocol::OpenaiCompletions,
            models: std::collections::HashMap::new(),
            options: ProviderOptions::default(),
            api_key_env_vars: env_vars,
        }
    }

    fn unique_paths(test_name: &str) -> AgentPaths {
        AgentPaths {
            agent_id: Some(format!("global/{test_name}")),
            workspace: None,
            ..Default::default()
        }
    }

    #[test]
    fn resolve_api_key_from_provider_options() {
        let paths = unique_paths("opts");
        let key = crate::registry::agent_paths_cache_key(&paths);

        let provider =
            create_test_provider_with_options("test", Some("test-api-key".to_string()), None);

        register_provider("test", provider, &key);

        let result = resolve_api_key("test", &paths);
        assert_eq!(result, Some("test-api-key".to_string()));

        clear_cache(&paths);
    }

    #[test]
    fn resolve_api_key_from_env_var() {
        let paths = unique_paths("env_var");
        let key = crate::registry::agent_paths_cache_key(&paths);

        // SAFETY: 测试专用环境变量，不会影响其他代码
        unsafe {
            std::env::set_var("TEST_API_KEY_VAR", "env-api-key");
        }

        let provider =
            create_test_provider_with_env_vars("test", vec!["TEST_API_KEY_VAR".to_string()]);

        register_provider("test", provider, &key);

        let result = resolve_api_key("test", &paths);
        assert_eq!(result, Some("env-api-key".to_string()));

        // SAFETY: 清理测试专用环境变量
        unsafe {
            std::env::remove_var("TEST_API_KEY_VAR");
        }
        clear_cache(&paths);
    }

    #[test]
    fn resolve_api_key_provider_options_priority() {
        let paths = unique_paths("priority");
        let key = crate::registry::agent_paths_cache_key(&paths);

        // SAFETY: 测试专用环境变量，不会影响其他代码
        unsafe {
            std::env::set_var("TEST_PRIORITY_VAR", "env-key");
        }

        let provider = Provider {
            name: "test".to_string(),
            api_protocol: fuyao_api::ApiProtocol::OpenaiCompletions,
            models: std::collections::HashMap::new(),
            options: ProviderOptions {
                api_key: Some("options-key".to_string()),
                base_url: None,
            },
            api_key_env_vars: vec!["TEST_PRIORITY_VAR".to_string()],
        };

        register_provider("test", provider, &key);

        // ProviderOptions.api_key 应优先
        let result = resolve_api_key("test", &paths);
        assert_eq!(result, Some("options-key".to_string()));

        // SAFETY: 清理测试专用环境变量
        unsafe {
            std::env::remove_var("TEST_PRIORITY_VAR");
        }
        clear_cache(&paths);
    }

    #[test]
    fn resolve_api_key_not_found() {
        let paths = unique_paths("not_found");
        let result = resolve_api_key("nonexistent", &paths);
        assert!(result.is_none());
    }

    #[test]
    fn get_base_url_from_provider() {
        let paths = unique_paths("base_url");
        let key = crate::registry::agent_paths_cache_key(&paths);

        let provider = create_test_provider_with_options(
            "aliyun",
            None,
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1".to_string()),
        );

        register_provider("aliyun", provider, &key);

        let result = get_base_url("aliyun", &paths);
        assert_eq!(
            result,
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1".to_string())
        );

        clear_cache(&paths);
    }

    #[test]
    fn get_base_url_none_when_not_configured() {
        let paths = unique_paths("no_base_url");
        let key = crate::registry::agent_paths_cache_key(&paths);

        let provider = create_test_provider_with_options("test", None, None);

        register_provider("test", provider, &key);

        let result = get_base_url("test", &paths);
        assert!(result.is_none());

        clear_cache(&paths);
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
            ProviderError::InvalidModelId(_)
        ));
    }
}
