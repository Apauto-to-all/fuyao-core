//! Provider 模块
//!
//! 供应商注册表、字段解析器、Provider trait 抽象、供应商管理功能底座
//! （admin 域：写回 global 层的落存储原语与载荷校验；管理门面在装配层）。
//! 基于 reqwest 自建 HTTP 客户端，不依赖 async-openai。

pub mod admin;
mod error;
mod factory;
mod openai;
mod provider;
mod registry;
mod resolver;
mod stream_decoder;

pub use admin::{ProviderAdminError, ProviderModelSpec, ProviderSpec};
pub use error::ProviderError;
pub use factory::{BuildProviderError, build_provider};
pub use openai::OpenAIProvider;
pub use provider::{
    BoxStream, ChatMessage, ChatRequest, ChatResponse, FinishReason, Provider, StreamError,
    StreamEvent, StreamOptions, StreamUsage,
};
pub use registry::{
    ProviderRegistry, agent_paths_cache_key, clear_cache, get_model, get_provider, list_models,
    list_providers, register_model, register_provider, unregister_model, unregister_provider,
};
pub use resolver::{get_base_url, parse_model_id, resolve_api_key};
pub use stream_decoder::StreamAggregator;
