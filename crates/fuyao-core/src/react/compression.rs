//! 上下文压缩子系统
//!
//! 负责在对话历史逼近上下文窗口上限时，调一次摘要 LLM 把早期消息折叠为摘要，
//! 释放 token 预算。两层职责：
//! - 触发层（[`run_pre_turn_compression`] / [`run_manual_compression`]）：判定「该不该压」
//!   ——前者走阈值门 + 子会话豁免，后者跳过判定直接压（用户意图优先）。
//! - 执行层（[`run_compression`]）：负责「怎么压」——发 Started → 流式摘要 → 落库 → 发 Ended。

use super::SessionCtx;
use super::builders;
use crate::dispatch;
use fuyao_api::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::output::{
    CompressionDeltaPayload, CompressionEndedPayload, CompressionMessage, CompressionPayload,
    CompressionReason, CompressionStartedPayload,
};

/// 压缩执行依赖的解析结果（model_id / 思考配置 / provider / 上下文长度）
///
/// 子代理 session 的压缩豁免判定（可配置，`[session.compression] skip_child` 默认 true）
///
/// 子代理以 Fresh 模式派生、只回传最终回复文本给父 Agent，自身完整历史留在子 session 内。
/// 若子代理中途压缩，早期工具证据会被摘要替代，最终回复失真并作为 tool_result 传导给父 Agent
/// 的决策；且压缩需额外调一次摘要 LLM，对一次性子代理在成本与质量上均不划算。
/// 仅自动触发路径（[`run_pre_turn_compression`]）使用此豁免——引擎替用户挡不划算的压缩；
/// 手动触发（[`run_manual_compression`]）不豁免：用户显式要对子会话压缩是用户的选择，照做。
fn compression_exempt(ctx: &SessionCtx) -> bool {
    ctx.is_child && ctx.compression_config.skip_child
}

/// 解析压缩执行所需的模型信息
///
/// **前缀缓存红线**：压缩必须用主对话这一轮的同一个 Provider/endpoint，否则原样发的请求
/// 会因 endpoint 切换导致前缀缓存失效。
///
/// model_id / thinking / context_length 解析统一走 [`builders::resolve_model`]（与主对话
/// `run_turn` 同一份逻辑，避免散算漂移）；provider 实例是压缩路径专属步骤——
/// [`builders::resolve_model`] 故意不查 registry（保持纯构造边界），故在此单独取。
///
/// 注：写回逻辑（turn.rs）已在首轮后把 model_id + thinking 物化进 session_params，
/// 正常运行期这里读到的都是 Some。None→default 分支仅首轮前 / 未物化时兜底。
async fn resolve_compression_model(
    ctx: &SessionCtx,
) -> Option<(
    builders::ResolvedModel,
    std::sync::Arc<dyn fuyao_provider::Provider>,
)> {
    let model_config = ctx.session_params.lock().await.model_config.clone();
    let resolved = builders::resolve_model(
        &model_config,
        &ctx.tools,
        ctx.is_child,
        &ctx.definition.tools,
        &ctx.agent_paths,
    )
    .map_err(|msg| {
        tracing::warn!(
            session_id = ctx.emitter.session_id(),
            cause = %msg,
            "压缩跳过：模型解析失败（model_id 无效或为空）"
        );
        msg
    })
    .ok()?;

    let provider = ctx.providers.get(&resolved.provider_id).or_else(|| {
        tracing::warn!(
            session_id = ctx.emitter.session_id(),
            provider_id = %resolved.provider_id,
            "压缩跳过：Provider 实例未注册"
        );
        None
    })?;

    Some((resolved, provider))
}

/// Pre-turn 自动上下文压缩检查（「该不该压」的阈值门）
///
/// 在主循环注入新消息前、调 LLM 前，根据上一轮真实 usage 判定要不要压缩。
/// 触发条件满足时调 [`run_compression`]（reason = auto）。手动触发见 [`run_manual_compression`]。
pub(super) async fn run_pre_turn_compression(ctx: &SessionCtx) {
    if compression_exempt(ctx) {
        return;
    }

    // 读取上一轮真实 usage（首轮无 usage 跳过——还没跑过没法判定）
    let usage = match ctx.last_usage.lock().await.clone() {
        Some(u) => u,
        None => return,
    };

    let (model, provider) = match resolve_compression_model(ctx).await {
        Some(v) => v,
        None => return,
    };

    // 模型未注册 → 注册表查不到 limit.context，无从判定阈值——不编造数字，
    // 跳过本次压缩判定（该模型的 turn 调用自会在 provider 处失败，不在压缩侧兜底）
    let Some(context_length) = model.context_length else {
        tracing::debug!(
            session_id = ctx.emitter.session_id(),
            model_id = %model.model_id,
            "模型未注册，上下文长度未知，跳过压缩判定"
        );
        return;
    };

    // 阈值检测：prompt_tokens >= threshold × (context_length - summary_max_tokens)
    let trigger = fuyao_session::should_compress(
        usage.prompt_tokens,
        context_length,
        &ctx.compression_config,
    );
    if !trigger {
        return;
    }

    run_compression(
        ctx,
        CompressionReason::Auto,
        usage.prompt_tokens,
        &model,
        &provider,
        None,
    )
    .await;
}

