//! 图片输入端到端测试（需真实 API Key + 网络 + 本地图片）
//!
//! 用分步装配的真实引擎验证多模态全链路：读本地图片 → base64 →
//! `InputEvent::User` 携带 images → 落库 → provider 构造 OpenAI 图片请求 →
//! 真实 LLM 返回图片描述。
//!
//! **不注册任何工具**：本测试只验证图片输入链路，无需 AI 调用工具。
//! 因此不走 `fuyao_app::start`（它会把内置 + MCP 工具全量注册进 ToolRegistry），
//! 改用 `init_engine` + `Engine::new` 分步装配，注入**空 ToolRegistry**——
//! 工具列表不会随请求发给 LLM，省 token。
//!
//! 标记 `#[ignore]`：依赖本机 `~/.fuyao/fuyao.toml` 配置的 provider
//! （含 API Key）、可联网调用真实 LLM、以及 workspace 根的 `.fuyao/bb.png`，
//! 非纯单元测试，手动运行：
//!
//! ```bash
//! cargo test -p fuyao-app --test image_input_e2e_test -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use fuyao_api::message::input::{UserMessage, UserMessageMode, UserMessageSource, UserPayload};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{AgentConfig, AgentPaths, EngineParams, ImageContent, ModelConfig, SessionParams};
use fuyao_app::App;
use fuyao_app::{InitResult, init_engine};
use fuyao_core::{Engine, PluginHost, ToolRegistry};

/// 测试用模型：已在 `~/.fuyao/fuyao.toml` 的 `[providers.sensenova.models."sensenova-6.7-flash-lite"]` 配置
const MODEL_ID: &str = "sensenova/sensenova-6.7-flash-lite";

/// 待识别的本地图片：workspace 根下的 `.fuyao/bb.png`
///
/// 用相对基准拼接（`CARGO_MANIFEST_DIR` 指向 `crates/fuyao-app`），需回退两层：
/// `crates/fuyao-app` → `crates` → workspace 根。避免硬编码绝对路径。
const IMAGE_REL_PATH: &str = "../../.fuyao/bb.png";

/// 提问文本
const ASK_TEXT: &str = "请描述这张图片的内容";

/// 本轮等待 LLM 回复的超时（真实网络调用，给足余量）
const RECV_TIMEOUT: Duration = Duration::from_secs(120);

/// 用完整引擎验证图片输入端到端：发送带图片的用户消息，收取并打印 LLM 的图片描述
#[tokio::test]
#[ignore]
async fn image_input_described_by_real_llm() {
    println!("=== 图片输入端到端测试 ===\n");

    // 1. 分步装配真实引擎（不走 start，跳过工具收集）：
    //    init_engine：加载 ~/.fuyao/fuyao.toml → 配置 / 日志 / provider 注册与实例化
    //    AgentPaths::from_cwd 以当前工作目录为 workspace，使模型配置与 API Key 正常加载
    let engine_params = EngineParams {
        agent_paths: AgentPaths::from_cwd(),
    };
    let InitResult {
        provider,
        log_guard,
    } = init_engine(&engine_params)
        .await
        .expect("引擎初始化失败，请检查 ~/.fuyao/fuyao.toml 配置与 API Key");

    // 装配插件工厂（与真实引擎一致，保留循环检测 guard；本测试无工具调用，不会触发）
    let mut plugin_host = PluginHost::new();
    plugin_host.add(Box::new(fuyao_guard::LoopGuardPlugin::new()));

    // 注入空 ToolRegistry：本测试无需 AI 调工具，空表不随请求发给 LLM，省 token
    let empty_tools = ToolRegistry::builder().build();
    // store 所有权归装配方，创建后注入 Engine（与 SessionManager 共享同一份）
    let store = std::sync::Arc::new(
        fuyao_session::SessionStore::new(engine_params.agent_paths.sessions_db_path())
            .await
            .expect("创建会话存储失败"),
    );
    let engine = Engine::new(engine_params, provider, empty_tools, plugin_host, store).await;
    let app = App::new(engine, None, log_guard);

    // 2. 读本地图片 → base64 → ImageContent（mime 按扩展名推断）
    let image = load_image().expect("加载测试图片失败");
    println!(
        "[e2e] 已加载图片：mime={}, base64 长度={} 字符",
        image.mime_type,
        image.data.len()
    );

    // 3. 创建 session，绑定目标模型
    let session_id = app
        .create_session(SessionParams {
            agent_config: AgentConfig {
                definition: "default".to_string(),
            },
            model_config: ModelConfig {
                model_id: MODEL_ID.to_string(),
                thinking_type: None,
                reasoning_effort: None,
            },
        })
        .await
        .expect("创建 session 失败");
    println!("[e2e] 已创建 session，模型 {MODEL_ID}\n");

    // 4. 发送带图片的用户消息
    let event = InputEvent::User(UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: ASK_TEXT.to_string(),
            images: vec![image],
            mode: UserMessageMode::Guide,
            source: UserMessageSource::User,
            client_message_id: None,
        },
    });
    app.send(&session_id, event).await.expect("发送消息失败");
    println!("[e2e] 已发送提问：「{ASK_TEXT}」\n");

    // 5. 消费本轮事件，收集助手最终回复文本，直到终态
    let reply = consume_until_terminal(&app).await;

    // 6. 打印并断言模型确实给出了非空描述
    println!("\n[e2e] 模型回复：\n{reply}\n");
    assert!(
        !reply.trim().is_empty(),
        "模型回复不应为空——若为空，检查目标模型是否声明了 modalities.input 含 image"
    );

    app.shutdown().await;
    println!("[e2e] ✅ 测试完成");
}

