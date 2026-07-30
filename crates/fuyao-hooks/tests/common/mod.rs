//! fuyao-hooks 集成测试共享 fixture
//!
//! 提供三类构造能力：
//! - 事件构造器（构造 OutputEvent 驱动钩子链路）
//! - SessionSender 夹具（绑定三条通道接收端，验证消息落点）
//! - 可配置 FakePlugin / FakeInstance（手写 fake：trait 返回 boxed future，automock 不适用；
//!   仿 src/plugin/tests.rs 的 CountingPlugin 风格，但参数化以驱动多种跨模块协作场景）
//!
//! 全部使用默认配置（不调 set_config），走 get_config 未 set 返回 default 的兜底，
//! 规避 set_config 的 OnceLock 进程级单例串扰。

// 跨测试二进制共享：未用部分不报 dead_code
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use fuyao_api::PluginEventSource;
use fuyao_api::message::EventBase;
use fuyao_api::message::output::{
    ChunkMessage, ChunkPayload, InterruptMessage as OutputInterruptMessage,
    PluginMessage as OutputPluginMessage, ToolCallMessage, ToolCallPayload, ToolResultMessage,
    ToolResultPayload, UserMessage as OutputUserMessage,
};
use fuyao_hooks::{HooksRegistry, Plugin, PluginInstance, SessionSender};

// ============================================================================
// 事件构造器
// ============================================================================

/// 构造流式文本块事件（content 用于 intercept 串联修改的可观测字段）
pub fn make_chunk(content: &str) -> ChunkMessage {
    ChunkMessage {
        base: EventBase::default(),
        payload: ChunkPayload {
            content: Some(content.to_string()),
            reasoning: None,
        },
    }
}

/// 构造工具调用事件（tool_args 解析失败兜底为 Null）
pub fn make_tool_call(name: &str, args: &str) -> ToolCallMessage {
    ToolCallMessage {
        base: EventBase::default(),
        payload: ToolCallPayload {
            tool_call_id: "call_1".to_string(),
            tool_name: name.to_string(),
            tool_args: serde_json::from_str(args).unwrap_or(serde_json::Value::Null),
        },
    }
}

/// 构造工具结果事件
pub fn make_tool_result(name: &str, content: &str) -> ToolResultMessage {
    ToolResultMessage {
        base: EventBase::default(),
        payload: ToolResultPayload {
            tool_call_id: "call_1".to_string(),
            tool_name: name.to_string(),
            content: content.to_string(),
        },
    }
}

// ============================================================================
// SessionSender 夹具
// ============================================================================

/// 构造绑定三条通道接收端的 SessionSender + 三条 rx，identity 固定 "fake_plugin"
///
/// 字段注入绕开全局状态：通道由测试构造，零环境变量依赖。
pub fn make_sender() -> (
    SessionSender,
    tokio::sync::mpsc::Receiver<OutputUserMessage>,
    tokio::sync::mpsc::Receiver<OutputInterruptMessage>,
    tokio::sync::mpsc::Receiver<OutputPluginMessage>,
) {
    let (tx_user, rx_user) = tokio::sync::mpsc::channel(16);
    let (tx_interrupt, rx_interrupt) = tokio::sync::mpsc::channel(16);
    let (tx_plugin, rx_plugin) = tokio::sync::mpsc::channel(16);
    let sender = SessionSender::new(
        PluginEventSource {
            name: "fake_plugin".into(),
        },
        tx_user,
        tx_interrupt,
        tx_plugin,
    );
    (sender, rx_user, rx_interrupt, rx_plugin)
}

// ============================================================================
// 可配置 FakePlugin / FakeInstance
// ============================================================================

/// 执行记录：以字符串序列记录各阶段被调用的顺序
///
/// 不同阶段（create_instance / register / observe / intercept / send_input）
/// 共享同一份日志，使执行顺序在跨阶段断言中可观测。
/// 由调用方构造（`Arc::new(Mutex::new(Vec::new()))`），构造 FakePlugin 时注入，
/// 装配后调用方持同一引用断言顺序。
pub type ExecLog = Arc<Mutex<Vec<String>>>;

/// 钩子动作：描述 FakeInstance 在 register 阶段注册哪种钩子、钩子做什么
#[derive(Clone)]
pub enum HookAction {
    /// 注册 observe 钩子，执行时记录日志（log_tag）
    Observe { log_tag: String },
    /// 注册 intercept 钩子（priority，执行时记录 log_tag 并放行）
    Intercept { priority: i32, log_tag: String },
    /// 注册 send_input 钩子，回调内立即用 sender 发一条 User 消息（content）
    SendInputUser { log_tag: String, content: String },
}