/// 手动触发上下文压缩（控制通道 Compress 命令的处理）
///
/// 与自动压缩（[`run_pre_turn_compression`]）共用 [`run_compression`] 执行流程，区别有二：
/// ① 跳过阈值检测（用户意图优先，不判「该不该压」）；
/// ② 不适用子会话豁免——用户显式要对子会话压缩是用户的选择，引擎照做，
///    子代理失真风险由用户自担（自动压缩替用户挡不划算的压缩，手动不挡）。
/// 触发原因标记为 manual。
///
/// `note` 为控制命令附言（发送方对摘要的侧重要求），原样传给摘要生成；
/// 自动触发路径没有用户附言，恒为 None——附言仅手动路径携带。
pub(super) async fn run_manual_compression(ctx: &SessionCtx, note: Option<&str>) {
    let (model, provider) = match resolve_compression_model(ctx).await {
        Some(v) => v,
        None => return,
    };

    // prompt_tokens：取上一轮真实 usage（首轮前无 usage 则 0——仅用于 Started 事件展示）
    let prompt_tokens = ctx
        .last_usage
        .lock()
        .await
        .as_ref()
        .map(|u| u.prompt_tokens)
        .unwrap_or(0);

    run_compression(
        ctx,
        CompressionReason::Manual,
        prompt_tokens,
        &model,
        &provider,
        note,
    )
    .await;
}