/// 消费本轮事件直到终态，拼接助手流式文本回复
///
/// 终态判定：`Assistant` 事件且 `finish_reason != "tool_calls"`（非工具调用中间态），
/// 或 Error / Interrupt。
async fn consume_until_terminal(app: &App) -> String {
    let mut text = String::new();
    loop {
        let event = match tokio::time::timeout(RECV_TIMEOUT, app.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => {
                println!("[e2e] 引擎已关闭");
                break;
            }
            Err(_) => panic!("[e2e] ❌ 等待回复超时（{RECV_TIMEOUT:?}），可能网络异常或模型无响应"),
        };

        match &event {
            OutputEvent::Chunk(m) => {
                // 流式增量文本，实时拼接 + 打印（--nocapture 可见）
                if let Some(piece) = m.payload.content.as_deref() {
                    print!("{piece}");
                    text.push_str(piece);
                }
            }
            OutputEvent::Assistant(m) => {
                // 终态：finish_reason 非 tool_calls 即本轮结束
                if m.payload.finish_reason.as_deref() != Some("tool_calls") {
                    println!();
                    break;
                }
            }
            OutputEvent::Error(m) => {
                panic!("[e2e] ❌ 引擎报错：{}", m.payload.message);
            }
            OutputEvent::Interrupt(m) => {
                panic!("[e2e] ❌ 被中断：{}", m.payload.reason);
            }
            _ => {
                // 其余事件（ToolCall / ToolResult / Retry 等）原样打印，不参与终态
                let json = serde_json::to_string(&event).unwrap_or_default();
                println!("\n[e2e] 事件：{json}");
            }
        }
    }
    text
}

/// 加载本地测试图片：读文件 → base64 编码 → 按扩展名推断 mime → ImageContent
///
/// 路径基准为 `crates/fuyao-app`（`CARGO_MANIFEST_DIR`），回退两层到 workspace 根，
/// 拼接相对路径 `IMAGE_REL_PATH`，避免硬编码绝对路径。
fn load_image() -> Result<ImageContent, String> {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = base.join(IMAGE_REL_PATH);

    let bytes =
        std::fs::read(&path).map_err(|e| format!("读取图片失败 {}: {e}", path.display()))?;

    let mime = infer_mime(&path).ok_or_else(|| format!("不支持的图片格式：{}", path.display()))?;

    // fully-qualified 调用避免引入 base64::Engine trait（与 fuyao_core::Engine 结构体重名）
    let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);

    Ok(ImageContent {
        mime_type: mime.to_string(),
        data,
    })
}

/// 按文件扩展名推断图片 MIME 类型
///
/// 覆盖 OpenAI 兼容协议主流图片类型；未知扩展名返回 None。
fn infer_mime(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}
