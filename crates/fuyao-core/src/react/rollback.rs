//! 对话回退执行
//!
//! 处理控制通道的 Rollback 命令：调 store 层的单事务原子回退（删目标 seq 之后的消息 +
//! 重算 count 类字段），再把领域结果投影成 wire 载荷发 Rollback 事件。失败发 Error 事件，
//! 不影响 task 后续运行（turn 边界语义：回退失败等价于没回退）。

use super::SessionCtx;
use crate::dispatch;
use fuyao_api::EventBase;
use fuyao_api::UserMessageMode;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::input::UserMessageSource;
use fuyao_api::message::output::{RollbackMessage, RollbackPayload, UserPayload};

/// 执行对话回退（控制通道 Rollback 命令的处理）
///
/// 与手动压缩在控制通道里地位对等：都是 task 在 turn 边界自执行的 DB 写命令。
/// 复用 store 层 [`fuyao_session::SessionStore::rollback_to`] 的单事务原子执行体
/// （删目标 seq 之后的所有消息 + 重算 count 类字段 + 局部 UPDATE sessions）。
///
/// 三步：
/// 1. 调 `rollback_to`（内部已校验目标 role/kind，非法目标事务回滚、DB 不变；
///    重算的 count 类字段已在事务内局部 UPDATE 写回 sessions 表——DB 唯一数据源，
///    无需内存刷新）
/// 2. 投影：把领域结果 `RollbackResult` 转成 wire 载荷 `RollbackPayload`
///    （目标消息本体 `Message` → `UserPayload`，按「回退后将作为新 guide 重新发送」
///    补 mode=Guide / source=User——这是应用语义，属 react 层职责，不入 store）
/// 3. 发 `OutputEvent::Rollback` 事件（经 dispatch 管道：拦截 → 发送 → 观察），
///    前端据此显示「已回退 N 条」通知 + 把目标用户消息填输入框
///
/// 失败处理：`rollback_to` 返回错误时（目标不存在 / 非法目标 / session 不存在），
/// 发 `OutputEvent::Error` 让前端感知，不 panic、不影响 task 后续运行（turn 边界语义：
/// 回退失败等价于没回退，task 继续按原状态跑）。
pub(super) async fn run_rollback(ctx: &SessionCtx, target_seq: i64) {
    let result = match ctx
        .store
        .rollback_to(ctx.emitter.session_id(), target_seq)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                target_seq = target_seq,
                cause = %e,
                "对话回退失败"
            );
            dispatch::dispatch(
                &ctx.emitter,
                &ctx.hooks,
                OutputEvent::Error(fuyao_api::message::output::ErrorMessage {
                    base: EventBase::default(),
                    payload: fuyao_api::message::output::ErrorPayload {
                        message: format!("对话回退失败：{e}"),
                        recoverable: true,
                    },
                }),
            )
            .await;
            return;
        }
    };

    // rollback_to 事务内已把重算的 count 类字段局部 UPDATE 写回 sessions 表，
    // DB 是唯一数据源——无需内存刷新（下轮 build_chat_request 等读路径均从 DB 取）。

    tracing::info!(
        session_id = ctx.emitter.session_id(),
        target_seq = result.target_seq,
        deleted_total = result.deleted_total,
        message_count = result.message_count,
        "对话回退完成"
    );

    // 投影：领域结果 → output wire 载荷
    let payload = RollbackPayload {
        target_seq: result.target_seq,
        deleted_count: result.deleted_count,
        deleted_total: result.deleted_total,
        // 目标 user 消息 → UserPayload（填输入框）：取 content/images，
        // 按「回退后将作为新 guide 重新发送」补 mode/source——原消息的 mode/source 语义不再适用
        target_message: result.target_message.as_ref().map(|m| UserPayload {
            content: m.content.clone().unwrap_or_default(),
            images: m.images.clone(),
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        }),
        message_count: result.message_count,
        tool_call_count: result.tool_call_count,
        last_compacted_seq: result.last_compacted_seq,
        compression_count: result.compression_count,
    };

    // 发 Rollback 事件：前端据此显示「已回退 N 条」通知 + 把目标用户消息填输入框
    dispatch::dispatch(
        &ctx.emitter,
        &ctx.hooks,
        OutputEvent::Rollback(RollbackMessage {
            base: EventBase::default(),
            payload,
        }),
    )
    .await;
}
