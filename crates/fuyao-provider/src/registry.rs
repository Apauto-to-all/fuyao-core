//! 供应商/模型注册表
//!
//! 按 AgentPaths 缓存 Provider/Model，支持三层配置隔离。
//!
//! 每个 agent_paths 对应独立的 Provider/Model 注册表：
//! - agent_paths_key = (agent_id, workspace_str)
//! - 同一个 agent_paths 的多次调用复用缓存
//! - 不同 agent_paths 可以有不同的 Provider 配置

use fuyao_api::{AgentPaths, Model, Provider};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// 全局 Provider 缓存
static PROVIDER_CACHE: LazyLock<Mutex<HashMap<String, HashMap<String, Provider>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 全局 Model 缓存
static MODEL_CACHE: LazyLock<Mutex<HashMap<String, HashMap<String, Model>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 生成 agent_paths 缓存 key
pub fn agent_paths_cache_key(agent_paths: &AgentPaths) -> String {
    let agent_id = agent_paths.agent_id.as_deref().unwrap_or("");
    let workspace = agent_paths
        .workspace
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    format!("{agent_id}|{workspace}")
}

/// 注册 Provider 到指定 agent_paths
///
/// # Arguments
/// * `provider_id` - Provider ID
/// * `provider` - Provider 配置
/// * `agent_paths_key` - agent_paths 缓存 key
pub fn register_provider(provider_id: &str, provider: Provider, agent_paths_key: &str) {
    let mut cache = PROVIDER_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if !cache.contains_key(agent_paths_key) {
        cache.insert(agent_paths_key.to_string(), HashMap::new());
    }
    cache
        .get_mut(agent_paths_key)
        .expect("agent_paths_key 必须存在")
        .insert(provider_id.to_lowercase(), provider);
}

/// 注册 Model 到指定 agent_paths
///
/// # Arguments
/// * `full_id` - 完整模型 ID（provider_id/model_id）
/// * `model` - Model 配置
/// * `agent_paths_key` - agent_paths 缓存 key
pub fn register_model(full_id: &str, model: Model, agent_paths_key: &str) {
    let mut cache = MODEL_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if !cache.contains_key(agent_paths_key) {
        cache.insert(agent_paths_key.to_string(), HashMap::new());
    }
    cache
        .get_mut(agent_paths_key)
        .expect("agent_paths_key 必须存在")
        .insert(full_id.to_lowercase(), model);
}

/// 获取 Provider
///
/// # Arguments
/// * `provider_id` - Provider ID
/// * `agent_paths` - Agent 三层目录的身份证明
///
/// # Returns
/// Provider 如果找到，否则 None
pub fn get_provider(provider_id: &str, agent_paths: &AgentPaths) -> Option<Provider> {
    let cache = PROVIDER_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let key = agent_paths_cache_key(agent_paths);
    cache
        .get(&key)
        .and_then(|providers| providers.get(&provider_id.to_lowercase()).cloned())
}

/// 获取 Model
///
/// # Arguments
/// * `full_id` - 完整模型 ID（provider_id/model_id）
/// * `agent_paths` - Agent 三层目录的身份证明
///
/// # Returns
/// Model 如果找到，否则 None
pub fn get_model(full_id: &str, agent_paths: &AgentPaths) -> Option<Model> {
    let cache = MODEL_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let key = agent_paths_cache_key(agent_paths);
    cache
        .get(&key)
        .and_then(|models| models.get(&full_id.to_lowercase()).cloned())
}

/// 列出指定 agent_paths 的所有 Model
///
/// # Arguments
/// * `agent_paths` - Agent 三层目录的身份证明
///
/// # Returns
/// full_id -> Model 的字典
pub fn list_models(agent_paths: &AgentPaths) -> HashMap<String, Model> {
    let cache = MODEL_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let key = agent_paths_cache_key(agent_paths);
    cache.get(&key).cloned().unwrap_or_default()
}

