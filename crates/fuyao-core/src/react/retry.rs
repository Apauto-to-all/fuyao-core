//! LLM 调用重试驱动（RetryRunner）
//!
//! 在 ReAct 循环与 stream 模块之间插入一层：驱动 [`stream::run_stream_session`]，
//! 遇可恢复错误自动重试，每次重试前发 [`OutputEvent::Retry`] 给 UI。
//!
//! ## 设计边界
//!
//! - **per-session**：函数接 `&SessionCtx`，天然持 session_id（发 Retry 事件用）
//! - **不感知中断**：本模块不监听 `rx_interrupt`。重试 sleep 期间若外层 select! 决定
//!   中断，只需 drop 本函数返回的 future——`tokio::time::sleep` 自然取消，
//!   控制权回到外层中断分支。重试与中断完全正交。
//! - **不污染 stream 模块**：stream.rs 只做流式解码（错误冒泡），本模块在更外层协调
//! - **不污染 interrupt 模块**：不引入 phase / Backoff 字段
//!
//! ## 策略（高层 session retry 语义）
//!
//! | 错误类别 | 处理 |
//! |---------|------|
//! | 首 chunk 后的错误 | 立即冒泡（已吐内容难以撤销，重试会导致 UI 重复输出）|
//! | 严重错误（Auth / 4xx / StreamParseError / ContextOverflow）| 立即冒泡 |
//! | 可恢复错误（RateLimit / Timeout / Connection / 5xx）| 退避后重试，次数上限 `RetryConfig.max_retries` |
//!
//! 首 chunk 判定：流式开始后 `TurnState.text` 或 `reasoning` 非空即视为已吐首 chunk
//! （即助手已开始输出的判定语义）。
//!
//! ## 退避
//!
//! 见 [`fuyao_provider::backoff_duration`]：起始 2000ms ×2 指数，无响应头封顶 30s，
//! 有 retry-after 头用头值（封顶 ≈24.8 天）。
//!
//! ## 端到端测试
//!
//! 单元测试需要构造完整 `SessionCtx`（含真实 SessionStore），成本高。
//! 实际行为测试放在 `fuyao-app/tests/assembly_test.rs`——那里已有完整的
//! 临时 SessionStore + MockProvider fixture，覆盖：
//! - 可恢复错误：发 Retry 事件 → 重试 → 产出 Assistant
//! - 严重错误：不发 Retry 事件，Error 冒泡
//! - 首 chunk 后错误：不发 Retry 事件
//! - max_retries 耗尽：连发多条 Retry 事件后 Error 冒泡

use crate::dispatch;
use crate::emit::Emitter;
use crate::interrupt::SharedTurnState;
use crate::react::SessionCtx;
use crate::stream::{self, StreamResult};
use fuyao_api::get_config;
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::output::{RetryMessage, RetryPayload};
use fuyao_hooks::SharedHooks;
use fuyao_provider::{
    Provider, StreamDecoder, StreamError, StreamOptions, backoff_duration, is_retryable,
};
use std::sync::Arc;

