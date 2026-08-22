//! 供应商运行时注册集成测试：写盘 → 内存注册 → 存活 engine 立即可用的端到端链路
//!
//! 跨层协作验证（fuyao-app 管理面写盘 + fuyao-core 运行时注册原语 + 真实
//! OpenAIProvider HTTP 行为），钉死两条核心契约：
//! 1. **创建即生效**：引擎存活期间，经 ProviderManager 写盘新供应商后调
//!    `Engine::register_provider`，新 session 的新 turn 直接路由到新供应商
//!    （不经重启、不经缓存重装）；
//! 2. **运行中 turn 零中断**：删除（反注册）发生在 turn 的 LLM 调用已发出之后，
//!    该 turn 仍完整跑完；下一轮解析缺失时报错信息含实体标识（可诊断）。
//!
//! mock 策略：LLM HTTP 端点不可控（真实供应商 API），用手写 mini SSE server
//! （tokio TcpListener）承接——「请求到达」信号让删除时机的判定确定性成立
//! （信号到达 = provider 已解析 + HTTP 已发出），延迟响应用于拉开删除窗口。
//!
//! 全局状态规避：不走 `init_engine`（不 set_config，get_config 走 default 兜底），
//! 手动完成「配置注册 → 实例批量构造 → Engine 装配」三步（全部公开 API）；
//! 每用例独立临时 fuyao_home + 唯一 agent_id（字段注入隔离缓存）。

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::make_store;
use fuyao_api::message::input::{UserMessage, UserPayload};
use fuyao_api::message::{EventBase, InputEvent, OutputEvent};
use fuyao_api::{AgentConfig, AgentPaths, EngineParams, Model, ModelConfig, SessionParams};
use fuyao_app::{ProviderManager, ProviderModelSpec, ProviderSpec};
use fuyao_core::{Engine, PluginHost};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// 构造带临时 fuyao_home + 唯一 agent_id 的 AgentPaths（隔离注册缓存与落盘）
fn isolated_paths(test_name: &str) -> (AgentPaths, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("创建临时 fuyao_home 失败");
    let paths = AgentPaths {
        agent_id: Some(format!("global/{test_name}")),
        workspace: None,
        extra_dirs: Vec::new(),
        fuyao_home: home.path().to_path_buf(),
    };
    (paths, home)
}

/// 构造最小合法模型（limit.context 必填正整数）
fn runtime_model(name: &str) -> Model {
    Model {
        name: name.to_string(),
        cost: Default::default(),
        limit: fuyao_api::ModelLimit {
            context: 64000,
            input: None,
            output: 4096,
        },
        reasoning_efforts: vec![],
        modalities: Default::default(),
    }
}

/// 指定 model_id 的 SessionParams（definition 走内置 default 兜底）
fn params_with_model(model_id: &str) -> SessionParams {
    SessionParams {
        agent_config: AgentConfig {
            definition: "default".to_string(),
        },
        model_config: ModelConfig {
            model_id: model_id.to_string(),
            thinking_type: None,
            reasoning_effort: None,
        },
    }
}

/// Guide 模式用户消息
fn guide_message(content: &str) -> InputEvent {
    InputEvent::User(UserMessage {
        base: EventBase::default(),
        payload: UserPayload {
            content: content.to_string(),
            images: vec![],
            mode: Default::default(),
            source: Default::default(),
        },
    })
}

