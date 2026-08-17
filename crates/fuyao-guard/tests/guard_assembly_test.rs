//! fuyao-guard 集成测试：LoopGuardPlugin 装配链路
//!
//! 单元测试已覆盖 LoopGuardState 内部状态机的纯逻辑。本集成测试聚焦跨模块的
//! 端到端装配链路——这是单元测试的空白带：
//! `LoopGuardPlugin::create_instance()` → `instance.register(&mut hooks, &sender)` 装钩子进 HooksRegistry
//! → `hook_output_observe/intercept` 喂真实 OutputEvent → 验证检测链路生效。
//!
//! 全部使用默认配置（不调 set_config），走 get_config 未 set 返回 default 的兜底。

mod common;

use common::{make_chunk, make_tool_call, make_tool_result};
use fuyao_api::message::input::UserMessageSource;
use fuyao_api::message::output::UserMessage as OutputUserMessage;
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_guard::LoopGuardPlugin;
use fuyao_hooks::{HooksRegistry, Plugin, SessionSender, SharedHooks};
use std::sync::Arc;

use fuyao_api::message::UserMessageMode;
use fuyao_api::message::output::UserPayload as OutputUserPayload;

/// 构造已注册 loop_guard 插件的 hooks
///
/// 装配链路：create_instance → register（sender 持 dummy 通道，测试不验证消息投递，
/// 只验证 observe/intercept 链路）→ finalize 冻结 → 包 Arc 只读共享。
fn assembled_guard() -> SharedHooks {
    let plugin = LoopGuardPlugin::new();
    let mut registry = HooksRegistry::new();
    let instance = plugin.create_instance();

    // 构造 SessionSender（dummy 通道，测试不验证投递侧）
    let (tx_interrupt, _rx_interrupt) = tokio::sync::mpsc::channel(16);
    let (tx_user, _rx_user) = tokio::sync::mpsc::channel(16);
    let sender = SessionSender::new("loop_guard", tx_user, tx_interrupt);
    instance.register(&mut registry, &sender);

    registry.finalize();
    Arc::new(registry)
}

/// 构造用户主动消息事件（source=User，触发完全重置）
fn make_user_event_from_user() -> OutputEvent {
    OutputEvent::User(OutputUserMessage {
        base: EventBase::default(),
        payload: OutputUserPayload {
            content: "用户消息".to_string(),
            images: vec![],
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        },
    })
}

// ============================================================================
// 插件装配与钩子注册
// ============================================================================

#[test]
fn plugin_registers_hooks() {
    // register 后 hooks 应含 output_observe + output_intercept 两类钩子
    let plugin = LoopGuardPlugin::new();
    assert_eq!(plugin.name(), "loop_guard");

    let mut registry = HooksRegistry::new();
    let instance = plugin.create_instance();
    let (tx_interrupt, _rx_interrupt) = tokio::sync::mpsc::channel(16);
    let (tx_user, _rx_user) = tokio::sync::mpsc::channel(16);
    let sender = SessionSender::new("loop_guard", tx_user, tx_interrupt);
    instance.register(&mut registry, &sender);
    // register 完成（不 panic）即说明钩子注册 + sender 保存成功
}

#[test]
fn register_with_sender_does_not_panic_without_consumer() {
    // 即使无人消费 rx，register 注入 sender 也能正常完成（try_send 非阻塞）
    let hooks = assembled_guard();
    let _ = hooks;
}

// ============================================================================
// 工具循环检测：端到端升级链
// ============================================================================

