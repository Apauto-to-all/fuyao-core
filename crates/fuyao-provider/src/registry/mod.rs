//! Provider 实例路由（值类型，与全局缓存正交）
//!
//! [`ProviderRegistry`] 持有已构造好的 `Arc<dyn Provider>` 实例，按 `provider_id`
//! 查询——这是引擎级共享（Engine 持有 `Arc<ProviderRegistry>`）的路由层。
//!
//! 与 [`cache`]（进程级 Provider/Model **配置**缓存）是两个不相关抽象：数据与调用
//! 零重叠。唯一耦合点是 [`ProviderRegistry::from_registered`]——它读取
//! [`cache::list_providers`] 驱动实例构造。

mod cache;

// 再导出全局缓存 API，保持 `fuyao_provider::registry::*` 与 lib.rs 的再导出路径不变
pub use cache::{
    agent_paths_cache_key, clear_cache, get_model, get_provider, list_models, list_providers,
    register_model, register_provider,
};

use crate::openai::OpenAIProvider;
use crate::provider::Provider as ProviderTrait;
use fuyao_api::AgentPaths;
use std::collections::HashMap;
use std::sync::Arc;

/// Provider 实例注册表（多 Provider 路由）
///
/// 持有已构造好的 `Arc<dyn Provider>` **实例**，按 `provider_id` 查询。
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
    /// 遍历 [`list_providers`](cache::list_providers) 的每个 provider_id，调
    /// [`OpenAIProvider::new`] 建实例。单个失败（API Key 未配等）仅记 WARN 跳过，
    /// 其余成功的照常注册——支持渐进配置（部分 provider 配错也能启动引擎）。
    ///
    /// 调用方应在返回后检查 [`is_empty`](Self::is_empty)：空表示所有 provider
    /// 都建实例失败（通常是配置文件 / 环境变量都没设），引擎无法启动。
    ///
    /// 这是本值类型与 [`cache`] 全局缓存的唯一耦合点——读缓存配置驱动实例构造。
    pub fn from_registered(agent_paths: &AgentPaths) -> Self {
        let mut instances: HashMap<String, Arc<dyn ProviderTrait>> = HashMap::new();
        let providers = cache::list_providers(agent_paths);
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn unique_paths(test_name: &str) -> AgentPaths {
        AgentPaths {
            agent_id: Some(format!("test/{test_name}")),
            workspace: None,
            ..Default::default()
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