/// 手写 mini SSE server：OpenAI 兼容 `/chat/completions` 的流式端点
///
/// 每个请求：读完请求（headers + Content-Length body）→ 经 `tx_hit` 发「请求到达」
/// 信号 → 延迟 `delay` → 回固定 SSE（单条 TextDelta + Done + [DONE]）。
/// 返回 base_url（`http://127.0.0.1:{port}`，OpenAIProvider 自动拼
/// `/chat/completions`）。
///
/// 「请求到达」信号是该 server 的核心测试价值：信号到达即证明 provider 实例
/// 已被 turn 解析并发出真实 HTTP 请求——删除时机不再依赖 sleep 猜测。
async fn spawn_sse_server(delay: Duration, tx_hit: mpsc::Sender<()>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定本地端口失败");
    let addr = listener.local_addr().expect("取本地地址失败");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let tx_hit = tx_hit.clone();
            let delay = delay;
            tokio::spawn(async move {
                // 读完整请求：headers 到 \r\n\r\n，再按 Content-Length 读 body
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = match socket.read(&mut chunk).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(header_end) = find_header_end(&buf) {
                        let content_length = extract_content_length(&buf[..header_end]);
                        if buf.len() >= header_end + content_length {
                            break;
                        }
                    }
                }
                // 请求完整到达：通知测试方（此刻 provider 已解析、HTTP 已发出）
                let _ = tx_hit.send(()).await;
                tokio::time::sleep(delay).await;
                let body = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"来自 mock 的回复\"}}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
                    "data: [DONE]\n\n",
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

/// 找到 HTTP 请求 headers 结束位置（\r\n\r\n 之后第一个字节的下标）
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// 从请求头里解析 Content-Length（缺失按 0）
fn extract_content_length(headers: &[u8]) -> usize {
    let text = String::from_utf8_lossy(headers);
    for line in text.split("\r\n") {
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            return value.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// 消费 session 事件流直到出现 Assistant（返回其 content），期间出现的 Error
/// 立即返回错误形态；超时返回 None（由调用方断言失败）
async fn wait_for_assistant(
    rx: &mut mpsc::UnboundedReceiver<OutputEvent>,
) -> Option<Result<String, String>> {
    for _ in 0..200 {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(OutputEvent::Assistant(a))) => {
                return Some(Ok(a.payload.content.unwrap_or_default()));
            }
            Ok(Some(OutputEvent::Error(e))) => return Some(Err(e.payload.message)),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
    None
}

/// 手动装配引擎（不走 init_engine，避开 set_config 的 OnceLock 全局状态）：
/// 配置注册缓存（复刻 init_engine 的注册步骤）→ 批量构造实例 → Engine::new
async fn assemble_engine(paths: &AgentPaths) -> Arc<Engine> {
    let config = fuyao_api::load_config(paths)
        .unwrap()
        .expect("测试布局应有配置");
    let cache_key = fuyao_provider::agent_paths_cache_key(paths);
    for (provider_id, provider) in &config.providers {
        fuyao_provider::register_provider(provider_id, provider.clone(), &cache_key);
        for (model_id, model) in &provider.models {
            let full_id = format!("{provider_id}/{model_id}");
            fuyao_provider::register_model(&full_id, model.clone(), &cache_key);
        }
    }
    let registry = fuyao_provider::ProviderRegistry::from_registered(paths);
    let store = make_store(paths).await;
    Engine::new(
        EngineParams {
            agent_paths: paths.clone(),
        },
        registry,
        fuyao_core::ToolRegistry::builder().build(),
        PluginHost::new(),
        store,
    )
    .await
}

/// 落盘一份指向 mock server 的 global 层供应商（明文 api_key 走 options，
/// 避免 .env 环境变量串扰）
fn write_provider_config(
    home: &std::path::Path,
    provider_id: &str,
    base_url: &str,
    model_id: &str,
) {
    let content = format!(
        "[providers.{provider_id}]\n\
         name = \"测试-{provider_id}\"\n\
         options = {{ api_key = \"sk-test\", base_url = \"{base_url}\" }}\n\
         [providers.{provider_id}.models.\"{model_id}\"]\n\
         name = \"{model_id}\"\n\
         limit = {{ context = 64000 }}\n"
    );
    std::fs::write(home.join("fuyao.toml"), content).expect("写测试配置失败");
}

// ============================================================================
// 创建即生效：写盘 + 运行时注册 → 存活 engine 的新 turn 直接调用新供应商
// ============================================================================

/// 引擎存活期间创建并运行时注册新供应商：新 session 的新 turn 不经重启即可
/// 路由到新供应商（mock server 收到真实 HTTP 请求、回复完整抵达）
#[tokio::test]
async fn create_then_register_is_immediately_callable() {
    let (paths, home) = isolated_paths("rt_create_usable");
    let (tx_hit, mut rx_hit) = mpsc::channel(8);
    let base_url = spawn_sse_server(Duration::ZERO, tx_hit).await;
    // 启动期只装配 alpha；beta 稍后经管理面创建
    write_provider_config(home.path(), "alpha", &base_url, "alpha-m");
    let engine = assemble_engine(&paths).await;

    // —— 存活引擎期间：管理面写盘新供应商 beta（模型一并内嵌写入）——
    let manager = ProviderManager::new(paths.clone());
    manager
        .create_provider(
            "beta",
            ProviderSpec {
                name: "Beta".to_string(),
                base_url: Some(base_url.clone()),
                api_key_env_var: Some("BETA_API_KEY".to_string()),
                // 明文进 .env 指定变量；构造实例走 api_key_env_vars 指针解析链
                api_key: Some("sk-beta".to_string()),
                models: vec![ProviderModelSpec {
                    id: "beta-m".to_string(),
                    model: runtime_model("beta-m"),
                }],
            },
        )
        .expect("创建 beta 失败");

    // 运行时注册（内存缓存 + 实例表）：此刻起存活 engine 对 beta 可调用
    engine
        .register_provider("beta")
        .expect("运行时注册 beta 失败");

    // —— 新 turn 直接调用新供应商：session 以 beta/beta-m 发消息 ——
    let (session_id, mut rx_event) = engine
        .create_session(params_with_model("beta/beta-m"))
        .await
        .expect("创建 session 失败");
    engine
        .send(&session_id, guide_message("你好"))
        .await
        .expect("发消息失败");

    // mock server 收到请求 = 新供应商被真实路由调用
    tokio::time::timeout(Duration::from_secs(5), rx_hit.recv())
        .await
        .expect("mock server 应收到新供应商的请求")
        .expect("server 任务不应退出");

    // turn 完整跑完：Assistant 回复抵达（不经重启）
    match wait_for_assistant(&mut rx_event).await {
        Some(Ok(content)) => assert!(
            content.contains("来自 mock 的回复"),
            "回复应来自 mock：{content}"
        ),
        other => panic!("新 turn 应完整完成，实际：{other:?}"),
    }

    engine.shutdown().await;
    fuyao_provider::clear_cache(&paths);
}

/// 运行时注册未落盘的供应商：报错（含实体标识与落盘指引）
#[tokio::test]
async fn register_without_disk_write_errors() {
    let (paths, home) = isolated_paths("rt_register_nodisk");
    let (tx_hit, _rx_hit) = mpsc::channel(8);
    let base_url = spawn_sse_server(Duration::ZERO, tx_hit).await;
    write_provider_config(home.path(), "alpha", &base_url, "alpha-m");
    let engine = assemble_engine(&paths).await;

    let err = engine.register_provider("ghost").expect_err("未落盘应报错");
    let msg = err.to_string();
    assert!(msg.contains("ghost"), "错误含实体标识：{msg}");
    assert!(msg.contains("未在三层"), "错误指向落盘前置条件：{msg}");

    engine.shutdown().await;
    fuyao_provider::clear_cache(&paths);
}

// ============================================================================
// 运行中 turn 零中断 + 下一轮缺失报错可诊断
// ============================================================================

/// turn 的 LLM 请求已发出后删除供应商（写盘 + 反注册）：本轮跑完，下一轮
/// 报错信息含实体标识
#[tokio::test]
async fn unregister_midturn_finishes_current_and_errors_next() {
    let (paths, home) = isolated_paths("rt_midturn_delete");
    // 延迟响应拉开删除窗口：请求到达后先挂起，测试方在此期间完成删除
    let (tx_hit, mut rx_hit) = mpsc::channel(8);
    let base_url = spawn_sse_server(Duration::from_millis(600), tx_hit).await;
    write_provider_config(home.path(), "beta", &base_url, "beta-m");
    let engine = assemble_engine(&paths).await;

    let (session_id, mut rx_event) = engine
        .create_session(params_with_model("beta/beta-m"))
        .await
        .expect("创建 session 失败");
    engine
        .send(&session_id, guide_message("第一轮"))
        .await
        .expect("发消息失败");

    // 等待请求到达信号：此刻 turn 已解析 provider 实例并发出 HTTP 调用
    tokio::time::timeout(Duration::from_secs(5), rx_hit.recv())
        .await
        .expect("应收到第一轮请求到达信号")
        .expect("server 任务不应退出");

    // 删除链路：管理面写盘（toml 段 + .env 变量）→ 引擎反注册（实例 + 缓存）
    let manager = ProviderManager::new(paths.clone());
    manager.delete_provider("beta").expect("写盘删除 beta 失败");
    assert!(engine.unregister_provider("beta"), "反注册应实际移除实例");

    // 零中断：第一轮照常完整完成（延迟响应抵达后 turn 收尾，Assistant 落地）
    match wait_for_assistant(&mut rx_event).await {
        Some(Ok(content)) => {
            assert!(
                content.contains("来自 mock 的回复"),
                "运行中 turn 应跑完：{content}"
            )
        }
        other => panic!("删除后运行中 turn 不应中断，实际：{other:?}"),
    }

    // 下一轮：解析缺失报错，信息含实体标识（provider 与 model 双重定位）
    engine
        .send(&session_id, guide_message("第二轮"))
        .await
        .expect("发消息失败");
    match wait_for_assistant(&mut rx_event).await {
        Some(Err(msg)) => {
            assert!(msg.contains("beta"), "错误信息含 provider 标识：{msg}");
            assert!(msg.contains("beta-m"), "错误信息含 model 标识：{msg}");
        }
        other => panic!("下一轮应报可诊断错误，实际：{other:?}"),
    }

    engine.shutdown().await;
    fuyao_provider::clear_cache(&paths);
}
