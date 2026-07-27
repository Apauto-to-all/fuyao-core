//! 引擎实现 [`SubagentOps`] trait
//!
//! 让工具调用上下文持有的弱引用能调引擎的派生子 session 能力。
//! trait 在 fuyao-api 定义（最小接口 + 错误返字符串），引擎 impl 转发现有方法。
//!
//! 错误处理：[`EngineError`] 实 `Display`，转 `String` 后返 `Err`——
//! 调用方（如子代理 handler）把错误描述作为工具结果回喂父 ReAct，不需要按错误类型分支。

use std::future::Future;
use std::pin::Pin;

use fuyao_api::{ChildSessionSource, InputEvent, OutputEvent, SessionParams, SubagentOps};
use tokio::sync::mpsc;

use super::Engine;

impl SubagentOps for Engine {
    fn create_child_session<'a>(
        &'a self,
        parent_session_id: &'a str,
        source: ChildSessionSource,
        params: SessionParams,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(String, mpsc::UnboundedReceiver<OutputEvent>), String>>
                + Send
                + 'a,
        >,
    > {
        // parent_session_id: &str → &SessionId（String）需要临时 owned（trait 签名收 &str，
        // Engine::create_child_session 收 &SessionId；两层接口对齐时改其中之一可省此分配）
        let parent_id = parent_session_id.to_string();
        Box::pin(async move {
            Engine::create_child_session(self, &parent_id, source, params)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn send<'a>(
        &'a self,
        id: &'a str,
        event: InputEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        let id = id.to_string();
        Box::pin(async move {
            Engine::send(self, &id, event)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn end_session<'a>(
        &'a self,
        id: &'a str,
        end_reason: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        let id = id.to_string();
        Box::pin(async move {
            Engine::end_session(self, &id, end_reason)
                .await
                .map_err(|e| e.to_string())
        })
    }
}
