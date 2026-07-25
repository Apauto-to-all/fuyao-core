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
use fuyao_api::ModelConfig;
use fuyao_api::message::output::{AssistantPayload, ToolCallMessage, ToolCallPayload};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_provider::{ChatMessage, ChatRequest, StreamOptions, ToolCallData};
use fuyao_session::SessionStore;

/// 解析后的模型信息（一轮 ReAct 用）
///
/// `provider_id` 用于从 `ProviderRegistry` 查 Provider 实例；`model` 是裸模型名，
/// 喂给 `provider.stream_chat`。两者从 `ModelConfig.model_id`（形如
/// `"provider_id/model_id"`）拆分而来——model_id=None 时回退 `[models.default]`。
#[derive(Debug)]
pub(crate) struct ResolvedModel {
    /// Provider ID（小写，给 ProviderRegistry.get 用）
    pub provider_id: String,
    /// 裸模型名（给 provider.stream_chat 用，不带 provider_id 前缀）
    pub model: String,
    /// 流式选项
    pub options: StreamOptions,
}

/// 从 DB 加载可见消息凑 ChatRequest
///
/// 系统提示词单独填 request.system（不进 messages 数组），
/// messages 只装 user/assistant/tool 对话历史。
///
/// **事件级落库模式下消息不在内存**：每次构造 ChatRequest 都从 DB 查询
/// 可见窗口（走 `idx_messages_session_seq` 索引，毫秒级）。
///
/// **配对兜底**：OpenAI/Anthropic 协议要求每个 assistant 的 tool_call 都有对应的
/// tool 结果消息。被拦截 Block、中断的工具调用不会有结果——这里在拼消息时
/// 为缺结果的 tool_call 补一条 error tool_result（content 标记中断）。
pub(crate) async fn build_chat_request(
    store: &SessionStore,
    session_id: &str,
    system_prompt: Option<&str>,
) -> ChatRequest {
    let history = match store.load_visible_messages(session_id).await {
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

    // 先收集所有已有 tool 结果的 tool_call_id（用于配对检查）
    let mut answered_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for m in &history {
        if m.role == "tool"
            && let Some(id) = &m.tool_call_id
        {
            answered_ids.insert(id.as_str());
        }
    }

    let mut messages = Vec::with_capacity(history.len());
    for m in &history {
        messages.push(ChatMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            reasoning: m.reasoning.clone(),
            tool_calls: m.tool_calls.as_ref().and_then(|tc| tc.as_array().cloned()),
            tool_call_id: m.tool_call_id.clone(),
            tool_name: m.tool_name.clone(),
        });

        // assistant 消息后：为缺结果的 tool_call 补 error tool_result
        if m.role == "assistant"
            && let Some(tool_calls) = m.tool_calls.as_ref().and_then(|tc| tc.as_array())
        {
            for tc in tool_calls {
                let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if !id.is_empty() && !answered_ids.contains(id) {
                    // 缺结果：补 error tool_result
                    messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: Some("[工具执行被拦截或中断]".to_string()),
                        reasoning: None,
                        tool_calls: None,
                        tool_call_id: Some(id.to_string()),
                        tool_name: tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    });
                }
            }
        }
    }

    ChatRequest {
        messages,
        system: system_prompt.map(String::from),
    }
}

/// 从 ModelConfig 解析本轮模型信息
///
/// model_id 解析顺序：
/// 1. **`ModelConfig.model_id = Some("provider_id/model_id")`**：直接拆分
/// 2. **`ModelConfig.model_id = None`**：读全局 `[models.default]` 配置兜底
///    - 配了 `[models.default]` → 用它的 `model` 字段（同样是 `"provider_id/model_id"` 格式）
///    - 没配 → 返回 `Err`（fail-loud：用户必须显式指定或配 default，引擎不猜）
///
/// **model_id 格式必须是 `"provider_id/model_id"`**——不带 `/` 视为格式错误返回 `Err`。
/// 这与 provider_id 路由契约一致（ProviderRegistry 按 provider_id 查实例）。
///
/// 思考控制参数（thinking_type / reasoning_effort）透传给 `StreamOptions`。
/// 工具定义从 registry 序列化（非空时带 tools 字段）。
///
/// 返回的 `ResolvedModel` 由调用方（turn.rs）继续从 `ctx.providers.get(provider_id)`
/// 查 Provider 实例——本函数不查 registry（保持纯构造器职责，与 IO 解耦）。
pub(crate) fn resolve_model(
    model_config: &ModelConfig,
    tools: &ToolRegistry,
) -> Result<ResolvedModel, String> {
    // 1. 确定 model_id 字符串：显式指定 → 用它；None → 读 [models.default]
    let model_id: String = match model_config.model_id.as_deref() {
        Some(id) => id.to_string(),
        None => {
            // 读全局配置（已由 init 阶段加载进 get_config）
            let default_ref = fuyao_api::get_config()
                .models
                .default
                .as_ref()
                .map(|r| r.model.clone())
                .filter(|s| !s.is_empty());
            match default_ref {
                Some(id) => id,
                None => {
                    return Err(
                        "未指定模型：ModelConfig.model_id 为空且未配置 [models.default]"
                            .to_string(),
                    );
                }
            }
        }
    };

    // 2. 拆 "provider_id/model_id" 格式
    let (provider_id, model) = match model_id.split_once('/') {
        Some((p, m)) if !p.is_empty() && !m.is_empty() => (p.to_lowercase(), m.to_string()),
        _ => {
            return Err(format!(
                "model_id 格式错误（应为 provider_id/model_id）: {model_id}"
            ));
        }
    };

    // 3. 构造 StreamOptions
    let tool_defs = tools.definitions_json();
    let options = StreamOptions {
        temperature: None,
        tools: if tool_defs.is_empty() {
            None
        } else {
            Some(tool_defs)
        },
        tool_choice: None,
        thinking_type: model_config.thinking_type.clone(),
        reasoning_effort: model_config.reasoning_effort.clone(),
    };

    Ok(ResolvedModel {
        provider_id,
        model,
        options,
    })
}

