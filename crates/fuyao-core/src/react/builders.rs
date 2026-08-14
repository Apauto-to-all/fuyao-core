//! 消息/请求构造器
//!
//! 从 ReAct 循环抽出的纯构造逻辑：
//! - [`build_chat_request`]：从 DB 加载可见消息凑 ChatRequest（async，走 store 查询）
//! - [`resolve_model`]：从 ModelConfig 解析 model + provider_id + StreamOptions
//! - 各类 Assistant Payload 构造器（事件用）
//! - 工具调用拦截回灌用的双向转换函数
//! - [`fill_assistant_message_usage_and_cost`]：填 assistant Message 的 token + cost 字段
//!
//! 注：Message 主体构造由 turn.rs 的 emit_to_history 闭包内联完成（因每种事件的字段映射不同，
//! 集中成 trait 反而过度抽象）。本模块只留 payload 构造、请求构造与 token/cost 填充。

use crate::stream::StreamResult;
use crate::tool_registry::ToolRegistry;
use fuyao_api::message::output::{
    AssistantPayload, ToolCallMessage, ToolCallPayload, extract_id_name_pairs,
};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_api::{AgentPaths, MessageRole, ModelConfig};
use fuyao_provider::{ChatMessage, ChatRequest, StreamOptions, ToolCallData};
use fuyao_session::SessionStore;
use std::collections::HashMap;

/// 解析后的模型信息（一轮 ReAct 用）
///
/// `provider_id` 用于从 `ProviderRegistry` 查 Provider 实例；`model` 是裸模型名，
/// 喂给 `provider.stream_chat`。`model_id` 是完整 `"provider_id/model_id"` 串，
/// 供调用方写回 session（见 turn.rs 物化逻辑）+ 落进 assistant 消息 + 喂费用计算。
/// 三者从 `ModelConfig.model_id` 拆分而来（model_id 必须非空，空值在 resolve_model 即报错）。
///
/// `context_length` 一并解析进来——主对话 keep_tokens 预算、压缩阈值门、中断补发
/// 三处消费点共用一份解析（`None` 表示模型未注册、上下文长度未知），
/// 调用方直接读 `resolved.context_length`，口径天然一致。
#[derive(Debug)]
pub(crate) struct ResolvedModel {
    /// 完整模型 ID（`"provider_id/model_id"` 形，写回 session / 落库 / 计费用）
    pub model_id: String,
    /// Provider ID（小写，给 ProviderRegistry.get 用）
    pub provider_id: String,
    /// 裸模型名（给 provider.stream_chat 用，不带 provider_id 前缀）
    pub model: String,
    /// 流式选项（思考参数取自 session 的 ModelConfig）
    pub options: StreamOptions,
    /// 模型上下文长度（查注册表 `model.limit.context`，查不到为 `None`）
    ///
    /// `None` 表示模型未注册（注册表无此条目）——不编造任何数字，由各消费点
    /// 自行决定降级语义：keep_tokens 预算按 0、压缩判定直接跳过；
    /// 该模型的 turn 调用自会在 provider 调用处失败，无需此处兜底。
    /// keep_tokens 预算、压缩阈值门等各消费点共用一份，避免散算漂移。
    pub context_length: Option<u32>,
}