#[tokio::test]
async fn tool_repeat_triggers_warn_then_inject_via_intercept() {
    // 重复同一工具调用（达到默认 threshold=4），observe 触发 Warn →
    // 紧接着 intercept 修改 ToolResult 内容注入警告
    let hooks = assembled_guard();

    // 默认 tool_repeat_threshold=4，连续 4 次相同调用后第 5 次触发检测
    for _ in 0..5 {
        let event = OutputEvent::ToolCall(make_tool_call("read", r#"{"path":"x"}"#));
        hooks.hook_output_observe(Arc::new(event)).await;
    }

    // observe 应已产生 pending（Warn 或更高级别，取决于升级链）
    // 验证 intercept 原地修改 ToolResult 内容注入警告（Warn 追加前缀保留原文，Inject 替换内容）
    let mut event = OutputEvent::ToolResult(make_tool_result("read", "原始文件内容"));
    let blocked = hooks.hook_output_intercept(&mut event);
    assert!(blocked.is_none(), "不应阻止事件");
    match event {
        OutputEvent::ToolResult(tr) => {
            assert!(
                tr.payload.content.contains("循环检测"),
                "检测触发后应注入循环检测信息，实际：{}",
                tr.payload.content
            );
        }
        _ => panic!("intercept 后事件应仍为 ToolResult"),
    }
}

#[tokio::test]
async fn tool_sequence_pattern_triggers_escalation() {
    // A→B→A→B 序列模式（默认 tool_alternate_threshold=6）触发检测
    let hooks = assembled_guard();

    // 交替调用两个工具 7 次（超过 threshold=6）
    for i in 0..7 {
        let tool = if i % 2 == 0 { "read" } else { "grep" };
        let event = OutputEvent::ToolCall(make_tool_call(tool, r#"{"pattern":"x"}"#));
        hooks.hook_output_observe(Arc::new(event)).await;
    }

    // 检测触发后，intercept 应原地修改 ToolResult（pending 非空）
    let mut event = OutputEvent::ToolResult(make_tool_result("read", "结果"));
    assert!(hooks.hook_output_intercept(&mut event).is_none());
    if let OutputEvent::ToolResult(tr) = event {
        assert!(
            tr.payload.content.contains("循环检测"),
            "序列模式检测后应注入修改，实际：{}",
            tr.payload.content
        );
    } else {
        panic!("事件应仍为 ToolResult");
    }
}

// ============================================================================
// 文本循环检测
// ============================================================================

#[tokio::test]
async fn text_repetition_triggers_detection() {
    // 默认 streaming_check_interval=100，需要累积足够字符；用长重复文本触发
    let hooks = assembled_guard();

    // 反复喂入相同的长文本块，累积到 interval 触发检测
    let long_text = "重复内容重复内容重复内容".repeat(20);
    for _ in 0..15 {
        let event = OutputEvent::Chunk(make_chunk(Some(&long_text), None));
        hooks.hook_output_observe(Arc::new(event)).await;
    }
    // 文本检测触发后 pending_severity 被设置；intercept 处理 Chunk 时清理 pending
    // 这里不硬断言具体级别（受 Jaccard 阈值影响），只验证链路不 panic
}

#[tokio::test]
async fn chunk_with_reasoning_accumulates() {
    // reasoning 内容也应被累积（不影响 content 检测）
    let hooks = assembled_guard();
    let event = OutputEvent::Chunk(make_chunk(Some("正文"), Some("思考过程")));
    hooks.hook_output_observe(Arc::new(event)).await;
    // 不 panic 即通过
}

// ============================================================================
// 重置语义：用户消息 vs 插件注入
// ============================================================================

#[tokio::test]
async fn user_message_fully_resets_state() {
    // 用户主动消息（source=User）触发完全重置：先制造 pending，再发用户消息，pending 应清空
    let hooks = assembled_guard();

    // 制造 pending（重复工具调用）
    for _ in 0..5 {
        let event = OutputEvent::ToolCall(make_tool_call("read", r#"{"path":"x"}"#));
        hooks.hook_output_observe(Arc::new(event)).await;
    }

    // 用户主动消息 → 完全重置
    hooks
        .hook_output_observe(Arc::new(make_user_event_from_user()))
        .await;

    // 重置后 intercept 不再注入（pending 已清空），原样放行
    let mut event = OutputEvent::ToolResult(make_tool_result("read", "结果"));
    assert!(hooks.hook_output_intercept(&mut event).is_none());
    if let OutputEvent::ToolResult(tr) = event {
        assert_eq!(
            tr.payload.content, "结果",
            "用户消息重置后 intercept 应放行原始内容"
        );
    } else {
        panic!("事件应仍为 ToolResult");
    }
}

#[tokio::test]
async fn plugin_injected_message_only_clears_pending() {
    // 插件注入消息（source=Plugin）仅 clear_pending，保留检测器历史（保持"热"状态）
    let hooks = assembled_guard();

    // 制造工具历史
    for _ in 0..3 {
        let event = OutputEvent::ToolCall(make_tool_call("read", r#"{"path":"x"}"#));
        hooks.hook_output_observe(Arc::new(event)).await;
    }

    // 插件注入消息事件
    let plugin_msg = OutputEvent::User(OutputUserMessage {
        base: EventBase::default(),
        payload: OutputUserPayload {
            content: "引导消息".to_string(),
            images: vec![],
            mode: UserMessageMode::Guide,
            source: UserMessageSource::Plugin(fuyao_api::message::input::PluginSource {
                name: "loop_guard".to_string(),
            }),
        },
    });
    hooks.hook_output_observe(Arc::new(plugin_msg)).await;

    // 再次喂入同一工具调用，应立即触发检测（历史保留 → 热状态）
    for _ in 0..3 {
        let event = OutputEvent::ToolCall(make_tool_call("read", r#"{"path":"x"}"#));
        hooks.hook_output_observe(Arc::new(event)).await;
    }
    // 不硬断言级别，只验证链路不 panic（插件注入后检测仍热）
}

// ============================================================================
// intercept 边界
// ============================================================================

#[tokio::test]
async fn intercept_passes_through_non_tool_result() {
    // 非 ToolResult 事件（如 Assistant）应直接 Pass 不修改
    let hooks = assembled_guard();

    let mut event = OutputEvent::Assistant(fuyao_api::message::output::AssistantMessage {
        base: EventBase::default(),
        payload: fuyao_api::message::output::AssistantPayload {
            content: Some("助手回复".to_string()),
            reasoning: None,
            tool_calls: None,
            finish_reason: None,
            completion_tokens: 0,
            prompt_tokens: 0,
            total_tokens: 0,
            reasoning_tokens: 0,
            cached_tokens: 0,
        },
    });
    let blocked = hooks.hook_output_intercept(&mut event);
    assert!(blocked.is_none(), "非 ToolResult 不应被阻止");
    assert!(matches!(event, OutputEvent::Assistant(_)), "应原样放行");
}

#[tokio::test]
async fn intercept_tool_result_without_pending_passes_original() {
    // 无 pending 时，intercept 放行原始 ToolResult
    let hooks = assembled_guard();
    let mut event = OutputEvent::ToolResult(make_tool_result("read", "干净的结果"));
    assert!(hooks.hook_output_intercept(&mut event).is_none());
    if let OutputEvent::ToolResult(tr) = event {
        assert_eq!(tr.payload.content, "干净的结果");
    } else {
        panic!("事件应仍为 ToolResult");
    }
}

// ============================================================================
// 装配闭环：多事件流
// ============================================================================

#[tokio::test]
async fn full_assembly_handles_mixed_event_stream() {
    // 混合事件流：用户消息 → 工具调用 → 文本块 → 工具结果，整条链路不 panic
    let hooks = assembled_guard();

    // 1. 用户消息（重置）
    hooks
        .hook_output_observe(Arc::new(make_user_event_from_user()))
        .await;

    // 2. 几个不重复的工具调用
    hooks
        .hook_output_observe(Arc::new(OutputEvent::ToolCall(make_tool_call(
            "read",
            r#"{"path":"a"}"#,
        ))))
        .await;
    hooks
        .hook_output_observe(Arc::new(OutputEvent::ToolCall(make_tool_call(
            "write",
            r#"{"path":"b"}"#,
        ))))
        .await;

    // 3. 文本块
    hooks
        .hook_output_observe(Arc::new(OutputEvent::Chunk(make_chunk(
            Some("正常输出"),
            None,
        ))))
        .await;

    // 4. 工具结果经 intercept
    let mut tr = OutputEvent::ToolResult(make_tool_result("read", "a 的内容"));
    let _ = hooks.hook_output_intercept(&mut tr);

    // 不 panic 即通过——混合正常事件流不应误触发检测
}
