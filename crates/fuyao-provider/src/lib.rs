//! Provider 模块
//!
//! 供应商注册表、API Key 解析器、Provider 工厂、Provider trait 抽象。
//! 基于 reqwest 自建 HTTP 客户端，不依赖 async-openai。

pub mod client;
pub mod error;
pub mod openai;
pub mod provider;
pub mod registry;
pub mod resolver;
pub mod retry;
pub mod stream_decoder;

pub use client::{ClientError, create_provider, create_provider_with_model, parse_model_id};
pub use error::ProviderError;
pub use openai::OpenAIProvider;
pub use provider::{
    BoxStream, ChatMessage, ChatRequest, ChatResponse, FinishReason, Provider, StreamError,
    StreamEvent, StreamOptions, StreamUsage, ToolCallData,
};
pub use registry::{
    agent_paths_cache_key, clear_cache, get_model, get_provider, list_models, list_providers,
    register_model, register_provider,
};
pub use resolver::{get_base_url, resolve_api_key};
pub use retry::{backoff_duration, is_retryable};
pub use stream_decoder::StreamDecoder;