/// 从 DB 加载可见消息凑 ChatRequest
///
/// 系统提示词单独填 request.system（不进 messages 数组），
/// messages 只装 user/assistant/tool 对话历史。
///
/// **DB 唯一数据源**：消息与 system_prompt 均从 DB 现查——事件级落库模式下消息
/// 不进内存，system_prompt 也不缓存（压缩重建后经 `update_system_prompt` 落库，
/// 这里现读即最新值）。
///
/// **配对兜底**：OpenAI/Anthropic 协议要求每个 assistant 的 tool_call 都有对应的
/// tool 结果消息。被拦截 Block、中断的工具调用不会有结果——这里在拼消息时
/// 为缺结果的 tool_call 补一条 error tool_result（content 标记中断）。
pub(crate) async fn build_chat_request(
    store: &SessionStore,
    session_id: &str,
    keep_tokens: usize,
) -> ChatRequest {
    let history = match store.load_visible_messages(session_id, keep_tokens).await {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::warn!(
                session_id = session_id,
                cause = %e,
                "加载可见消息失败，本轮 LLM 调用将看到空历史"
            );
            Vec::new()
        }
    };

    // system_prompt 从 DB 现读（压缩重建后已落库，这里读到的是最新值）
    let system_prompt: Option<String> = match store.get(session_id).await {
        Ok(Some(session)) => session.system_prompt,
        Ok(None) => {
            tracing::warn!(
                session_id = session_id,
                "session 行不存在，system_prompt 取空"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                session_id = session_id,
                cause = %e,
                "读 session.system_prompt 失败，本轮按空系统提示词继续"
            );
            None
        }
    };

    // 先收集所有已有 tool 结果的 tool_call_id（用于配对检查）
    let mut answered_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for m in &history {
        if matches!(m.role, MessageRole::Tool)
            && let Some(id) = &m.tool_call_id
        {
            answered_ids.insert(id.as_str());
        }
    }

    let mut messages = Vec::with_capacity(history.len());
    for m in &history {
        messages.push(ChatMessage {
            role: m.role,
            content: m.content.clone(),
            images: m.images.clone(),
            reasoning: m.reasoning.clone(),
            tool_calls: m.tool_calls.as_ref().and_then(|tc| tc.as_array().cloned()),
            tool_call_id: m.tool_call_id.clone(),
            tool_name: m.tool_name.clone(),
        });

        // assistant 消息后：为缺结果的 tool_call 补 error tool_result
        if matches!(m.role, MessageRole::Assistant)
            && let Some(tool_calls) = m.tool_calls.as_ref()
        {
            // schema 解析集中到 extract_id_name_pairs，此处不再硬编码字段名
            for (id, name) in extract_id_name_pairs(tool_calls) {
                if !id.is_empty() && !answered_ids.contains(id.as_str()) {
                    // 缺结果：补 error tool_result
                    messages.push(ChatMessage {
                        role: MessageRole::Tool,
                        content: Some("[工具执行被拦截或中断]".to_string()),
                        images: vec![],
                        reasoning: None,
                        tool_calls: None,
                        tool_call_id: Some(id),
                        tool_name: Some(name),
                    });
                }
            }
        }
    }

    ChatRequest {
        messages,
        system: system_prompt,
    }
}

/// 解析模型上下文长度：查注册表 `model.limit.context`
///
/// 集中此片段，供 [`resolve_model`]（首轮解析）与压缩侧 / 中断补发侧等独立调用点共用。
/// 返回 `None` 表示模型未注册（注册表查不到该条目）——不编造任何数字，由调用方
/// 决定降级语义（压缩判定跳过 / keep_tokens 预算按 0）。`context` 为 0 的条目
/// 视同未声明（配置加载层已保证 toml 声明的 context 必为正整数，0 只可能来自
/// 程序化注册的残缺条目），同样返回 `None`。
/// `get_model` 查全局静态缓存（非 IO），本函数保持纯计算。
pub(crate) fn resolve_context_length(model_id: &str, agent_paths: &AgentPaths) -> Option<u32> {
    fuyao_provider::get_model(model_id, agent_paths)
        .map(|m| m.limit.context)
        .filter(|context| *context > 0)
}

