//! fuyao 引擎端到端冒烟测试
//!
//! 通过 `fuyao_app::start` 一键装配启动引擎，验证项目完整可用。
//! 启动时选择 session 数量：
//! - 单对话：使用默认模型（见 `DEFAULT_MODEL`），事件以 JSON 原样打印（无颜色）
//! - 双对话：同一条消息同时发给两个 session，A/B 各用不同模型（见 `DEFAULT_MODEL` / `MODEL_B`），
//!   流式输出交错到达，用颜色区分来源（A 青色 / B 黄色），体现多 session 并发不阻塞
//! - 子代理派生期间，子 session 事件用紫色与父 lane 区分，仍归父 lane 渲染
//!
//! 本例不做任何事件特化处理——每条 OutputEvent 直接序列化为 JSON 打印，
//! 仅附加可选颜色，目标是「能跑起来」，后续再迭代展示层。
//!
//! ## 运行
//! ```bash
//! cargo run --example chat -p fuyao-app
//! ```

use std::collections::HashMap;
use std::io::Write;

use fuyao_api::message::input::{UserMessage, UserMessageMode, UserMessageSource, UserPayload};
use fuyao_api::message::output::ChildSessionState;
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{AgentPaths, EngineParams, ModelConfig, SessionParams};
use fuyao_app::App;
use tokio::io::{AsyncBufReadExt, BufReader};

/// 模型 1（默认，provider/model 形式，按项目约定）：单对话与双对话的 session A 使用此模型
const DEFAULT_MODEL: &str = "sensenova/deepseek-v4-flash";

/// 模型 2：双对话的 session B 使用此模型，与 A 并行对比两路回答
const MODEL_B: &str = "sensenova/sensenova-6.7-flash-lite";

/// ANSI 颜色码：双对话时 A 用青色、B 用黄色，两路交错一眼区分归属
const COLOR_A: &str = "\x1b[36m"; // 青色
const COLOR_B: &str = "\x1b[33m"; // 黄色
/// 子代理事件颜色：子 session 事件统一用紫色，与父 lane 颜色区分
const COLOR_CHILD: &str = "\x1b[35m"; // 紫色
const COLOR_RESET: &str = "\x1b[0m";

/// 单路对话的展示状态
struct Lane {
    /// 标签（A/B），单对话时为空（不打印前缀）
    label: &'static str,
    /// session id
    session_id: String,
    /// ANSI 颜色码，None 表示不染色（单对话）
    color: Option<&'static str>,
    /// 本轮是否已收到终态事件（最终回复 / 错误 / 中断）
    finished: bool,
}

