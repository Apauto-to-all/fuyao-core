//! 历史落库与回放映射（history）
//!
//! 「事件 ↔ Message」双向映射与计费时机的唯一归属：
//! - 正向 [`emit_to_history`] / [`emit_billed_to_history`]：拦截 → 事件投影成
//!   Message（计费内化）→ 单条落 DB（seq 回填）→ 发送 → 观察
//! - 反向 [`messages_to_events`]：存储 Message → OutputEvent 投影（历史回放，
//!   供上层查询接口使用）
//!
//! # 为何收敛
//!
//! 事件 payload 与 Message 落库形态的映射知识（含 tool_calls 的双向 typed
//! 直映射）曾经散在各 emit 闭包与 app 侧的反向手写映射里，费用计算的
//! 「何时调用」靠每个产出点自觉。收敛后：调用方只声明「发生了什么事件」，
//! 映射、计费、seq 回填成为不可遗忘的实现细节；双向投影同居一模块，
//! 落库格式一改，正反两个方向单点同步。
//!
//! # 计费内化的依据
//!
//! `AssistantPayload` 的 token 五字段由流式结果构造时自 usage 填入，且拦截
//! 钩子不改 usage（token 是模型给的客观值）——拦截后事件 payload 即计费权威。
//! 计费模型取 turn 发起时的快照（用哪个模型跑就按哪个计费，不受 mid-turn
//! 参数更新影响），经 [`emit_billed_to_history`] 显式传入；user / tool_result /
//! 中断补发（token 全 0）走不计费入口。两个入口二选一，新增产出点必须
//! 显式决定是否计费。
//!
//! # 存储模型固有限制
//!
//! `Message` 未持久化用户消息的 `mode` / `source`（DB schema 无此列），回放时统一按
//! 普通用户消息兜底（`mode = Guide`、`source = User`）。这是「给人看的历史浏览」的
//! 合理近似——插件 / 系统注入的 user 消息回放成普通 user，不影响可读性。

mod images;

use crate::dispatch;
use crate::react::SessionCtx;
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::input::{UserMessageMode, UserMessageSource};
use fuyao_api::message::output::UserMessage as OutputUserMessage;
use fuyao_api::message::output::{
    AssistantMessage, AssistantPayload, ToolCallPayload, ToolResultMessage, ToolResultPayload,
    UserPayload,
};
use fuyao_api::{AgentPaths, Message, MessageRole, ToolCallData};

// ===== 正向：事件 → Message → 落库 =====

/// 进历史统一入口（不计费路径）
///
/// 适用于本身无费用的消息：user 注入、tool_result、中断补发（token 全 0）。
/// assistant 正常产出**必须**走 [`emit_billed_to_history`]——两个入口二选一，
/// 新增产出点被迫显式决定是否计费。
///
/// 管道：拦截 → [`event_to_message`] 投影落 DB（seq 回填）→ 发送 → 观察。
/// Block 时：不落库、不发（插件的责任，消息不进历史、UI 看不到）。
pub(crate) async fn emit_to_history(ctx: &SessionCtx, event: OutputEvent) {
    emit_inner(ctx, event, None).await;
}

/// 进历史统一入口（计费路径）：assistant 正常产出专用
///
/// `model_id` 是计费归属模型（turn 发起时的快照）——费用按「实际跑的模型」算，
/// 并写入 `Message.model_id` 供归属查询。映射、计费、落库、seq 回填全部内化，
/// 调用方不可能漏计费。
pub(crate) async fn emit_billed_to_history(ctx: &SessionCtx, event: OutputEvent, model_id: &str) {
    emit_inner(ctx, event, Some(model_id)).await;
}