/// 从流式结果构建 AssistantPayload（无工具调用，最终回复事件）
pub(crate) fn assistant_msg_to_payload(result: &StreamResult) -> AssistantPayload {
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
        tool_calls: None,
        finish_reason: Some("stop".to_string()),
        completion_tokens: result.usage.completion_tokens as i64,
        prompt_tokens: result.usage.prompt_tokens as i64,
        total_tokens: result.usage.total_tokens as i64,
        reasoning_tokens: result.usage.completion_reasoning_tokens.unwrap_or(0) as i64,
        cached_tokens: result.usage.prompt_cached_tokens.unwrap_or(0) as i64,
    }
}

/// 从流式结果构建含 tool_calls 的 AssistantPayload（工具调用事件）
pub(crate) fn assistant_with_tool_calls_to_payload(result: &StreamResult) -> AssistantPayload {
    let tool_call_payloads: Vec<ToolCallPayload> = result
        .tool_calls
        .iter()
        .map(|tc| ToolCallPayload {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            tool_args: serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null),
        })
        .collect();

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
        tool_calls: Some(tool_call_payloads),
        finish_reason: Some("tool_calls".to_string()),
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
        let mut session = Session::new(None, Some("系统提示词".to_string()));
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

        let request =
            build_chat_request(&store, &session.id, session.system_prompt.as_deref()).await;

        // 应有：user + assistant + 1 真实结果 + 2 补充 error 结果 = 5 条
        let tool_msgs: Vec<_> = request
            .messages
            .iter()
            .filter(|m| m.role == "tool")
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

        let request =
            build_chat_request(&store, &session.id, session.system_prompt.as_deref()).await;
        let tool_count = request.messages.iter().filter(|m| m.role == "tool").count();
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

        let request =
            build_chat_request(&store, &session.id, session.system_prompt.as_deref()).await;
        assert_eq!(request.messages.len(), 2, "无工具调用时消息数不变");
    }

    // ===== resolve_model 单测 =====
    //
    // set_config 是 OnceLock（只能 set 一次），单元测试不能 set；这里覆盖默认状态
    // （get_config 返回 FuyaoConfig::default()，其中 models.default = None）。
    // "None + 配 [models.default] → 用 default" 的正向用例由 fuyao-app 集成测试覆盖
    // （集成测试是独立二进制，OnceLock 不串扰）。

    fn empty_registry() -> ToolRegistry {
        ToolRegistryBuilder::default().build()
    }

    fn params_with_model(model_id: Option<&str>) -> ModelConfig {
        ModelConfig {
            model_id: model_id.map(String::from),
            thinking_type: None,
            reasoning_effort: None,
        }
    }

    #[test]
    fn resolve_model_explicit_id_splits_provider_and_model() {
        let tools = empty_registry();
        let params = params_with_model(Some("DeepSeek/deepseek-v4-flash"));
        let r = resolve_model(&params, &tools).expect("显式 model_id 应解析成功");
        // provider_id 小写化
        assert_eq!(r.provider_id, "deepseek");
        // model 保持原样
        assert_eq!(r.model, "deepseek-v4-flash");
    }

    #[test]
    fn resolve_model_none_without_default_returns_err() {
        // 默认状态：未 set_config，get_config 返回 default（models.default = None）
        let tools = empty_registry();
        let params = params_with_model(None);
        let err = resolve_model(&params, &tools).expect_err("无 default 应返回 Err");
        assert!(err.contains("未指定模型"), "错误信息应明确：{err}");
        assert!(
            err.contains("[models.default]"),
            "错误信息应指引配置项：{err}"
        );
    }

    #[test]
    fn resolve_model_invalid_format_no_slash_returns_err() {
        let tools = empty_registry();
        let params = params_with_model(Some("invalid-no-slash"));
        let err = resolve_model(&params, &tools).expect_err("格式错误应返回 Err");
        assert!(err.contains("格式错误"), "错误信息应明确：{err}");
    }

    #[test]
    fn resolve_model_empty_provider_or_model_returns_err() {
        let tools = empty_registry();
        // "/model" — provider 空
        let params = params_with_model(Some("/model"));
        resolve_model(&params, &tools).expect_err("provider 空应报错");
        // "provider/" — model 空
        let params = params_with_model(Some("provider/"));
        resolve_model(&params, &tools).expect_err("model 空应报错");
    }
}
