//! 队列操作（双队列协同）
//!
//! guide / pending 两个对等队列的核心操作：
//! - [`consume_all_guide`]：一次性取出 guide 全部消息（非阻塞）
//! - [`drain_pending_to_guide`]：pending 全部倒进 guide（无条件，幂等）
//! - [`inject_messages`]：把一批队列消息经 `emit_to_history` 单条落 DB
//!
//! 消费语义（一次性全取）：触发消费时机时，guide 有多少条全部取出，
//! 每条经 `emit_to_history`（拦截 → `store.insert_message` 落 DB → 发送事件 → 观察），
//! 与 assistant / tool_result 走完全相同的统一管道。
//!
//! User 消息的拦截/发送/观察**全在消费时刻**统一发生（入队纯排队，无 side effect）。

use super::SessionCtx;
use crate::dispatch;
use crate::engine::types::SharedQueue;
use fuyao_api::message::output::UserMessage as OutputUserMessage;
use fuyao_api::{ImageContent, Message, OutputEvent};

/// 一次性取出 guide 全部消息（非阻塞，drain 清空队列）
pub(crate) fn consume_all_guide(guide: &SharedQueue) -> Vec<OutputUserMessage> {
    let mut q = guide.lock().unwrap_or_else(|e| e.into_inner());
    q.drain(..).collect()
}

/// pending 全部倒进 guide（无条件，幂等，锁顺序 pending→guide）
///
/// pending 为空时立即返回。锁顺序固定 pending 先、guide 后，无死锁风险。
pub(crate) fn drain_pending_to_guide(guide: &SharedQueue, pending: &SharedQueue) {
    let mut p = pending.lock().unwrap_or_else(|e| e.into_inner());
    if p.is_empty() {
        return;
    }
    let mut g = guide.lock().unwrap_or_else(|e| e.into_inner());
    while let Some(q) = p.pop_front() {
        g.push_back(q);
    }
}

/// 把一批队列消息经 `emit_to_history` 单条落 DB
///
/// 每条 output 侧 `OutputUserMessage` 直接包成 `OutputEvent::User`，经统一管道：
/// 拦截 → 构造 `Message::user` 调 `store.insert_message` 单条落 DB → 发送事件给 UI → 观察钩子。
///
/// 与 assistant / tool_result 完全对称——拦截/存储/发送三者同源，插件可在消费时刻
/// 改写或阻断 user 消息（修复"拦截裂缝在 user 消息上重现"的结构性缺陷）。
///
/// Block 时：该消息不落库、不发（插件的责任，与 assistant Block 语义一致）。
///
/// **图片忠实落库**：带图消息不做任何模型能力判断——图片经入站节流（超限压缩、
/// 失败省略）后随消息原样落库、原样发送。不支持图片输入的模型由 provider 返回
/// 4xx 显式报错（fail-loud），引擎不擅自降级。
pub(crate) async fn inject_messages(ctx: &SessionCtx, msgs: Vec<OutputUserMessage>) {
    for m in msgs {
        // 入站节流（CPU 密集：base64 解码 + 图像编解码）放阻塞线程池，避免占用 async worker
        let (kept, failed) = if m.payload.images.is_empty() {
            (Vec::new(), 0usize)
        } else {
            let imgs = m.payload.images.clone();
            let total = imgs.len();
            match tokio::task::spawn_blocking(move || super::normalize::normalize_images(&imgs))
                .await
            {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(
                        session_id = %ctx.emitter.session_id(),
                        image_count = total,
                        cause = %e,
                        "图片节流任务异常，全部按失败处理"
                    );
                    (Vec::new(), total)
                }
            }
        };

        let event = OutputEvent::User(m);
        let _ = dispatch::emit_to_history(
            &ctx.emitter,
            &ctx.hooks,
            ctx.store.as_ref(),
            event,
            move |ev| user_msg_from_event(ev, kept, failed),
        )
        .await;
    }
}

/// 图片入站节流失败时的占位文本（解码失败 / 压缩后仍超限）
const IMAGE_PROCESS_FAILED_PLACEHOLDER: &str = "[图片已省略：图片处理失败]";

/// 从 User 输出事件构造 Message（emit_to_history 闭包）
///
/// 拦截后的 content 用于构造 Message——保证「拦截 → 存储 → 发送」三者一致。
/// 带图消息：图已在 [`inject_messages`] 经阻塞池节流得到 `kept`（达标图）与 `failed`
/// （失败计数），失败图替换为占位文本，不拖累其余图；达标图随消息原样落库。
fn user_msg_from_event(
    ev: &OutputEvent,
    kept: Vec<ImageContent>,
    failed: usize,
) -> Option<Message> {
    match ev {
        OutputEvent::User(m) => {
            // 无图消息：纯文本落库
            if m.payload.images.is_empty() {
                return Some(Message::user(m.payload.content.clone()));
            }
            // 有图消息：节流失败计数 > 0 时附加占位文本（处理失败容错，非能力判断）
            if failed > 0 {
                tracing::warn!(
                    session_id = %m.base.session_id.as_deref().unwrap_or(""),
                    image_count = m.payload.images.len(),
                    failed = failed,
                    "图片处理失败，已省略并附加占位文本"
                );
            }
            let content = if failed > 0 {
                append_placeholder(&m.payload.content, IMAGE_PROCESS_FAILED_PLACEHOLDER)
            } else {
                m.payload.content.clone()
            };
            Some(Message::user_with_images(content, kept))
        }
        _ => None,
    }
}

/// 在消息文本后附加占位文本（纯图消息则占位文本即全文）
fn append_placeholder(content: &str, placeholder: &str) -> String {
    if content.trim().is_empty() {
        placeholder.to_string()
    } else {
        format!("{content}\n\n{placeholder}")
    }
}