/// 列出指定 agent_paths 的所有 Provider
///
/// # Arguments
/// * `agent_paths` - Agent 三层目录的身份证明
///
/// # Returns
/// Provider ID -> Provider 的字典
pub fn list_providers(agent_paths: &AgentPaths) -> HashMap<String, Provider> {
    let cache = PROVIDER_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let key = agent_paths_cache_key(agent_paths);
    cache.get(&key).cloned().unwrap_or_default()
}

/// 清除指定 agent_paths 的缓存
///
/// # Arguments
/// * `agent_paths` - Agent 三层目录的身份证明
pub fn clear_cache(agent_paths: &AgentPaths) {
    let key = agent_paths_cache_key(agent_paths);
    {
        let mut cache = PROVIDER_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.remove(&key);
    }
    {
        let mut cache = MODEL_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::{ModelCost, ModelLimit, ModelModalities, ProviderOptions};
    use std::path::PathBuf;

    fn create_test_model(name: &str) -> Model {
        Model {
            name: name.to_string(),
            cost: ModelCost::default(),
            limit: ModelLimit::default(),
            reasoning_efforts: vec![],
            modalities: ModelModalities::default(),
        }
    }

    fn create_test_provider(name: &str) -> Provider {
        Provider {
            name: name.to_string(),
            models: HashMap::new(),
            options: ProviderOptions::default(),
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
    fn agent_paths_cache_key_with_both() {
        let paths = AgentPaths {
            agent_id: Some("global/coder".to_string()),
            workspace: Some(PathBuf::from("/home/user/project")),
            ..Default::default()
        };
        let key = agent_paths_cache_key(&paths);
        assert_eq!(key, "global/coder|/home/user/project");
    }

    #[test]
    fn agent_paths_cache_key_with_none() {
        let paths = AgentPaths::default();
        let key = agent_paths_cache_key(&paths);
        assert_eq!(key, "|");
    }

    #[test]
    fn register_and_get_provider() {
        let paths = unique_paths("get_provider");
        let key = agent_paths_cache_key(&paths);
        let provider = create_test_provider("aliyun");

        register_provider("aliyun", provider.clone(), &key);
        let result = get_provider("aliyun", &paths);
        assert!(result.is_some());
        assert_eq!(result.expect("Provider 必须存在").name, "aliyun");

        clear_cache(&paths);
    }

    #[test]
    fn register_and_get_model() {
        let paths = unique_paths("get_model");
        let key = agent_paths_cache_key(&paths);
        let model = create_test_model("qwen3.6-plus");

        register_model("aliyun/qwen3.6-plus", model.clone(), &key);
        let result = get_model("aliyun/qwen3.6-plus", &paths);
        assert!(result.is_some());
        assert_eq!(result.expect("Model 必须存在").name, "qwen3.6-plus");

        clear_cache(&paths);
    }

    #[test]
    fn get_provider_case_insensitive() {
        let paths = unique_paths("case_insensitive");
        let key = agent_paths_cache_key(&paths);
        let provider = create_test_provider("aliyun");

        register_provider("Aliyun", provider, &key);
        let result = get_provider("ALIYUN", &paths);
        assert!(result.is_some());

        clear_cache(&paths);
    }

    #[test]
    fn list_providers_returns_all() {
        let paths = unique_paths("list_providers");
        let key = agent_paths_cache_key(&paths);

        register_provider("aliyun", create_test_provider("aliyun"), &key);
        register_provider("deepseek", create_test_provider("deepseek"), &key);

        let providers = list_providers(&paths);
        assert_eq!(providers.len(), 2);
        assert!(providers.contains_key("aliyun"));
        assert!(providers.contains_key("deepseek"));

        clear_cache(&paths);
    }

    #[test]
    fn clear_cache_removes_all() {
        let paths = unique_paths("clear_all");
        let key = agent_paths_cache_key(&paths);

        register_provider("aliyun", create_test_provider("aliyun"), &key);
        register_model(
            "aliyun/qwen3.6-plus",
            create_test_model("qwen3.6-plus"),
            &key,
        );

        clear_cache(&paths);

        assert!(get_provider("aliyun", &paths).is_none());
        assert!(get_model("aliyun/qwen3.6-plus", &paths).is_none());
    }
}