#[tokio::main]
async fn main() {
    println!("=== fuyao 引擎端到端冒烟测试 ===\n");

    // 1. 一键装配：init（配置 / 日志 / Provider）→ 工具收集 → 创建 store → 启动引擎 → 装配
    //    返回 FuyaoApp { app（运行时交互）, sessions（会话管理）}，本例只用 app 跑对话
    //    from_cwd：以当前工作目录为 workspace，使 .fuyao/skills 等项目级资源生效
    let fuyao_app::FuyaoApp { app, .. } = fuyao_app::start(EngineParams {
        agent_paths: AgentPaths::from_cwd(),
    })
    .await
    .expect("引擎启动失败，请检查配置与 API Key");

    // 单一 reader 贯穿全程：先读模式选择，再读交互输入
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);

    // 2. 选择 session 数量，据此创建对话
    let two = read_two_sessions(&mut reader).await;
    let mut lanes = Vec::new();

    let session_a = app
        .create_session(SessionParams {
            model_config: ModelConfig {
                model_id: Some(DEFAULT_MODEL.to_string()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .expect("创建 session A 失败");
    lanes.push(Lane {
        label: if two { "A" } else { "" },
        session_id: session_a,
        color: if two { Some(COLOR_A) } else { None },
        finished: false,
    });

    if two {
        let session_b = app
            .create_session(SessionParams {
                model_config: ModelConfig {
                    model_id: Some(MODEL_B.to_string()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .expect("创建 session B 失败");
        lanes.push(Lane {
            label: "B",
            session_id: session_b,
            color: Some(COLOR_B),
            finished: false,
        });
        println!(
            "\n已创建两个并发对话（颜色区分，A 用 {DEFAULT_MODEL} / B 用 {MODEL_B}）：\n  {ca}[A]{r}  {cb}[B]{r}\n",
            ca = COLOR_A,
            cb = COLOR_B,
            r = COLOR_RESET
        );
    } else {
        println!("\n已创建单个对话（模型 {DEFAULT_MODEL}）\n");
    }

    // 3. 交互循环：读取输入 → 发给所有 session → 消费本轮事件
    loop {
        print!("你 > ");
        std::io::stdout().flush().ok();
        let mut input = String::new();
        match reader.read_line(&mut input).await {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("读取输入失败：{e}");
                break;
            }
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input == "exit" || input == "quit" {
            break;
        }

        // 同一条消息发给所有 session（模型由各自 session 的 SessionParams 决定，创建时定）
        for lane in &lanes {
            send_to(&app, &lane.session_id, input).await;
        }

        // 消费本轮：App::recv 流出事件，按 session_id 路由到对应 lane 染色打印，
        // 直到所有 lane 收到终态事件（Assistant 非 tool_calls / Error / Interrupt）
        consume_turn(&app, &mut lanes).await;
        println!();
    }

    app.shutdown().await;
    println!("已退出。");
}

/// 读取是否双 session 模式（输入 2 为双对话，其余默认单对话）
async fn read_two_sessions<R>(reader: &mut R) -> bool
where
    R: AsyncBufReadExt + Unpin,
{
    print!("选择 session 数量（1=单对话，2=双对话并发，默认 1）：");
    std::io::stdout().flush().ok();
    let mut buf = String::new();
    let _ = reader.read_line(&mut buf).await;
    buf.trim() == "2"
}

/// 把一条用户消息发给指定 session（模型由 session 的 SessionParams 决定，创建时定）
async fn send_to(app: &App, session_id: &str, content: &str) {
    let session_id = session_id.to_string();
    let event = InputEvent::User(UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: content.to_string(),
            images: vec![],
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        },
    });
    if let Err(e) = app.send(&session_id, event).await {
        eprintln!("[发送失败：{e}]");
    }
}

/// 消费一轮所有事件，按 session_id 路由到对应 lane 染色打印，直到所有 lane 完成
///
/// 子代理派生期间，子 session 的事件（Chunk / ToolCall / ...）经父 session 出站通道
/// 流出，base.session_id 标的是 child。本函数维护 `child_session_id → 父 lane` 映射，
/// 让子事件归父 lane 渲染（沿用父颜色），但不参与终态判断——子的 stop 不能误判
/// 父本轮结束。映射由 ChildSession 生命周期事件（Started 建立 / Ended 拆除）维护。
async fn consume_turn(app: &App, lanes: &mut [Lane]) {
    // child_session_id → 父 lane idx：本轮动态建立 / 拆除
    let mut child_to_parent: HashMap<String, usize> = HashMap::new();

    loop {
        let Some(event) = app.recv().await else {
            println!("[引擎已关闭]");
            return;
        };
        let sid = event_session_id(&event).unwrap_or("");

        // ChildSession 生命周期事件：base.session_id 是父（由父上下文发出），
        // payload 标 child 生命周期——先据此维护映射，再走通用渲染
        if let OutputEvent::ChildSession(m) = &event
            && let Some(parent_idx) = lanes
                .iter()
                .position(|l| l.session_id == m.payload.parent_session_id)
        {
            match m.payload.state {
                ChildSessionState::Started => {
                    child_to_parent.insert(m.payload.child_session_id.clone(), parent_idx);
                }
                ChildSessionState::Ended => {
                    child_to_parent.remove(&m.payload.child_session_id);
                }
            }
        }

        // 路由：父 lane 直接匹配；否则查子 session 映射回父 lane；都不在则忽略
        let Some(idx) = lanes
            .iter()
            .position(|l| l.session_id == sid)
            .or_else(|| child_to_parent.get(sid).copied())
        else {
            continue;
        };

        // 序列化为 JSON 原样打印；子 session 事件（经 child 映射命中）用紫色区分父子
        let json = serde_json::to_string(&event).unwrap_or_else(|_| "<序列化失败>".into());
        let is_child = lanes[idx].session_id != sid;
        print_event(&json, &lanes[idx], is_child);

        // 终态判断：只认父 session_id 自身的事件（子的 stop 不能误判父本轮结束）
        if is_terminal(&event) && lanes[idx].session_id == sid {
            lanes[idx].finished = true;
            if lanes.iter().all(|l| l.finished) {
                return;
            }
        }
    }
}

/// 按 lane 颜色打印一段 JSON（无颜色时为纯文本）
///
/// `child=true` 时改用子代理紫色，与父 lane 颜色区分；prefix 仍取父 lane 标签
/// （标识归属），仅颜色切换父子。
fn print_event(json: &str, lane: &Lane, child: bool) {
    let prefix = if lane.label.is_empty() {
        String::new()
    } else {
        format!("[{}] ", lane.label)
    };
    let color = if child { Some(COLOR_CHILD) } else { lane.color };
    match color {
        Some(c) => println!("{c}{prefix}{json}{COLOR_RESET}"),
        None => println!("{prefix}{json}"),
    }
}

/// 提取事件的 session_id（enum 各变体的 base 均自带 session_id）
fn event_session_id(event: &OutputEvent) -> Option<&str> {
    match event {
        OutputEvent::Chunk(m) => m.base.session_id.as_deref(),
        OutputEvent::User(m) => m.base.session_id.as_deref(),
        OutputEvent::ToolCall(m) => m.base.session_id.as_deref(),
        OutputEvent::ToolResult(m) => m.base.session_id.as_deref(),
        OutputEvent::Assistant(m) => m.base.session_id.as_deref(),
        OutputEvent::Interrupt(m) => m.base.session_id.as_deref(),
        OutputEvent::Error(m) => m.base.session_id.as_deref(),
        OutputEvent::Plugin(m) => m.base.session_id.as_deref(),
        OutputEvent::Compression(m) => m.base.session_id.as_deref(),
        OutputEvent::Title(m) => m.base.session_id.as_deref(),
        OutputEvent::Retry(m) => m.base.session_id.as_deref(),
        OutputEvent::ChildSession(m) => m.base.session_id.as_deref(),
    }
}

/// 判断是否为终态事件（标志一个 session 本轮结束）
fn is_terminal(event: &OutputEvent) -> bool {
    match event {
        // 助手最终回复：finish_reason 非 tool_calls（tool_calls 是中间态，还要等续答）
        OutputEvent::Assistant(m) => m.payload.finish_reason.as_deref() != Some("tool_calls"),
        OutputEvent::Error(_) | OutputEvent::Interrupt(_) => true,
        _ => false,
    }
}
