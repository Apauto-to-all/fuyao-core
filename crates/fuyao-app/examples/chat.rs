//! fuyao 引擎端到端冒烟测试
//!
//! 通过 `fuyao_app::start` 一键装配启动引擎，验证项目完整可用。
//! 启动时选择 session 数量：
//! - 单对话：默认 deepseek 模型，事件以 JSON 原样打印（无颜色）
//! - 双对话：同一条消息同时发给两个 session，流式输出交错到达，
//!   用颜色区分来源（A 青色 / B 黄色），体现多 session 并发不阻塞
//!
//! 本例不做任何事件特化处理——每条 OutputEvent 直接序列化为 JSON 打印，
//! 仅附加可选颜色，目标是「能跑起来」，后续再迭代展示层。
//!
//! ## 运行
//! ```bash
//! cargo run --example chat -p fuyao-app
//! ```

use std::io::Write;

use fuyao_api::message::input::{UserMessage, UserMessageMode, UserMessageSource, UserPayload};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{AgentPaths, EngineParams, ModelConfig, SessionParams};
use fuyao_app::App;
use tokio::io::{AsyncBufReadExt, BufReader};

/// 默认模型（provider/model 形式，按项目约定）
const DEFAULT_MODEL: &str = "deepseek/deepseek-v4-flash";

/// ANSI 颜色码：双对话时 A 用青色、B 用黄色，两路交错一眼区分归属
const COLOR_A: &str = "\x1b[36m"; // 青色
const COLOR_B: &str = "\x1b[33m"; // 黄色
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

    // 1. 一键装配：init（配置 / 日志 / Provider）→ 工具收集 → 启动引擎 → App fan-in 装配
    //    from_cwd：以当前工作目录为 workspace，使 .fuyao/skills 等项目级资源生效
    let app = fuyao_app::start(EngineParams {
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
                    model_id: Some(DEFAULT_MODEL.to_string()),
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
            "\n已创建两个并发对话（颜色区分）：\n  {ca}[A]{r}  {cb}[B]{r}\n",
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

        // 同一条消息发给所有 session（各自带 DEFAULT_MODEL）
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
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
        },
    });
    if let Err(e) = app.send(&session_id, event).await {
        eprintln!("[发送失败：{e}]");
    }
}

/// 消费一轮所有事件，按 session_id 路由到对应 lane 染色打印，直到所有 lane 完成
async fn consume_turn(app: &App, lanes: &mut [Lane]) {
    loop {
        let Some(event) = app.recv().await else {
            println!("[引擎已关闭]");
            return;
        };
        // 路由：按 session_id 定位 lane，非本轮 session 的事件忽略
        let sid = event_session_id(&event).unwrap_or("");
        let Some(idx) = lanes.iter().position(|l| l.session_id == sid) else {
            continue;
        };

        // 序列化为 JSON 原样打印（带可选颜色），不做任何事件特化处理
        let json = serde_json::to_string(&event).unwrap_or_else(|_| "<序列化失败>".into());
        print_event(&json, &lanes[idx]);

        // 终态事件标记本 lane 完成；全部完成则本轮结束
        if is_terminal(&event) {
            lanes[idx].finished = true;
            if lanes.iter().all(|l| l.finished) {
                return;
            }
        }
    }
}

/// 按 lane 颜色打印一段 JSON（无颜色时为纯文本）
fn print_event(json: &str, lane: &Lane) {
    let prefix = if lane.label.is_empty() {
        String::new()
    } else {
        format!("[{}] ", lane.label)
    };
    match lane.color {
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
