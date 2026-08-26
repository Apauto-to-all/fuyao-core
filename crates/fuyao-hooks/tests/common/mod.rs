//! fuyao-hooks 集成测试共享 fixture
//!
//! 提供三类构造能力：
//! - 事件构造器（构造 OutputEvent 驱动钩子链路）
//! - 通道夹具（构造 session 双通道，验证消息落点）
//! - 可配置 FakePlugin / FakeInstance（手写 fake：trait 返回 boxed future，automock 不适用；
//!   仿 src/plugin/tests.rs 的 CountingPlugin 风格，但参数化以驱动多种跨模块协作场景）
//!
//! 全部使用默认配置（不调 set_config），走 get_config 未 set 返回 default 的兜底，
//! 规避 set_config 的 OnceLock 进程级单例串扰。

// 跨测试二进制共享：未用部分不报 dead_code
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use fuyao_api::UserMessageMode;
use fuyao_api::message::QueueEntry;
use fuyao_api::message::output::{
    ChunkMessage, ChunkPayload, InterruptMessage as OutputInterruptMessage, ToolCallMessage,
    ToolCallPayload, ToolResultMessage, ToolResultPayload,
};
use fuyao_api::message::{EventBase, OutputEvent};
use fuyao_hooks::{HooksRegistry, NamedPluginInstance, Plugin, PluginInstance, SessionSender};

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
// 通道夹具
// ============================================================================

/// 构造 session 的两条通道（统一入站 / Interrupt），返回 (tx 对, rx 对)
///
/// 字段注入绕开全局状态：通道由测试构造，零环境变量依赖。
pub fn make_channels() -> (
    tokio::sync::mpsc::Sender<QueueEntry>,
    tokio::sync::mpsc::Receiver<QueueEntry>,
    tokio::sync::mpsc::Sender<OutputInterruptMessage>,
    tokio::sync::mpsc::Receiver<OutputInterruptMessage>,
) {
    let (tx_inbound, rx_inbound) = tokio::sync::mpsc::channel(16);
    let (tx_interrupt, rx_interrupt) = tokio::sync::mpsc::channel(16);
    (tx_inbound, rx_inbound, tx_interrupt, rx_interrupt)
}

// ============================================================================
// 可配置 FakePlugin / FakeInstance
// ============================================================================

/// 执行记录：以字符串序列记录各阶段被调用的顺序
///
/// 不同阶段（create_instance / register / observe / intercept）
/// 共享同一份日志，使执行顺序在跨阶段断言中可观测。
/// 由调用方构造（`Arc::new(Mutex::new(Vec::new()))`），构造 FakePlugin 时注入，
/// 装配后调用方持同一引用断言顺序。
pub type ExecLog = Arc<Mutex<Vec<String>>>;

/// 钩子动作：描述 FakeInstance 在 register 阶段注册哪种钩子、钩子做什么
#[derive(Clone)]
pub enum HookAction {
    /// 注册 observe 钩子（priority，执行时记录 log_tag）
    Observe { priority: i32, log_tag: String },
    /// 注册 intercept 钩子（priority，执行时记录 log_tag 并放行）
    Intercept { priority: i32, log_tag: String },
    /// register 时记录日志（log_tag）并立即用 sender 发一条 User 消息（content）
    SendUserOnRegister { log_tag: String, content: String },
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
    fn register(&self, hooks: &mut HooksRegistry, sender: &SessionSender) {
        if self.cfg.register_panic {
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:register_panic", self.cfg.name));
            panic!("{} register 崩溃", self.cfg.name);
        }
        for action in &self.cfg.actions {
            match action {
                HookAction::Observe { priority, log_tag } => {
                    let log = self.log.clone();
                    let tag = log_tag.clone();
                    hooks.register_output_observe(
                        *priority,
                        Arc::new(move |_msg| {
                            let log = log.clone();
                            let tag = tag.clone();
                            Box::pin(async move {
                                log.lock().unwrap().push(tag);
                            })
                        }),
                    );
                }
                HookAction::Intercept { priority, log_tag } => {
                    let log = self.log.clone();
                    let tag = log_tag.clone();
                    // 原地协议：钩子只记录日志不修改事件，返回 None 放行
                    hooks.register_output_intercept(
                        *priority,
                        Arc::new(move |_msg| {
                            log.lock().unwrap().push(tag.clone());
                            None
                        }),
                    );
                }
                HookAction::SendUserOnRegister { log_tag, content } => {
                    self.log.lock().unwrap().push(log_tag.clone());
                    sender.send_user(content.clone(), UserMessageMode::Guide);
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

/// 端到端装配：逐个 register（sender 绑插件名）→ finalize 冻结 → 包 Arc
///
/// 复刻 fuyao-core assemble_session_hooks 的核心步骤（不引入对 fuyao-core 的依赖）。
/// register 阶段做同步 panic 防护（与 fuyao-core 一致：register panic 的插件被跳过，
/// 不阻塞其他插件）。返回装配后的 SharedHooks（只读共享，无锁）。
///
/// 日志由调用方构造并注入 FakePlugin，装配后调用方持同一引用断言顺序。
pub fn assemble(
    instances: Vec<NamedPluginInstance>,
    tx_inbound: tokio::sync::mpsc::Sender<QueueEntry>,
    tx_interrupt: tokio::sync::mpsc::Sender<OutputInterruptMessage>,
) -> fuyao_hooks::SharedHooks {
    let mut registry = HooksRegistry::new();
    // notice 出站通道：装配测试不验证直送侧，接收端丢弃
    let (tx_event, _rx_event) = tokio::sync::mpsc::unbounded_channel::<OutputEvent>();
    for (name, instance) in &instances {
        // 每个实例拿到绑定自己插件名的 sender（注入消息 source 可追溯）
        let sender = SessionSender::new(
            name.clone(),
            "test-session",
            tx_inbound.clone(),
            tx_interrupt.clone(),
            tx_event.clone(),
        );
        // register panic 防护（同步）——复刻 fuyao-core 的 catch_unwind，单插件崩溃不阻塞
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            instance.register(&mut registry, &sender)
        }));
    }
    registry.finalize();
    Arc::new(registry)
}
