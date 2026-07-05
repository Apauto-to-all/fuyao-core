//! LLM 交互子系统
//!
//! 封装 LLM 流式调用的完整生命周期：
//! - 流式会话管理（重试/退避）
//! - 事件构建

pub(crate) mod event_builder;
pub(crate) mod stream_session;
