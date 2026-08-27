//! Anthropic Messages 协议 Provider 模块
//!
//! 请求体编码（[`request`]）与流式线解码（[`sse`]）均为纯函数 / 纯状态机
//! 模块，与传输层解耦、可独立单测。

pub mod request;
pub mod sse;
