//! 供应商/模型注册表
//!
//! 按 AgentPaths 缓存 Provider/Model，支持三层配置隔离。
//!
//! 每个 agent_paths 对应独立的 Provider/Model 注册表：
//! - agent_paths_key = (agent_id, workspace_str)
//! - 同一个 agent_paths 的多次调用复用缓存
//! - 不同 agent_paths 可以有不同的 Provider 配置

use crate::openai::OpenAIProvider;
use crate::provider::Provider as ProviderTrait;
use fuyao_api::{AgentPaths, Model, Provider};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

/// 全局 Provider 配置缓存（provider_id → Provider 配置，按 agent_paths 维度隔离）
static PROVIDER_CACHE: LazyLock<Mutex<HashMap<String, HashMap<String, Provider>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 全局 Model 缓存
static MODEL_CACHE: LazyLock<Mutex<HashMap<String, HashMap<String, Model>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Provider 实例注册表（多 Provider 路由）
///
/// 与上面的全局 PROVIDER_CACHE（Provider **配置**）不同：本结构持有
/// 已构造好的 `Arc<dyn Provider>` **实例**，按 `provider_id` 查询。
///
/// 引擎级共享（Engine 持有 `Arc<ProviderRegistry>`）。session 的
/// `SessionParams.model_config.model_id` 形如 `"provider_id/model_id"`——拆出 `provider_id`
/// 从本注册表取 Provider 实例，实现"不同 session 用不同 Provider"。
/// model_config 整 session 共享一份，可经 `Engine::update_session_params` 随时切。
///
/// 与旧"引擎持单个 `Arc<dyn Provider>`"模型的差异：
/// - 旧：启动时按 default model_id 选一个 Provider 实例，所有调用都打到这里
/// - 新：启动时把所有已注册 Provider 都建实例；每次调用按 session 的 provider_id 路由
///
/// 失败容错：单个 Provider 实例构造失败（如 API Key 缺失）不影响其他——
/// `from_registered` 跳过失败的并记 WARN，调用方用到该 provider_id 时
/// `get` 返回 None，由上层报错（消息级 fail-loud）。
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    /// provider_id → Provider 实例（key 已小写规范化）
    instances: HashMap<String, Arc<dyn ProviderTrait>>,
}

impl ProviderRegistry {
    /// 从全局配置注册表批量构造 Provider 实例
    ///
    /// 遍历 `list_providers(agent_paths)` 的每个 provider_id，调
    /// [`OpenAIProvider::new`] 建实例。单个失败（API Key 未配等）仅记 WARN 跳过，
    /// 其余成功的照常注册——支持渐进配置（部分 provider 配错也能启动引擎）。
    ///
    /// 调用方应在返回后检查 [`is_empty`](Self::is_empty)：空表示所有 provider
    /// 都建实例失败（通常是配置文件 / 环境变量都没设），引擎无法启动。
    pub fn from_registered(agent_paths: &AgentPaths) -> Self {
        let mut instances: HashMap<String, Arc<dyn ProviderTrait>> = HashMap::new();
        let providers = list_providers(agent_paths);
        for provider_id in providers.keys() {
            match OpenAIProvider::new(provider_id, agent_paths) {
                Some(p) => {
                    instances.insert(provider_id.to_lowercase(), Arc::new(p));
                }
                None => {
                    tracing::warn!(
                        provider = %provider_id,
                        "Provider 实例创建失败（通常是 API Key 未配置），该 provider 将不可用"
                    );
                }
            }
        }
        Self { instances }
    }

    /// 按 provider_id 查 Provider 实例
    ///
    /// key 大小写不敏感（内部已小写规范化）。找不到返回 None——由调用方
    /// （通常是 `turn.rs`）转成 `OutputEvent::Error` 给 UI，错误信息精准指向
    /// 哪个 provider_id 未注册。
    pub fn get(&self, provider_id: &str) -> Option<Arc<dyn ProviderTrait>> {
        self.instances.get(&provider_id.to_lowercase()).cloned()
    }

    /// 是否没有任何可用 Provider 实例
    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// 列出所有已注册实例的 provider_id（小写，用于诊断/日志）
    pub fn provider_ids(&self) -> Vec<String> {
        self.instances.keys().cloned().collect()
    }
}

impl ProviderRegistry {
    /// 手动注入一个 Provider 实例（带 provider_id 标签）
    ///
    /// 生产代码用 [`from_registered`](Self::from_registered) 从配置构造；
    /// 此方法供调用方（如装配层 / 测试）直接注入已构造的 Provider 实例，
    /// 例如把 MockProvider 包成 registry 供单元测试用。
    pub fn with_instance(provider_id: &str, instance: Arc<dyn ProviderTrait>) -> Self {
        let mut instances = HashMap::new();
        instances.insert(provider_id.to_lowercase(), instance);
        Self { instances }
    }
}

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

    // ===== ProviderRegistry 单测 =====
    //
    // MockProvider 是最小 Provider 实现：所有方法返回空/默认值，仅用于占位。
    // ProviderRegistry 本身不关心 Provider 内部行为，只关心按 provider_id 路由。

    /// 最小 Provider 实现（测试占位用）
    struct MockProvider;

    #[async_trait::async_trait]
    impl ProviderTrait for MockProvider {
        fn stream_chat(
            &self,
            _request: crate::provider::ChatRequest,
            _model: &str,
            _options: crate::provider::StreamOptions,
        ) -> crate::provider::BoxStream<
            Result<crate::provider::StreamEvent, crate::provider::StreamError>,
        > {
            // 空流——ProviderRegistry 不关心 Provider 行为
            Box::pin(futures_util::stream::empty())
        }

        async fn chat(
            &self,
            _request: crate::provider::ChatRequest,
            _model: &str,
            _options: crate::provider::StreamOptions,
        ) -> Result<crate::provider::ChatResponse, crate::provider::StreamError> {
            Ok(crate::provider::ChatResponse {
                content: None,
                reasoning: None,
                tool_calls: None,
                usage: crate::provider::StreamUsage::default(),
                finish_reason: crate::provider::FinishReason::Stop,
            })
        }
    }

    #[test]
    fn provider_registry_with_instance_lookup() {
        let instance: Arc<dyn ProviderTrait> = Arc::new(MockProvider);
        let registry = ProviderRegistry::with_instance("aliyun", instance);

        // 大小写不敏感查询
        assert!(registry.get("aliyun").is_some());
        assert!(registry.get("ALIYUN").is_some());
        assert!(registry.get("Aliyun").is_some());
        assert!(registry.get("nonexistent").is_none());
        assert!(!registry.is_empty());
    }

    #[test]
    fn provider_registry_default_is_empty() {
        let registry = ProviderRegistry::default();
        assert!(registry.is_empty());
        assert!(registry.get("any").is_none());
    }

    #[test]
    fn provider_registry_provider_ids_returns_lowercased() {
        let instance: Arc<dyn ProviderTrait> = Arc::new(MockProvider);
        let registry = ProviderRegistry::with_instance("DeepSeek", instance);
        let ids = registry.provider_ids();
        assert_eq!(ids, vec!["deepseek".to_string()]);
    }

    /// from_registered 在没有注册任何 provider 时返回空 registry（不 panic）
    #[test]
    fn provider_registry_from_registered_empty_when_no_provider() {
        let paths = unique_paths("from_empty");
        let registry = ProviderRegistry::from_registered(&paths);
        assert!(registry.is_empty());
        // 清理（虽然没注册什么，保险起见）
        clear_cache(&paths);
    }
}