/// 管道主体：拦截 → 映射落库（seq 回填）→ 发送 → 观察
async fn emit_inner(ctx: &SessionCtx, event: OutputEvent, bill_model: Option<&str>) {
    // 1. 拦截（同步原地修改；Block：不落库、不发——插件的责任，消息不进历史、UI 看不到）
    let Some(mut intercepted) = dispatch::intercept(&ctx.hooks, event) else {
        return;
    };

    // 2. 用拦截后事件投影成 Message 后落 DB。
    //    sessions 表的计数 / 费用累加由 insert_message 事务内原子完成（单一数据源）。
    //    失败仅 warn——保证拦截→发送→观察管道不被 DB 写失败阻塞；
    //    调用方继续推进（消息可能丢失但 turn 流程不卡死，对齐 fail-loud 但不崩原则）
    if let Some(mut msg) = event_to_message(&intercepted, bill_model, &ctx.agent_paths) {
        if let Err(e) = ctx
            .store
            .insert_message(ctx.emitter.session_id(), &mut msg)
            .await
        {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %e,
                role = msg.role.as_str(),
                "消息落库失败（已丢弃，不影响 turn 推进）"
            );
        } else {
            // 落库成功：seq 已回填到 msg，反写进事件 base，使实时事件与历史回放同构。
            // 前端游标分页据此连续定位，实时 / 历史 id 不再分裂。
            intercepted.base_mut().seq = Some(msg.seq);
        }
    }

    // 3. 发送事件给 UI + 4. 观察钩子（deliver 按值消费事件）
    dispatch::deliver(&ctx.emitter, &ctx.hooks, intercepted).await;
}

/// 事件 → Message 投影（正向映射的全部知识）
///
/// 只处理进历史的三类事件（User / Assistant / ToolResult）；其余事件（Chunk /
/// Error / Interrupt 通知 / Title 等）不落库，调用方应走 `dispatch::dispatch`
/// 纯事件通道。类型不匹配返回 `None`：事件照常发送，只是不进历史。
fn event_to_message(
    ev: &OutputEvent,
    bill_model: Option<&str>,
    agent_paths: &AgentPaths,
) -> Option<Message> {
    match ev {
        OutputEvent::User(m) => {
            // 图片已由 inject_user_messages 预写节流结果进事件，此处原样落库
            if m.payload.images.is_empty() {
                Some(Message::user(m.payload.content.clone()))
            } else {
                Some(Message::user_with_images(
                    m.payload.content.clone(),
                    m.payload.images.clone(),
                ))
            }
        }
        OutputEvent::Assistant(m) => {
            let p = &m.payload;
            let mut msg = Message::assistant(p.content.clone());
            msg.reasoning = p.reasoning.clone();
            // tool_calls：事件 payload → typed 直映射，与拦截后事件严格同源。
            // arguments 取 tool_args 序列化（Value 序列化恒为合法 JSON 串），
            // 空列表归 None（与「无工具调用」语义一致）
            if let Some(calls) = &p.tool_calls
                && !calls.is_empty()
            {
                msg.tool_calls = Some(
                    calls
                        .iter()
                        .map(|tc| ToolCallData {
                            id: tc.tool_call_id.clone(),
                            name: tc.tool_name.clone(),
                            arguments: tc.tool_args.to_string(),
                        })
                        .collect(),
                );
            }
            msg.finish_reason = p.finish_reason.clone();
            // token 五字段：事件 payload 即权威（构造时自 usage 填入，拦截不改）
            msg.prompt_tokens = p.prompt_tokens;
            msg.completion_tokens = p.completion_tokens;
            msg.reasoning_tokens = p.reasoning_tokens;
            msg.cached_tokens = p.cached_tokens;
            // 计费 + 模型归属：bill_model 为 None（中断补发，token 全 0）时不计费、
            // 不填 model_id——与「中断时模型未给用量」的语义一致
            if let Some(model_id) = bill_model {
                msg.model_id = Some(model_id.to_string());
                fuyao_session::fill_message_cost(&mut msg, model_id, agent_paths);
            }
            // 不变量：assistant 消息 content 与 tool_calls 不得同时为空——
            // OpenAI 协议要求二者至少其一存在，双空消息进入历史会让下轮请求
            // 直接 400。双空只出现在「模型仅产出思考内容即被中断」的场景
            // （reasoning 非空、正文未开始），落库前补空串 content 使历史数据
            // 始终协议合法；content=None 且携带 tool_calls 的纯工具调用消息
            // 是合法形态，不干预。
            if msg.tool_calls.is_none() && msg.content.as_deref().is_none_or(str::is_empty) {
                msg.content = Some(String::new());
            }
            Some(msg)
        }
        OutputEvent::ToolResult(m) => Some(Message::tool_result(
            m.payload.tool_call_id.clone(),
            m.payload.tool_name.clone(),
            m.payload.content.clone(),
        )),
        _ => None,
    }
}