/// 从 ModelConfig 解析本轮模型信息
///
/// model_id 必须是 `"provider_id/model_id"` 格式的非空串——空串或缺少 `/` 视为
/// 未指定 / 格式错误，返回 `Err`（fail-loud：引擎不提供任何隐式兜底模型）。
/// 这与 provider_id 路由契约一致（ProviderRegistry 按 provider_id 查实例）。
///
/// 思考参数（thinking_type / reasoning_effort）直接取自 session 的 ModelConfig，
/// 与 model_id 同束传递——thinking 即便 None 也算数（= 该模型自身默认行为）。
///
/// 结果进 `options`，调用方（turn.rs）据此写回 session 物化（见写回逻辑），保证
/// DB 消息 / 费用 / 标题三处消费点都能读到实际生效值。
///
/// `context_length` 一并由 [`resolve_context_length`] 算出填进返回值——模型未注册
/// 时为 `None`（不编造数字，各消费点从返回值取同一份口径，无需自行解析）。
///
/// 工具定义按 `is_child`（递归防护）+ `definition_tools`（定义层收窄）双重过滤后序列化，
/// 两者取交集。
///
/// 返回的 `ResolvedModel` 由调用方（turn.rs）继续从 `ctx.providers.get(provider_id)`
/// 查 Provider 实例——本函数不查 registry（保持纯构造器职责，与 IO 解耦）。
pub(crate) fn resolve_model(
    model_config: &ModelConfig,
    tools: &ToolRegistry,
    is_child: bool,
    definition_tools: &HashMap<String, bool>,
    agent_paths: &AgentPaths,
) -> Result<ResolvedModel, String> {
    // model_id 必须非空——空串视为未指定（引擎不提供隐式兜底模型，调用方必须显式给出）
    let model_id: &str = &model_config.model_id;
    if model_id.is_empty() {
        return Err("未指定模型：ModelConfig.model_id 为空".to_string());
    }

    // 拆 "provider_id/model_id" 格式
    let (provider_id, model) = match model_id.split_once('/') {
        Some((p, m)) if !p.is_empty() && !m.is_empty() => (p.to_lowercase(), m.to_string()),
        _ => {
            return Err(format!(
                "model_id 格式错误（应为 provider_id/model_id）: {model_id}"
            ));
        }
    };

    // 思考参数直接取自 session 的 ModelConfig（thinking 即便 None 也算数 = 模型自身默认）
    let thinking_type = model_config.thinking_type.clone();
    let reasoning_effort = model_config.reasoning_effort.clone();

    // 构造 StreamOptions（工具定义按 is_child + definition_tools 过滤——递归防护 + 定义层收窄）
    let tool_defs = tools.definitions_json_for(is_child, definition_tools);
    let options = StreamOptions {
        tools: if tool_defs.is_empty() {
            None
        } else {
            Some(tool_defs)
        },
        thinking_type,
        reasoning_effort,
    };

    // context_length 与主对话 keep_tokens 预算、压缩阈值门共用一份（集中此处解析）
    let context_length = resolve_context_length(model_id, agent_paths);

    Ok(ResolvedModel {
        model_id: model_id.to_string(),
        provider_id,
        model,
        options,
        context_length,
    })
}

/// 从流式结果构建 AssistantPayload
///
/// `result.tool_calls` 非空时构造工具调用 payload 并标记 `finish_reason="tool_calls"`，
/// 否则 `finish_reason="stop"`。最终回复与工具调用两条路径的 content / reasoning /
/// 五个 token 字段映射完全相同，原先复制在两个函数里；按 tool_calls 是否为空分叉即可。
pub(crate) fn assistant_payload(result: &StreamResult) -> AssistantPayload {
    let (tool_calls, finish_reason) = if result.tool_calls.is_empty() {
        (None, "stop")
    } else {
        let payloads: Vec<ToolCallPayload> = result
            .tool_calls
            .iter()
            .map(|tc| ToolCallPayload {
                tool_call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                tool_args: serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null),
            })
            .collect();
        (Some(payloads), "tool_calls")
    };

    AssistantPayload {
        content: if result.text.is_empty() {
            None
        } else {
            Some(result.text.clone())
        },
        reasoning: if result.reasoning.is_empty() {
            None
        } else {
            Some(result.reasoning.clone())
        },
        tool_calls,
        finish_reason: Some(finish_reason.to_string()),
        completion_tokens: result.usage.completion_tokens as i64,
        prompt_tokens: result.usage.prompt_tokens as i64,
        total_tokens: result.usage.total_tokens as i64,
        reasoning_tokens: result.usage.completion_reasoning_tokens.unwrap_or(0) as i64,
        cached_tokens: result.usage.prompt_cached_tokens.unwrap_or(0) as i64,
    }
}

// ===== 工具调用拦截回灌用的转换函数 =====
//
// 工具调用逐个经 dispatch_intercept 拦截后，需要从拦截后的 OutputEvent::ToolCall
// 提取出执行用的 ToolCallData（参数可能被插件修改），保证「执行 / 存储 / 发送」
// 三者数据一致（都以拦截后的 payload 为准）。
//
// 注：assistant Message 的 token + cost 字段填充归 fuyao-session 的 `fill_message_cost`
// 统一实现（所有费用计算集中在 session 模块，避免散落）。