/// FakeInstance 配置：插件名 + 注册哪些钩子 + 是否在 register 阶段 panic
pub struct FakeInstanceConfig {
    pub name: String,
    pub actions: Vec<HookAction>,
    /// register 阶段是否 panic（测试 register panic 防护由消费方负责，此处仅标记）
    pub register_panic: bool,
}

/// session 级可配置 fake 实例
struct FakeInstance {
    cfg: FakeInstanceConfig,
    log: ExecLog,
}

impl PluginInstance for FakeInstance {
    fn register(&self, hooks: &mut HooksRegistry) {
        if self.cfg.register_panic {
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:register_panic", self.cfg.name));
            panic!("{} register 崩溃", self.cfg.name);
        }
        for action in &self.cfg.actions {
            match action {
                HookAction::Observe { log_tag } => {
                    let log = self.log.clone();
                    let tag = log_tag.clone();
                    hooks.register_output_observe(Arc::new(move |_msg| {
                        let log = log.clone();
                        let tag = tag.clone();
                        Box::pin(async move {
                            log.lock().unwrap().push(tag);
                        })
                    }));
                }
                HookAction::Intercept { priority, log_tag } => {
                    let log = self.log.clone();
                    let tag = log_tag.clone();
                    hooks.register_output_intercept(
                        *priority,
                        Arc::new(move |msg| {
                            log.lock().unwrap().push(tag.clone());
                            fuyao_hooks::InterceptResult::Pass(msg.clone())
                        }),
                    );
                }
                HookAction::SendInputUser { log_tag, content } => {
                    let log = self.log.clone();
                    let tag = log_tag.clone();
                    let content = content.clone();
                    hooks.register_send_input(
                        0,
                        Arc::new(move |sender| {
                            let log = log.clone();
                            let tag = tag.clone();
                            let content = content.clone();
                            Box::pin(async move {
                                log.lock().unwrap().push(tag);
                                sender.send_user(content);
                            })
                        }),
                    );
                }
            }
        }
    }
}

/// FakePlugin 配置（Clone 以支持装配链路中多阶段复制）
#[derive(Clone)]
pub struct FakePluginConfig {
    pub name: String,
    pub actions: Vec<HookAction>,
    /// create_instance 阶段是否 panic（测试 PluginHost 跳过崩溃插件）
    pub create_instance_panic: bool,
    /// register 阶段是否 panic
    pub register_panic: bool,
}

/// 引擎级可配置 fake 工厂
pub struct FakePlugin {
    cfg: FakePluginConfig,
    log: ExecLog,
}

impl FakePlugin {
    pub fn new(cfg: FakePluginConfig, log: ExecLog) -> Self {
        Self { cfg, log }
    }
}

impl Plugin for FakePlugin {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    fn create_instance(&self) -> Box<dyn PluginInstance> {
        if self.cfg.create_instance_panic {
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:create_instance_panic", self.cfg.name));
            panic!("{} create_instance 崩溃", self.cfg.name);
        }
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:create_instance", self.cfg.name));
        Box::new(FakeInstance {
            cfg: FakeInstanceConfig {
                name: self.cfg.name.clone(),
                actions: self.cfg.actions.clone(),
                register_panic: self.cfg.register_panic,
            },
            log: self.log.clone(),
        })
    }
}

// ============================================================================
// 装配链路 helper
// ============================================================================

/// 端到端装配：PluginHost.create_instances → 逐个 register → init_send_inputs
///
/// 复刻 fuyao-core assemble_session_hooks 的核心三步（不引入对 fuyao-core 的依赖）。
/// register 阶段做同步 panic 防护（与 fuyao-core 一致：register panic 的插件被跳过，
/// 不阻塞其他插件）。返回装配后的 SharedHooks。
///
/// 日志由调用方构造并注入 FakePlugin，装配后调用方持同一引用断言顺序。
pub async fn assemble(
    instances: Vec<Box<dyn PluginInstance>>,
    sender: SessionSender,
) -> fuyao_hooks::SharedHooks {
    let mut registry = HooksRegistry::new();
    for instance in &instances {
        // register panic 防护（同步）——复刻 fuyao-core 的 catch_unwind，单插件崩溃不阻塞
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            instance.register(&mut registry)
        }));
    }
    registry.init_send_inputs(sender).await;
    Arc::new(tokio::sync::Mutex::new(registry))
}