/// 执行一次上下文压缩（「怎么压」的执行体）
///
/// 自动 / 手动两条触发路径共用本函数：
/// - 自动（[`run_pre_turn_compression`]）：先过阈值门，命中才调（reason = auto），
///   无附言（None）
/// - 手动（[`run_manual_compression`]）：跳过阈值门直接调（reason = manual，用户意图优先），
///   可携带控制命令附言
///
/// `note` 为控制命令附言（发送方对摘要的侧重要求），原样传给摘要生成——
/// 是否提供不影响压缩流程本身。
///
/// 流程：发 Started → 调摘要 LLM（流式 Delta 并发转发）→ apply 落库 → 发 Ended。
/// 失败处理（失败保持边界 + 错误分级）：
/// - 摘要为空 / 无可压缩内容：log warn 跳过
/// - LLM 调用失败：log warn 跳过（下次照常触发判定）
/// - 落库失败：log warn 跳过
///
/// 同步执行：task 内串行，期间不接收新消息（天然互斥，不需要锁/队列/通道）。
async fn run_compression(
    ctx: &SessionCtx,
    reason: CompressionReason,
    prompt_tokens: u32,
    model: &builders::ResolvedModel,
    provider: &std::sync::Arc<dyn fuyao_provider::Provider>,
    note: Option<&str>,
) {
    // 上下文长度未知（模型未注册）时按 0：仅影响 Started 事件展示值，
    // 摘要 LLM 调用会因模型不存在在 provider 处失败，不在压缩侧兜底
    let context_length = model.context_length.unwrap_or(0);

    tracing::info!(
        session_id = ctx.emitter.session_id(),
        reason = ?reason,
        prompt_tokens = prompt_tokens,
        context_length = context_length,
        model_id = %model.model_id,
        "触发上下文压缩"
    );

    // 发 Compression Started 事件：调摘要 LLM 之前，让前端显示"压缩中..."状态
    dispatch::dispatch(
        &ctx.emitter,
        &ctx.hooks,
        OutputEvent::Compression(CompressionMessage {
            base: EventBase::default(),
            payload: CompressionPayload::Started(CompressionStartedPayload {
                reason,
                prompt_tokens,
                context_length,
            }),
        }),
    )
    .await;

    // 从 DB 加载可见窗口：与主对话同口径，前缀缓存可复用。
    // generate_summary 内部不切窗，把传入 messages 全量发给 LLM
    let visible_messages = match ctx
        .store
        .load_visible_messages(ctx.emitter.session_id())
        .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %e,
                "压缩前加载可见消息失败，跳过本次压缩"
            );
            return;
        }
    };

    // 执行层：生成摘要（原消息原样发，前缀缓存完整命中）
    //
    // 流式增量通过 channel 转发到并发的 Delta 事件发送任务：
    // - callback 是同步 FnMut，无法 await dispatch，所以用 try_send 推到 channel
    // - select! 并发：generate_summary 与 Delta 消费者同时跑，每收到一条就发 Compression Delta
    // - channel 满了 try_send 失败就丢（Delta 本就是 live-only 增量，丢得起）
    let (delta_tx, mut delta_rx) =
        tokio::sync::mpsc::channel::<(Option<String>, Option<String>)>(32);

    // 构造压缩用 options：复用 session 思考配置（tools 由 generate_summary 内部强制清空）
    let compression_options = fuyao_provider::StreamOptions {
        thinking_type: model.options.thinking_type.clone(),
        reasoning_effort: model.options.reasoning_effort.clone(),
        ..fuyao_provider::StreamOptions::default()
    };

    // callback 用 let 绑定避免临时值生命周期问题（future 会借用它）
    // move：让闭包持有 delta_tx，select! 后 drop(on_delta) 即释放 delta_tx，
    // 使 delta_consumer 的 recv 返回 None 退出（否则 delta_tx 留在作用域致死锁）
    let mut on_delta = move |content: Option<&str>, reasoning: Option<&str>| {
        let _ = delta_tx.try_send((content.map(String::from), reasoning.map(String::from)));
    };

    // Delta 消费者：循环从 channel 取 delta 发事件，delta_tx drop 后 recv 返回 None 退出
    let mut delta_consumer = Box::pin(async {
        while let Some((content, reasoning)) = delta_rx.recv().await {
            dispatch::dispatch(
                &ctx.emitter,
                &ctx.hooks,
                OutputEvent::Compression(CompressionMessage {
                    base: EventBase::default(),
                    payload: CompressionPayload::Delta(CompressionDeltaPayload {
                        content,
                        reasoning,
                    }),
                }),
            )
            .await;
        }
    });

    // system_prompt 从 DB 现读（压缩重建后已落库，这里读到的是最新值）
    let system_prompt: Option<String> = match ctx.store.get(ctx.emitter.session_id()).await {
        Ok(Some(s)) => s.system_prompt,
        Ok(None) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                "压缩时 session 行不存在，system_prompt 取空"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %e,
                "压缩时读 system_prompt 失败，按空继续"
            );
            None
        }
    };

    // summary 与 Delta 消费者并发跑；summary 完成后 on_delta（含 delta_tx）drop，
    // Delta 消费者 recv 返回 None 自然退出
    let summary = tokio::select! {
        biased;
        s = fuyao_session::generate_summary(
            system_prompt.as_deref(),
            &visible_messages,
            provider,
            &model.model,
            note,
            compression_options,
            &mut on_delta,
        ) => {
            match s {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        session_id = ctx.emitter.session_id(),
                        cause = %e,
                        "摘要生成失败，跳过本次压缩"
                    );
                    return;
                }
            }
        }
        _ = &mut delta_consumer => {
            unreachable!("Delta 消费者先于 summary 结束")
        }
    };
    // drop on_delta：释放其持有的 delta_tx → delta_consumer 的 recv 返回 None 自然退出
    drop(on_delta);
    // 等 Delta 消费者把剩余积压推完
    let _ = (&mut delta_consumer).await;

    // 落地层：写 compaction 边界消息（可见窗口 = 最新摘要 + 摘要后消息，读取侧动态拼接）
    // reason 透传给 mark_compaction，保证 DB 审计列（tool_name）与 Started/Ended 事件 reason 一致
    match ctx
        .store
        .mark_compaction(
            ctx.emitter.session_id(),
            summary.content.clone(),
            reason.into(),
        )
        .await
    {
        Ok(new_seq) => {
            // 重建 system_prompt：build_system_prompt 纯本地拼接（不调 LLM），
            // 保证旧 system 中残留的动态内容（如"基于刚才的 X 错误继续排查"）在
            // X 已被压进摘要后不再误导模型
            // definition 创建时定死不应变（前缀缓存红线），复用 ctx.definition 零加载
            // 用途按 parent_session_id 推断：子 session（子代理）用 Subagent 校验，
            // 主 session / fork 用 Primary。与创建时的用途保持一致。
            let usage = if ctx.is_child {
                fuyao_prompt::PromptUsage::Subagent
            } else {
                fuyao_prompt::PromptUsage::Primary
            };
            let new_prompt =
                fuyao_prompt::build_system_prompt(&ctx.agent_paths, &ctx.definition, usage);

            // 落库新 system_prompt（单字段 UPDATE，DB 唯一数据源）。
            // 失败时仅 warn 跳过：compaction 边界已落库，下轮请求 build_chat_request
            // 从 DB 读到的仍是旧 prompt——影响有限，不阻塞压缩流程
            if let Err(e) = ctx
                .store
                .update_system_prompt(ctx.emitter.session_id(), &new_prompt)
                .await
            {
                tracing::warn!(
                    session_id = ctx.emitter.session_id(),
                    cause = %e,
                    "system_prompt 落库失败，下轮请求仍用旧 prompt"
                );
            }

            // 发 Compression Ended 事件：apply 落库成功后，让前端移除"压缩中"状态、展示摘要
            dispatch::dispatch(
                &ctx.emitter,
                &ctx.hooks,
                OutputEvent::Compression(CompressionMessage {
                    base: EventBase::default(),
                    payload: CompressionPayload::Ended(CompressionEndedPayload {
                        reason,
                        content: summary.content.clone(),
                        new_seq,
                    }),
                }),
            )
            .await;
        }
        Err(e) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %e,
                "压缩落地失败，跳过本次压缩"
            );
        }
    }
}
