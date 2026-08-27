//! Anthropic Messages 协议 Provider 模块
//!
//! 请求体编码（[`request`]）、流式线解码（[`sse`]）、非流式响应解析
//! （[`completion`]）、HTTP 错误分类（[`classify`]）均为纯函数 / 纯状态机
//! 模块，与传输层解耦、可独立单测。

pub mod classify;
pub mod completion;
pub mod request;
pub mod sse;