// ===== user 消息批量进历史（含图片入站节流） =====

/// 图片入站节流失败时的占位文本（解码失败 / 压缩后仍超限）
const IMAGE_PROCESS_FAILED_PLACEHOLDER: &str = "[图片已省略：图片处理失败]";

/// 把一批队列 user 消息经统一管道单条落 DB
///
/// 每条消息先做图片入站节流（CPU 密集，放阻塞线程池），**节流结果直接预写进
/// 事件**（达标图替换原图、失败计数 > 0 时附加占位文本）——UI 与 DB 看到同一份
/// 内容，「拦截 → 存储 → 发送」三者一致。之后每条过 [`emit_to_history`]：
/// 拦截 → 落 DB → 发送事件 → 观察钩子，与 assistant / tool_result 完全对称。
///
/// **图片忠实落库**：带图消息不做任何模型能力判断——图片经节流（超限压缩、
/// 失败省略）后随消息原样落库、原样发送。不支持图片输入的模型由 provider 返回
/// 4xx 显式报错（fail-loud），引擎不擅自降级。
pub(crate) async fn inject_user_messages(ctx: &SessionCtx, msgs: Vec<OutputUserMessage>) {
    for mut m in msgs {
        // 入站节流（base64 解码 + 图像编解码）放阻塞线程池，避免占用 async worker
        let (kept, failed) = if m.payload.images.is_empty() {
            (Vec::new(), 0usize)
        } else {
            let imgs = m.payload.images.clone();
            let total = imgs.len();
            match tokio::task::spawn_blocking(move || images::normalize_images(&imgs)).await {
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

        // 节流结果预写进事件：UI 与 DB 同源；失败图降级为占位文本（对话连续性优先）
        if failed > 0 {
            tracing::warn!(
                session_id = %ctx.emitter.session_id(),
                image_count = m.payload.images.len(),
                failed = failed,
                "图片处理失败，已省略并附加占位文本"
            );
            m.payload.content =
                append_placeholder(&m.payload.content, IMAGE_PROCESS_FAILED_PLACEHOLDER);
        }
        m.payload.images = kept;

        let event = OutputEvent::User(m);
        emit_to_history(ctx, event).await;
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

// ===== 反向：Message → 事件（历史回放投影） =====

/// 单条历史 Message → OutputEvent
///
/// 按 [`MessageRole`] 分流到 User / Assistant / ToolResult；`system` 无对应事件变体,
/// 返回 `None`（系统提示是构造而非对话内容，不进历史回放流）。
///
/// `base` 复刻原消息的 timestamp / session_id，并把 seq 直接填入——历史回放与实时事件
/// 同构：进历史事件落库后 seq 同样填进 base.seq，前端拿到同一来源的 seq，
/// 游标分页据此连续定位，无需区分实时 / 历史。
fn message_to_event(msg: &Message) -> Option<OutputEvent> {
    // base 复刻原消息时间戳与会话标识；seq 直接填入，与实时事件同源同构
    let base = EventBase {
        seq: Some(msg.seq),
        timestamp: msg.timestamp,
        session_id: Some(msg.session_id.clone()),
    };
    match msg.role {
        MessageRole::User => Some(OutputEvent::User(OutputUserMessage {
            base,
            payload: UserPayload {
                content: msg.content.clone().unwrap_or_default(),
                images: msg.images.clone(),
                // mode / source 未持久化，按普通用户消息兜底
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
            },
        })),
        MessageRole::Assistant => Some(OutputEvent::Assistant(AssistantMessage {
            base,
            payload: AssistantPayload {
                content: msg.content.clone(),
                reasoning: msg.reasoning.clone(),
                tool_calls: parse_tool_calls(msg.tool_calls.as_deref()),
                finish_reason: msg.finish_reason.clone(),
                completion_tokens: msg.completion_tokens,
                prompt_tokens: msg.prompt_tokens,
                // Message 未单独存 total_tokens，按 prompt + completion 求和近似
                total_tokens: msg.prompt_tokens + msg.completion_tokens,
                reasoning_tokens: msg.reasoning_tokens,
                cached_tokens: msg.cached_tokens,
            },
        })),
        MessageRole::Tool => Some(OutputEvent::ToolResult(ToolResultMessage {
            base,
            payload: ToolResultPayload {
                tool_call_id: msg.tool_call_id.clone().unwrap_or_default(),
                tool_name: msg.tool_name.clone().unwrap_or_default(),
                content: msg.content.clone().unwrap_or_default(),
            },
        })),
        // 系统消息是 prompt 构造，非对话内容，不进历史回放流
        MessageRole::System => None,
    }
}

/// typed tool_calls → 扁平 ToolCallPayload 列表
///
/// 字段直映射：`id → tool_call_id`、`name → tool_name`、
/// `arguments`（JSON 字符串）解析为 `tool_args`（`Value`）。
/// 解析失败兜底 `Value::Null`（外部脏数据容错，不阻断整段历史回放）；
/// 空列表归 None（与流式事件「无工具调用」语义一致）。
fn parse_tool_calls(tool_calls: Option<&[ToolCallData]>) -> Option<Vec<ToolCallPayload>> {
    let parsed: Vec<_> = tool_calls?
        .iter()
        .map(|tc| ToolCallPayload {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            tool_args: serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null),
        })
        .collect();
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

/// 历史消息列表 → 事件流（seq 正序，旧 → 新）
///
/// 接收「seq 倒序」（存储默认查询顺序，最新在前）的 [`Message`] 列表，反向遍历投影
/// 得正序（旧在前、新在后），使历史回放流与实时流时序一致。
///
/// 游标分页标准模式：存储层 `ORDER BY seq DESC`（倒序取数利于游标定位边界），业务层
/// 翻成正序返回——用户看对话是旧→新。`.iter().rev()` 反向遍历 DESC 输入即得 ASC，
/// 一次到位，不再额外翻转。
///
/// `system` 消息投影为 `None` 会被跳过，故返回长度可能小于输入。
pub fn messages_to_events(messages: Vec<Message>) -> Vec<OutputEvent> {
    messages.iter().rev().filter_map(message_to_event).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::ImageContent;

    // ── 测试辅助 ─────────────────────────────────────────────

    /// 构造一条 Message（零值兜底，按 role / 覆盖字段微调）
    fn make_msg(role: MessageRole, seq: i64, overrides: &dyn Fn(&mut Message)) -> Message {
        let mut msg = Message {
            id: Some(seq),
            session_id: "sess-1".into(),
            model_id: None,
            role,
            content: None,
            images: vec![],
            reasoning: None,
            tool_call_id: None,
            tool_calls: None,
            tool_name: None,
            finish_reason: None,
            timestamp: seq as f64,
            prompt_tokens: 0,
            completion_tokens: 0,
            reasoning_tokens: 0,
            cached_tokens: 0,
            cost: 0.0,
            seq,
            kind: fuyao_api::MessageKind::Message,
        };
        overrides(&mut msg);
        msg
    }

    /// 构造一条 Assistant 输出事件（零值兜底，按覆盖字段微调）
    fn make_assistant_event(overrides: &dyn Fn(&mut AssistantPayload)) -> OutputEvent {
        let mut payload = AssistantPayload {
            content: None,
            reasoning: None,
            tool_calls: None,
            finish_reason: None,
            completion_tokens: 0,
            prompt_tokens: 0,
            total_tokens: 0,
            reasoning_tokens: 0,
            cached_tokens: 0,
        };
        overrides(&mut payload);
        OutputEvent::Assistant(AssistantMessage {
            base: EventBase::default(),
            payload,
        })
    }

    // ── 正向映射：event_to_message ───────────────────────────

    #[test]
    fn billed_assistant_maps_tokens_model_and_finish_reason() {
        let ev = make_assistant_event(&|p| {
            p.content = Some("回复".into());
            p.finish_reason = Some("stop".into());
            p.prompt_tokens = 20;
            p.completion_tokens = 10;
            p.reasoning_tokens = 5;
            p.cached_tokens = 2;
        });
        let paths = AgentPaths::default();
        let msg = event_to_message(&ev, Some("test/m1"), &paths).expect("应投影成功");
        assert_eq!(msg.role, MessageRole::Assistant);
        assert_eq!(msg.content.as_deref(), Some("回复"));
        assert_eq!(msg.finish_reason.as_deref(), Some("stop"));
        // token 五字段自事件 payload 取值（payload 即计费权威）
        assert_eq!(msg.prompt_tokens, 20);
        assert_eq!(msg.completion_tokens, 10);
        assert_eq!(msg.reasoning_tokens, 5);
        assert_eq!(msg.cached_tokens, 2);
        // 计费归属模型写入 model_id；价格表未注册该模型时 cost 为 0（费用数学归 session crate 测试）
        assert_eq!(msg.model_id.as_deref(), Some("test/m1"));
        assert_eq!(msg.cost, 0.0);
    }

    #[test]
    fn unbilled_assistant_skips_model_and_cost() {
        // 中断补发路径：token 全 0，不计费、不填 model_id
        let ev = make_assistant_event(&|p| {
            p.content = Some("部分回复".into());
            p.finish_reason = Some("interrupted".into());
        });
        let paths = AgentPaths::default();
        let msg = event_to_message(&ev, None, &paths).expect("应投影成功");
        assert_eq!(msg.finish_reason.as_deref(), Some("interrupted"));
        assert_eq!(msg.model_id, None);
        assert_eq!(msg.cost, 0.0);
    }

    #[test]
    fn assistant_both_content_and_tool_calls_empty_fills_empty_string() {
        // 模型仅产出思考内容即被中断：content 与 tool_calls 双空。
        // OpenAI 协议要求 assistant 消息二者至少其一存在，落库前补空串
        // content 保证历史数据协议合法；reasoning 原样保留
        let ev = make_assistant_event(&|p| {
            p.reasoning = Some("思考到一半".into());
            p.finish_reason = Some("interrupted".into());
        });
        let paths = AgentPaths::default();
        let msg = event_to_message(&ev, None, &paths).expect("应投影成功");
        assert_eq!(msg.content.as_deref(), Some(""));
        assert!(msg.tool_calls.is_none());
        assert_eq!(msg.reasoning.as_deref(), Some("思考到一半"));
    }

    #[test]
    fn assistant_content_none_with_tool_calls_untouched() {
        // 纯工具调用消息：content=None + tool_calls 非空是合法形态，不干预
        let ev = make_assistant_event(&|p| {
            p.finish_reason = Some("tool_calls".into());
            p.tool_calls = Some(vec![ToolCallPayload {
                tool_call_id: "call_1".into(),
                tool_name: "search".into(),
                tool_args: serde_json::json!({"q": "rust"}),
            }]);
        });
        let paths = AgentPaths::default();
        let msg = event_to_message(&ev, None, &paths).expect("应投影成功");
        assert_eq!(msg.content, None);
        assert!(msg.tool_calls.is_some());
    }

    #[test]
    fn assistant_tool_calls_payload_maps_to_typed() {
        let ev = make_assistant_event(&|p| {
            p.finish_reason = Some("tool_calls".into());
            p.tool_calls = Some(vec![
                ToolCallPayload {
                    tool_call_id: "call_1".into(),
                    tool_name: "search".into(),
                    tool_args: serde_json::json!({"q": "rust"}),
                },
                ToolCallPayload {
                    tool_call_id: "call_2".into(),
                    tool_name: "read".into(),
                    tool_args: serde_json::json!({"path": "/a.rs"}),
                },
            ]);
        });
        let paths = AgentPaths::default();
        let msg = event_to_message(&ev, None, &paths).expect("应投影成功");
        let calls = msg.tool_calls.expect("应落库 tool_calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "search");
        // arguments 是合法 JSON 字符串，内容与 payload tool_args 语义一致
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&calls[0].arguments).ok(),
            Some(serde_json::json!({"q": "rust"}))
        );
        assert_eq!(calls[1].name, "read");
    }

    #[test]
    fn tool_result_maps_with_tool_name() {
        let ev = OutputEvent::ToolResult(ToolResultMessage {
            base: EventBase::default(),
            payload: ToolResultPayload {
                tool_call_id: "call_1".into(),
                tool_name: "get_weather".into(),
                content: "sunny".into(),
            },
        });
        let paths = AgentPaths::default();
        let msg = event_to_message(&ev, None, &paths).expect("应投影成功");
        assert_eq!(msg.role, MessageRole::Tool);
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_1"));
        // tool_name 随消息落库：回放侧据此还原工具名
        assert_eq!(msg.tool_name.as_deref(), Some("get_weather"));
        assert_eq!(msg.content.as_deref(), Some("sunny"));
    }

    #[test]
    fn user_event_maps_images_verbatim() {
        // 事件图片已由 inject_user_messages 预写节流结果，映射器原样落库
        let ev = OutputEvent::User(OutputUserMessage {
            base: EventBase::default(),
            payload: UserPayload {
                content: "看图".into(),
                images: vec![ImageContent {
                    mime_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                }],
                mode: UserMessageMode::Guide,
                source: UserMessageSource::User,
            },
        });
        let paths = AgentPaths::default();
        let msg = event_to_message(&ev, None, &paths).expect("应投影成功");
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content.as_deref(), Some("看图"));
        assert_eq!(msg.images.len(), 1);
    }

    #[test]
    fn non_history_event_variants_map_to_none() {
        // Chunk / Error / Interrupt 通知等不进历史：返回 None（事件照常发送）
        let paths = AgentPaths::default();
        let ev = OutputEvent::Interrupt(fuyao_api::message::output::InterruptMessage {
            base: EventBase::default(),
            payload: fuyao_api::message::output::InterruptPayload::new(
                "用户取消",
                fuyao_api::InterruptSource::User,
            ),
        });
        assert!(event_to_message(&ev, None, &paths).is_none());
    }

    #[test]
    fn append_placeholder_covers_text_and_image_only() {
        // 有文本：追加在文末
        assert_eq!(
            append_placeholder("看图", "[占位]"),
            "看图\n\n[占位]".to_string()
        );
        // 纯图（空文本）：占位文本即全文
        assert_eq!(append_placeholder("  ", "[占位]"), "[占位]".to_string());
    }

    // ── 双向往返：正反映射互逆 ────────────────────────────────

    #[test]
    fn tool_calls_roundtrip_preserves_semantics() {
        // 事件扁平 → 落库嵌套 → 回放扁平：三个字段语义不变
        let ev = make_assistant_event(&|p| {
            p.content = Some("调用工具".into());
            p.tool_calls = Some(vec![ToolCallPayload {
                tool_call_id: "call_1".into(),
                tool_name: "search".into(),
                tool_args: serde_json::json!({"q": "rust"}),
            }]);
        });
        let paths = AgentPaths::default();
        let msg = event_to_message(&ev, Some("test/m1"), &paths).expect("应投影成功");
        let replayed = message_to_event(&msg).expect("应投影回事件");
        let OutputEvent::Assistant(m) = replayed else {
            panic!("应为 Assistant 变体");
        };
        let calls = m.payload.tool_calls.expect("回放应有 tool_calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_call_id, "call_1");
        assert_eq!(calls[0].tool_name, "search");
        assert_eq!(calls[0].tool_args, serde_json::json!({"q": "rust"}));
    }

    // ── 反向映射：message_to_event（历史回放） ─────────────────

    #[test]
    fn user_message_projects_with_default_mode_source() {
        let msg = make_msg(MessageRole::User, 1, &|m| {
            m.content = Some("你好".into());
        });

        let event = message_to_event(&msg).expect("user 应投影成事件");
        let OutputEvent::User(OutputUserMessage { base, payload }) = event else {
            panic!("应为 User 变体，实际：{event:?}");
        };
        assert_eq!(payload.content, "你好");
        // mode / source 未持久化，兜底成 Guide / User
        assert_eq!(payload.mode, UserMessageMode::Guide);
        assert_eq!(payload.source, UserMessageSource::User);
        // base 复刻原消息时间戳与会话标识
        assert!((base.timestamp - 1.0).abs() < f64::EPSILON);
        assert_eq!(base.session_id.as_deref(), Some("sess-1"));
        // seq 直接来自 Message，与实时事件同源
        assert_eq!(base.seq, Some(1));
    }

    #[test]
    fn user_message_carries_images() {
        let img = ImageContent {
            mime_type: "image/png".into(),
            data: "iVBORw0".into(),
        };
        let msg = make_msg(MessageRole::User, 2, &|m| {
            m.images = vec![img.clone()];
        });

        let OutputEvent::User(OutputUserMessage { payload, .. }) =
            message_to_event(&msg).expect("user 应投影")
        else {
            panic!("变体类型不符");
        };
        assert_eq!(payload.images.len(), 1);
        assert_eq!(payload.images[0].mime_type, "image/png");
    }

    #[test]
    fn user_empty_content_defaults_to_empty_string() {
        // content = None 时 UserPayload.content 兜底空串（而非 panic）
        let msg = make_msg(MessageRole::User, 3, &|_| {});
        let OutputEvent::User(OutputUserMessage { payload, .. }) =
            message_to_event(&msg).expect("user 应投影")
        else {
            panic!("变体类型不符");
        };
        assert_eq!(payload.content, "");
    }

    #[test]
    fn assistant_message_projects_tokens_and_content() {
        let msg = make_msg(MessageRole::Assistant, 4, &|m| {
            m.content = Some("回复".into());
            m.reasoning = Some("思考".into());
            m.finish_reason = Some("stop".into());
            m.prompt_tokens = 20;
            m.completion_tokens = 10;
            m.reasoning_tokens = 5;
            m.cached_tokens = 2;
        });

        let OutputEvent::Assistant(AssistantMessage { payload, .. }) =
            message_to_event(&msg).expect("assistant 应投影")
        else {
            panic!("变体类型不符");
        };
        assert_eq!(payload.content.as_deref(), Some("回复"));
        assert_eq!(payload.reasoning.as_deref(), Some("思考"));
        assert_eq!(payload.finish_reason.as_deref(), Some("stop"));
        assert_eq!(payload.prompt_tokens, 20);
        assert_eq!(payload.completion_tokens, 10);
        // total_tokens = prompt + completion 近似
        assert_eq!(payload.total_tokens, 30);
        assert_eq!(payload.reasoning_tokens, 5);
        assert_eq!(payload.cached_tokens, 2);
        // 无 tool_calls
        assert!(payload.tool_calls.is_none());
    }

    #[test]
    fn assistant_tool_calls_typed_to_flat() {
        // 落库形态：typed 直存，arguments 是 JSON 字符串
        let tool_calls = vec![
            ToolCallData {
                id: "call_1".into(),
                name: "search".into(),
                arguments: "{\"q\":\"rust\"}".into(),
            },
            ToolCallData {
                id: "call_2".into(),
                name: "read".into(),
                arguments: "{\"path\":\"/a.rs\"}".into(),
            },
        ];
        let msg = make_msg(MessageRole::Assistant, 5, &|m| {
            m.tool_calls = Some(tool_calls.clone());
        });

        let OutputEvent::Assistant(AssistantMessage { payload, .. }) =
            message_to_event(&msg).expect("assistant 应投影")
        else {
            panic!("变体类型不符");
        };
        let calls = payload.tool_calls.expect("应有 tool_calls");
        assert_eq!(calls.len(), 2);

        assert_eq!(calls[0].tool_call_id, "call_1");
        assert_eq!(calls[0].tool_name, "search");
        // arguments 字符串应解析成 JSON 对象
        assert_eq!(calls[0].tool_args, serde_json::json!({"q": "rust"}));

        assert_eq!(calls[1].tool_call_id, "call_2");
        assert_eq!(calls[1].tool_name, "read");
        assert_eq!(calls[1].tool_args, serde_json::json!({"path": "/a.rs"}));
    }

    #[test]
    fn assistant_invalid_arguments_string_falls_back_to_null() {
        // arguments 非 JSON（外部脏数据）→ tool_args 兜底 Null，不阻断回放
        let msg = make_msg(MessageRole::Assistant, 6, &|m| {
            m.tool_calls = Some(vec![ToolCallData {
                id: "call_x".into(),
                name: "bad".into(),
                arguments: "not-json".into(),
            }]);
        });

        let OutputEvent::Assistant(AssistantMessage { payload, .. }) =
            message_to_event(&msg).expect("assistant 应投影")
        else {
            panic!("变体类型不符");
        };
        let call = &payload.tool_calls.expect("应有 tool_calls")[0];
        assert_eq!(call.tool_name, "bad");
        assert_eq!(call.tool_args, serde_json::Value::Null);
    }

    #[test]
    fn assistant_empty_tool_calls_array_becomes_none() {
        // 空数组投影成 None（而非空 Vec），与流式事件「无工具调用」语义一致
        let msg = make_msg(MessageRole::Assistant, 8, &|m| {
            m.tool_calls = Some(vec![]);
        });
        let OutputEvent::Assistant(AssistantMessage { payload, .. }) =
            message_to_event(&msg).expect("assistant 应投影")
        else {
            panic!("变体类型不符");
        };
        assert!(payload.tool_calls.is_none());
    }

    #[test]
    fn tool_message_projects_to_tool_result_with_name() {
        let msg = make_msg(MessageRole::Tool, 9, &|m| {
            m.tool_call_id = Some("call_1".into());
            m.tool_name = Some("get_weather".into());
            m.content = Some("sunny".into());
        });

        let OutputEvent::ToolResult(ToolResultMessage { base, payload }) =
            message_to_event(&msg).expect("tool 应投影")
        else {
            panic!("变体类型不符");
        };
        assert_eq!(payload.tool_call_id, "call_1");
        assert_eq!(payload.tool_name, "get_weather");
        assert_eq!(payload.content, "sunny");
        assert_eq!(base.seq, Some(9));
    }

    #[test]
    fn tool_message_missing_fields_default_empty() {
        // tool_call_id / tool_name / content 缺失时兜底空串，不 panic
        let msg = make_msg(MessageRole::Tool, 10, &|_| {});
        let OutputEvent::ToolResult(ToolResultMessage { payload, .. }) =
            message_to_event(&msg).expect("tool 应投影")
        else {
            panic!("变体类型不符");
        };
        assert_eq!(payload.tool_call_id, "");
        assert_eq!(payload.tool_name, "");
        assert_eq!(payload.content, "");
    }

    #[test]
    fn system_message_is_skipped() {
        // system 无对应事件变体，投影成 None
        let msg = make_msg(MessageRole::System, 11, &|m| {
            m.content = Some("你是助手".into());
        });
        assert!(message_to_event(&msg).is_none());
    }

    #[test]
    fn messages_to_events_reverses_desc_to_asc() {
        // 存储默认 seq 倒序（最新在前）；批量投影应翻成正序（旧在前）。
        // 用 base.seq 断言真实顺序——两端类型相同也能区分，
        // 避免此前「User→Assistant→User 类型序列翻不翻转都成立」的无效断言。
        let messages = vec![
            make_msg(MessageRole::User, 3, &|m| {
                m.content = Some("三".into());
            }),
            make_msg(MessageRole::Assistant, 2, &|m| {
                m.content = Some("二".into());
            }),
            make_msg(MessageRole::User, 1, &|m| {
                m.content = Some("一".into());
            }),
        ];

        let events = messages_to_events(messages);
        assert_eq!(events.len(), 3);
        // 正序：seq 1 → 2 → 3，用 base.seq 锁死顺序
        let seqs: Vec<Option<i64>> = events
            .iter()
            .map(|e| match e {
                OutputEvent::User(m) => m.base.seq,
                OutputEvent::Assistant(m) => m.base.seq,
                _ => None,
            })
            .collect();
        assert_eq!(seqs, vec![Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn messages_to_events_skips_system() {
        // system 在批量投影中被过滤
        let messages = vec![
            make_msg(MessageRole::System, 2, &|_| {}),
            make_msg(MessageRole::User, 1, &|m| {
                m.content = Some("用户".into());
            }),
        ];

        let events = messages_to_events(messages);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], OutputEvent::User(_)));
    }

    #[test]
    fn messages_to_events_empty_input() {
        assert!(messages_to_events(vec![]).is_empty());
    }
}