/// 驱动一次完整的「LLM 调用 + 重试」过程
///
/// 内部循环：每次都重建 `StreamDecoder`，调 [`stream::run_stream_session`]，
/// 按错误类型决定是否重试。`request` 由调用方传入（已构建好的 `ChatRequest`，
/// 每次 retry 复用同一份——本 session 的 messages 在一次 LLM 调用内不变）。
///
/// `state` 由调用方传入并共享给 stream——这样外层 select! 中断分支可以读到本次累积的
/// 部分结果（发增量 AssistantMessage）。重试成功时 state 持最终一轮的累积结果；
/// 重试失败（错误冒泡）时 state 持最后一次尝试的累积结果（可能为空）。
///
/// `provider` 由调用方传入（`turn.rs` 从 `ProviderRegistry` 按 provider_id 解析后传入）。
/// retry / stream 不直接接 `ProviderRegistry`——保持职责单一（只关心一个 Provider 实例）。
///
/// 重试期间发 [`OutputEvent::Retry`]（含 attempt / wait_ms / cause）经 dispatch 管道。
/// 退避 sleep 期间外层 select! 可随时 drop 本 future 触发中断（sleep 自然取消）。
///
/// 返回值由调用方（`turn.rs`）处理：Ok → 走 ReAct 正常分支；Err → 发 Error 事件 + 落库。
pub(crate) async fn run_stream_with_retry(
    ctx: &SessionCtx,
    request: fuyao_provider::ChatRequest,
    model: &str,
    options: &StreamOptions,
    provider: &Arc<dyn Provider>,
    state: &SharedTurnState,
) -> Result<StreamResult, StreamError> {
    let max_retries = get_config().llm.retry.max_retries;
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;

        // 每次重试都重建 decoder + 清空 state（不携带上一次的部分结果）
        let mut decoder = StreamDecoder::new();
        {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.text.clear();
            s.reasoning.clear();
            s.tool_calls.clear();
        }

        // request 在重试间不变（一次 LLM 调用内 session.messages 不变）——直接 clone
        let result = stream::run_stream_session(
            request.clone(),
            model,
            options.clone(),
            provider,
            &ctx.emitter,
            &ctx.hooks,
            &mut decoder,
            state,
        )
        .await;

        match result {
            Ok(r) => return Ok(r),
            Err(e) => {
                // 首 chunk 判定：state 持有本轮累积的 text/reasoning，非空说明已吐过首 chunk
                let has_started = {
                    let s = state.lock().unwrap_or_else(|e| e.into_inner());
                    !s.text.is_empty() || !s.reasoning.is_empty()
                };

                // 三种停止重试的情形：首 chunk 后 / 严重错误 / 次数耗尽
                if has_started || !is_retryable(&e) || attempt > max_retries {
                    return Err(e);
                }

                // 可恢复错误：发 Retry 事件 → sleep 退避 → 重试
                let wait = backoff_duration(attempt, &e);
                emit_retry_event(
                    &ctx.emitter,
                    &ctx.hooks,
                    attempt,
                    max_retries,
                    wait.as_millis() as u64,
                    &e,
                )
                .await;
                // 退避 sleep 期间若收到 shutdown 信号，立即冒泡 Cancelled
                // （让 turn.rs 的 shutdown 分支走中断路径，不发 Error 事件）
                // 不监听 rx_interrupt——重试与中断正交，sleep 期间被中断靠外层 select! drop future
                tokio::select! {
                    biased;
                    _ = ctx.shutdown_token.cancelled() => {
                        return Err(StreamError::Cancelled);
                    }
                    _ = tokio::time::sleep(wait) => {}
                }
                // 回 loop 顶部：attempt += 1，清空 state 重试
            }
        }
    }
}

/// 发 OutputEvent::Retry 给 UI（经 dispatch 管道：拦截 → 发送 → 观察）
async fn emit_retry_event(
    emitter: &Emitter,
    hooks: &SharedHooks,
    attempt: u32,
    max_retries: u32,
    wait_ms: u64,
    error: &StreamError,
) {
    // 结构化日志（按 AGENTS.md 规范，重试属可恢复 → WARN 不报警）
    tracing::warn!(
        attempt = attempt,
        max_retries = max_retries,
        wait_ms = wait_ms,
        cause = %error,
        session_id = emitter.session_id(),
        "LLM 调用失败，将重试"
    );

    // 发 OutputEvent::Retry 给 UI（经统一管道，可被拦截/观察）
    dispatch::dispatch(
        emitter,
        hooks,
        OutputEvent::Retry(RetryMessage {
            base: EventBase::default(),
            payload: RetryPayload {
                attempt,
                max_retries,
                wait_ms,
                cause: error.to_string(),
            },
        }),
        None,
    )
    .await;
}
