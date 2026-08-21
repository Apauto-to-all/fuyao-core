//! Provider / Model 配置的进程级全局缓存
//!
//! 按 `AgentPaths` 维度隔离的 Provider / Model **配置**缓存（静态可变映射）。
//! 与同目录 [`super`]（`ProviderRegistry` 实例路由值类型）是两个不相关抽象：
//! - 本模块缓存「配置」（`fuyao_api::Provider` / `fuyao_api::Model`），供装配层注册
//! - `ProviderRegistry` 持有「已构造的 Provider 实例」，供引擎按 provider_id 路由调用
//!
//! 两者数据与调用零重叠；`ProviderRegistry::from_registered` 是唯一耦合点——
//! 它读取本模块的 `list_providers` 驱动实例构造。

use fuyao_api::{AgentPaths, Model, Provider};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// 全局 Provider 配置缓存（provider_id → Provider 配置，按 agent_paths 维度隔离）
static PROVIDER_CACHE: LazyLock<Mutex<HashMap<String, HashMap<String, Provider>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 全局 Model 配置缓存（full_id → Model 配置，按 agent_paths 维度隔离）
static MODEL_CACHE: LazyLock<Mutex<HashMap<String, HashMap<String, Model>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 锁获取：中毒互斥量时恢复内部数据继续运行（避免一次 panic 永久致缓存不可用）
///
/// 进程级缓存的中毒风险只在持锁期间 panic 时出现；此处选择恢复而非 propagate，
/// 让后续请求仍能读写缓存——诊断已由 panic 本身留下，缓存可用性优先。
fn lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|e| e.into_inner())
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
    let mut cache = lock(&PROVIDER_CACHE);
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
    let mut cache = lock(&MODEL_CACHE);
    if !cache.contains_key(agent_paths_key) {
        cache.insert(agent_paths_key.to_string(), HashMap::new());
    }
    cache
        .get_mut(agent_paths_key)
        .expect("agent_paths_key 必须存在")
        .insert(full_id.to_lowercase(), model);
}

/// 反注册 Provider：从指定 agent_paths 的缓存移除一条 Provider 配置
///
/// 运行时移除路径（供应商删除后的内存一致性）：key 大小写归一同 register。
/// 移除不存在的条目为幂等 no-op。
///
/// # Arguments
/// * `provider_id` - Provider ID
/// * `agent_paths_key` - agent_paths 缓存 key
pub fn unregister_provider(provider_id: &str, agent_paths_key: &str) {
    let mut cache = lock(&PROVIDER_CACHE);
    if let Some(providers) = cache.get_mut(agent_paths_key) {
        providers.remove(&provider_id.to_lowercase());
    }
}

/// 反注册 Model：从指定 agent_paths 的缓存移除一条 Model 配置
///
/// key 大小写归一同 register；移除不存在的条目为幂等 no-op。
///
/// # Arguments
/// * `full_id` - 完整模型 ID（provider_id/model_id）
/// * `agent_paths_key` - agent_paths 缓存 key
pub fn unregister_model(full_id: &str, agent_paths_key: &str) {
    let mut cache = lock(&MODEL_CACHE);
    if let Some(models) = cache.get_mut(agent_paths_key) {
        models.remove(&full_id.to_lowercase());
    }
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
    let cache = lock(&PROVIDER_CACHE);
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
    let cache = lock(&MODEL_CACHE);
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
    let cache = lock(&MODEL_CACHE);
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
    let cache = lock(&PROVIDER_CACHE);
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
        let mut cache = lock(&PROVIDER_CACHE);
        cache.remove(&key);
    }
    {
        let mut cache = lock(&MODEL_CACHE);
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
            agent_id: Some(format!("global/{test_name}")),
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

    // ===== 反注册（运行时移除的内存一致性） =====

    /// 反注册 Provider：移除后 get 不到；大小写归一；移除不存在的幂等 no-op
    #[test]
    fn unregister_provider_removes_and_is_idempotent() {
        let paths = unique_paths("unreg_provider");
        let key = agent_paths_cache_key(&paths);
        register_provider("aliyun", create_test_provider("aliyun"), &key);

        unregister_provider("ALIYUN", &key);
        assert!(
            get_provider("aliyun", &paths).is_none(),
            "大小写归一后应移除"
        );

        // 再移除（条目已不存在）不 panic、不影响缓存结构
        unregister_provider("aliyun", &key);
        assert!(get_provider("aliyun", &paths).is_none());
    }

    /// 反注册 Provider 不波及其他条目（Model 缓存与别的 Provider 动）
    #[test]
    fn unregister_provider_keeps_other_entries_intact() {
        let paths = unique_paths("unreg_prov_keeps_others");
        let key = agent_paths_cache_key(&paths);
        register_provider("aliyun", create_test_provider("aliyun"), &key);
        register_provider("other", create_test_provider("other"), &key);
        register_model("other/m2", create_test_model("m2"), &key);

        unregister_provider("aliyun", &key);

        assert!(
            get_provider("other", &paths).is_some(),
            "其他 provider 不受波及"
        );
        assert!(
            get_model("other/m2", &paths).is_some(),
            "其他模型条目不受波及"
        );
        clear_cache(&paths);
    }

    /// 反注册 Model：移除后 get 不到；大小写归一；幂等 no-op
    #[test]
    fn unregister_model_removes_and_is_idempotent() {
        let paths = unique_paths("unreg_model");
        let key = agent_paths_cache_key(&paths);
        register_model(
            "aliyun/qwen3.6-plus",
            create_test_model("qwen3.6-plus"),
            &key,
        );

        unregister_model("Aliyun/QWEN3.6-PLUS", &key);
        assert!(
            get_model("aliyun/qwen3.6-plus", &paths).is_none(),
            "大小写归一后应移除"
        );
        unregister_model("aliyun/qwen3.6-plus", &key);
        assert!(get_model("aliyun/qwen3.6-plus", &paths).is_none());
    }
}
