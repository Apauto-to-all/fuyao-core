//! fuyao-session 集成测试共享 fixture
//!
//! 提供四类构造能力：
//! - temp_store：真实 SQLite + tempdir 隔离（复用 src/store/tests.rs 的范式：
//!   std::mem::forget(dir) 让 TempDir 跨 async await 保活）
//! - 唯一 AgentPaths：隔离全局 MODEL_CACHE（用唯一 agent_id + clear_cache 收尾）
//! - FakeTitleProvider：手写 fake Provider（trait 标了 #[async_trait]，automock 不适用；
//!   仿 fuyao-provider/src/registry.rs 的 MockProvider 风格，用于 maybe_generate_title 测试）
//! - 价格表构造器：构造带 ModelCost 的 Model，用于 calculate_cost 真实集成
//!
//! 全部使用默认配置（不调 set_config），走 get_config 未 set 返回 default 的兜底，
//! 规避 set_config 的 OnceLock 进程级单例串扰。

// 跨测试二进制共享：未用部分不报 dead_code
#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use fuyao_api::{AgentPaths, Model, ModelCost, PriceTier};
use fuyao_provider::{
    BoxStream, ChatRequest, ChatResponse, FinishReason, Provider, StreamError, StreamEvent,
    StreamOptions, StreamUsage,
};
use fuyao_session::SessionStore;
use tempfile::tempdir;

// ============================================================================
// temp_store：真实 SQLite + tempdir 隔离
// ============================================================================

/// 构造独立 SQLite 文件的 SessionStore（每个测试一个临时目录）
///
/// 用 std::mem::forget(dir) 放弃 TempDir 的自动清理——async 测试跨 await 持有路径，
/// TempDir 提前 drop 会删掉 db 文件导致后续查询失败。临时目录由系统在重启时清理。
/// （复用 src/store/tests.rs 与 src/compressor/apply.rs 的既有范式）
pub async fn temp_store() -> SessionStore {
    let dir = tempdir().expect("创建临时目录失败");
    let db_path = dir.path().join("test.db");
    std::mem::forget(dir);
    SessionStore::new(db_path).await.expect("创建存储失败")
}

// ============================================================================
// 唯一 AgentPaths：隔离全局 MODEL_CACHE
// ============================================================================

/// 用原子计数器生成全局唯一的 agent_id，隔离 MODEL_CACHE / PROVIDER_CACHE
///
/// 全局注册表用 agent_paths_key 作 HashMap key，测试间用唯一 agent_id 避免串扰。
/// 调用方测完应 clear_cache 收尾。
pub fn unique_paths(tag: &str) -> AgentPaths {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    AgentPaths {
        agent_id: Some(format!("global/{tag}-{n}")),
        workspace: None,
        extra_dirs: vec![],
        fuyao_home: dirs_or_default(),
    }
}

/// 获取 fuyao_home（与 AgentPaths::default 内部一致，避免重复调 get_fuyao_home）
fn dirs_or_default() -> std::path::PathBuf {
    AgentPaths::default().fuyao_home
}

// ============================================================================
// 价格表构造器：calculate_cost 真实集成用
// ============================================================================

/// 构造带固定价格表的 Model（输入 2 元/M、输出 12 元/M）
///
/// 用于 calculate_cost / fill_message_cost 的真实集成测试：
/// register_model 注入此 Model 后，calculate_cost 应返回非零费用。
pub fn priced_model(name: &str) -> Model {
    // 价格 Decimal 字面量：整数 2/12 与小数 0.4
    fn d(v: &str) -> rust_decimal::Decimal {
        v.parse().unwrap()
    }
    Model {
        name: name.to_string(),
        cost: ModelCost {
            input: Some(d("2")),
            output: Some(d("12")),
            reasoning: None,
            cache: Some(d("0.4")),
            tiers: vec![PriceTier {
                max_tokens: 200_000,
                input: Some(d("2")),
                output: Some(d("12")),
                reasoning: None,
                cache: Some(d("0.4")),
            }],
        },
        limit: fuyao_api::ModelLimit::default(),
        reasoning_efforts: vec![],
        modalities: fuyao_api::ModelModalities::default(),
    }
}

// ============================================================================
// FakeTitleProvider：maybe_generate_title 测试用
// ============================================================================

/// 可配置的 fake Provider——chat 返回固定标题文本，stream_chat 返回空流
///
/// 用于 maybe_generate_title 端到端测试：构造时注入期望返回的标题内容，
/// 验证从 ProviderRegistry 取实例 → chat → clean_title 的全链路。
pub struct FakeTitleProvider {
    /// chat 返回的 content（None 时返回错误，测试失败回退路径）
    pub chat_content: Option<String>,
}

#[async_trait]
impl Provider for FakeTitleProvider {
    fn stream_chat(
        &self,
        _request: ChatRequest,
        _model: &str,
        _options: StreamOptions,
    ) -> BoxStream<Result<StreamEvent, StreamError>> {
        // 标题生成只用 chat（非流式），stream_chat 在此场景不被调用，返回空流
        Box::pin(futures_util::stream::empty())
    }

    async fn chat(
        &self,
        _request: ChatRequest,
        _model: &str,
        _options: StreamOptions,
    ) -> Result<ChatResponse, StreamError> {
        match &self.chat_content {
            Some(content) => Ok(ChatResponse {
                content: Some(content.clone()),
                reasoning: None,
                tool_calls: None,
                usage: StreamUsage::default(),
                finish_reason: FinishReason::Stop,
            }),
            // chat_content=None 模拟 LLM 调用失败（触发 maybe_generate_title 回退/放弃）
            None => Err(StreamError::ApiError {
                status: None,
                message: "模拟标题生成失败".into(),
            }),
        }
    }
}

/// 构造注入 fake provider 的 ProviderRegistry（单实例，provider_id 固定）
///
/// maybe_generate_title 内部用 parse_model_id("prov/model") 取 provider_id="prov"，
/// 再从 registry.get("prov") 取实例。这里注册 id="prov" 的 fake。
pub fn registry_with_title_provider(content: &str) -> fuyao_provider::ProviderRegistry {
    let provider = Arc::new(FakeTitleProvider {
        chat_content: Some(content.to_string()),
    });
    fuyao_provider::ProviderRegistry::with_instance("prov", provider)
}

/// 构造 chat 恒失败的 ProviderRegistry（测试 fast 失败 → 回退主模型 → 也失败 → 返回 None）
pub fn registry_with_failing_provider() -> fuyao_provider::ProviderRegistry {
    let provider = Arc::new(FakeTitleProvider { chat_content: None });
    fuyao_provider::ProviderRegistry::with_instance("prov", provider)
}

/// 标题生成测试的 agent_paths 占位清理（no-op）
///
/// maybe_generate_title 的 _agent_paths 参数未使用（不查 MODEL_CACHE），
/// 此处仅为保持测试代码对称性，实际无全局状态需要清理。
pub fn cleanup_unused(_paths: &AgentPaths) {
    // maybe_generate_title 不触碰全局 model 注册表，无需清理
}