/// 把单个工具调用数据构造成 ToolCall 输出事件（供逐个拦截用）
pub(crate) fn tool_call_data_to_event(tc: &ToolCallData) -> OutputEvent {
    OutputEvent::ToolCall(ToolCallMessage {
        base: EventBase::default(),
        payload: ToolCallPayload {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            tool_args: serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null),
        },
    })
}

/// 从拦截后的 ToolCall 事件提取执行用的工具调用数据
///
/// 插件可能修改了 tool_name / tool_args，这里以拦截后的 payload 为准构造 ToolCallData。
/// 参数序列化回 JSON 字符串（execute_tools 内部按字符串解析参数）。
pub(crate) fn tool_call_event_to_data(event: &OutputEvent) -> Option<ToolCallData> {
    if let OutputEvent::ToolCall(msg) = event {
        Some(ToolCallData {
            id: msg.payload.tool_call_id.clone(),
            name: msg.payload.tool_name.clone(),
            arguments: msg.payload.tool_args.to_string(),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolRegistryBuilder;
    use fuyao_api::{Message, Session};

    /// 构造带工具调用的 assistant Message
    fn assistant_with_calls(ids: &[&str]) -> Message {
        let tool_calls: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": "echo", "arguments": "{}"}
                })
            })
            .collect();
        let mut msg = Message::assistant(Some("调用工具".to_string()));
        msg.tool_calls = Some(serde_json::Value::Array(tool_calls));
        msg.finish_reason = Some("tool_calls".to_string());
        msg
    }

    /// 构造临时 SessionStore + session（事件级落库模式下 pairing 测试需要真实 DB）
    async fn temp_store_with_session() -> (fuyao_session::SessionStore, Session) {
        let dir = std::env::temp_dir()
            .join("fuyao_builders_test")
            .join(uuid::Uuid::new_v4().to_string());
        let store = fuyao_session::SessionStore::new(dir.join("test.db"))
            .await
            .expect("构造 SessionStore 失败");
        let mut session = Session::new(None, None, Some("系统提示词".to_string()));
        session.id = "test_session".to_string();
        store.create(&session).await.unwrap();
        (store, session)
    }

    /// 把消息落进 DB（事件级落库模式）
    async fn insert(store: &fuyao_session::SessionStore, session_id: &str, mut msg: Message) {
        store
            .insert_message(session_id, &mut msg)
            .await
            .expect("insert 失败");
    }

    #[tokio::test]
    async fn pairing_fills_missing_tool_results() {
        // assistant 调用 3 个工具，只有 1 个有结果 → 补 2 条 error tool_result
        let (store, session) = temp_store_with_session().await;
        insert(&store, &session.id, Message::user("问题".to_string())).await;
        insert(
            &store,
            &session.id,
            assistant_with_calls(&["c1", "c2", "c3"]),
        )
        .await;
        insert(
            &store,
            &session.id,
            Message::tool_result("c2".into(), "结果2".into()),
        )
        .await;

        let request = build_chat_request(&store, &session.id, usize::MAX).await;

        // 应有：user + assistant + 1 真实结果 + 2 补充 error 结果 = 5 条
        let tool_msgs: Vec<_> = request
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .collect();
        assert_eq!(tool_msgs.len(), 3, "应有 3 条 tool 消息（1真实+2补充）");

        // c1 和 c3 被补充
        let supplemented_ids: Vec<_> = tool_msgs
            .iter()
            .filter(|m| m.content.as_deref() == Some("[工具执行被拦截或中断]"))
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert_eq!(
            supplemented_ids,
            vec!["c1", "c3"],
            "c1 和 c3 应被补充 error"
        );
    }

    #[tokio::test]
    async fn pairing_no_op_when_all_answered() {
        // 所有 tool_call 都有结果 → 不补充
        let (store, session) = temp_store_with_session().await;
        insert(&store, &session.id, assistant_with_calls(&["c1", "c2"])).await;
        insert(
            &store,
            &session.id,
            Message::tool_result("c1".into(), "结果1".into()),
        )
        .await;
        insert(
            &store,
            &session.id,
            Message::tool_result("c2".into(), "结果2".into()),
        )
        .await;

        let request = build_chat_request(&store, &session.id, usize::MAX).await;
        let tool_count = request
            .messages
            .iter()
            .filter(|m| matches!(m.role, MessageRole::Tool))
            .count();
        assert_eq!(tool_count, 2, "全部有结果时不补充");
    }

    #[tokio::test]
    async fn pairing_ignores_assistant_without_tool_calls() {
        // 无工具调用的 assistant 消息不触发补充
        let (store, session) = temp_store_with_session().await;
        insert(&store, &session.id, Message::user("你好".to_string())).await;
        insert(
            &store,
            &session.id,
            Message::assistant(Some("你好".to_string())),
        )
        .await;

        let request = build_chat_request(&store, &session.id, usize::MAX).await;
        assert_eq!(request.messages.len(), 2, "无工具调用时消息数不变");
    }

    // ===== resolve_model 单测 =====
    //
    // resolve_model 是纯函数（不读全局配置），单元测试直接覆盖：
    // - 显式非空 model_id → 拆分 provider_id / model + 透传 thinking
    // - 空 model_id → Err（引擎不提供隐式兜底）
    // - 格式非法（无 /、provider 空、model 空）→ Err

    fn empty_registry() -> ToolRegistry {
        ToolRegistryBuilder::default().build()
    }

    fn params_with_model(model_id: &str) -> ModelConfig {
        ModelConfig {
            model_id: model_id.to_string(),
            thinking_type: None,
            reasoning_effort: None,
        }
    }

    #[test]
    fn resolve_model_explicit_id_splits_provider_and_model() {
        let tools = empty_registry();
        let params = params_with_model("DeepSeek/deepseek-v4-flash");
        let r = resolve_model(
            &params,
            &tools,
            false,
            &HashMap::new(),
            &AgentPaths::default(),
        )
        .expect("显式 model_id 应解析成功");
        // provider_id 小写化
        assert_eq!(r.provider_id, "deepseek");
        // model 保持原样
        assert_eq!(r.model, "deepseek-v4-flash");
        // model_id 完整串原样带回（供写回 / 落库 / 计费用）
        assert_eq!(r.model_id, "DeepSeek/deepseek-v4-flash");
    }

    #[test]
    fn resolve_model_carries_explicit_thinking_into_options() {
        // 显式 model_config 的 thinking_type / reasoning_effort 透传进 options
        let tools = empty_registry();
        let params = ModelConfig {
            model_id: "deepseek/deepseek-v4-flash".to_string(),
            thinking_type: Some(fuyao_api::ThinkingType::Enabled),
            reasoning_effort: Some("high".to_string()),
        };
        let r = resolve_model(
            &params,
            &tools,
            false,
            &HashMap::new(),
            &AgentPaths::default(),
        )
        .expect("解析应成功");
        assert_eq!(
            r.options.thinking_type,
            Some(fuyao_api::ThinkingType::Enabled)
        );
        assert_eq!(r.options.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn resolve_model_empty_model_id_returns_err() {
        // model_id 为空 → Err（引擎不提供隐式兜底模型）
        let tools = empty_registry();
        let params = params_with_model("");
        let err = resolve_model(
            &params,
            &tools,
            false,
            &HashMap::new(),
            &AgentPaths::default(),
        )
        .expect_err("空 model_id 应返回 Err");
        assert!(err.contains("未指定模型"), "错误信息应明确：{err}");
    }

    #[test]
    fn resolve_model_invalid_format_no_slash_returns_err() {
        let tools = empty_registry();
        let params = params_with_model("invalid-no-slash");
        let err = resolve_model(
            &params,
            &tools,
            false,
            &HashMap::new(),
            &AgentPaths::default(),
        )
        .expect_err("格式错误应返回 Err");
        assert!(err.contains("格式错误"), "错误信息应明确：{err}");
    }

    #[test]
    fn resolve_model_empty_provider_or_model_returns_err() {
        let tools = empty_registry();
        // "/model" — provider 空
        let params = params_with_model("/model");
        resolve_model(
            &params,
            &tools,
            false,
            &HashMap::new(),
            &AgentPaths::default(),
        )
        .expect_err("provider 空应报错");
        // "provider/" — model 空
        let params = params_with_model("provider/");
        resolve_model(
            &params,
            &tools,
            false,
            &HashMap::new(),
            &AgentPaths::default(),
        )
        .expect_err("model 空应报错");
    }
}
