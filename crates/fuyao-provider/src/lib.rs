//! Provider 模块
//!
//! 供应商注册表、字段解析器、Provider trait 抽象。
//! 基于 reqwest 自建 HTTP 客户端，不依赖 async-openai。

mod error;
mod openai;
mod provider;
mod registry;
mod resolver;
mod retry;
mod stream_decoder;

pub use error::ProviderError;
pub use openai::OpenAIProvider;
pub use provider::{
    BoxStream, ChatMessage, ChatRequest, ChatResponse, FinishReason, Provider, StreamError,
    StreamEvent, StreamOptions, StreamUsage, ToolCallData,
};
pub use registry::{
    ProviderRegistry, agent_paths_cache_key, clear_cache, get_model, get_provider, list_models,
    list_providers, register_model, register_provider,
};
pub use resolver::{get_base_url, parse_model_id, resolve_api_key};
pub use retry::{backoff_duration, is_retryable};
pub use stream_decoder::StreamDecoder;
