//! 会话标题自动生成
//!
//! 首轮用户消息后立即异步生成简短标题（不等 AI 回复，避免长回复拖累延迟）：
//! - 仅用用户首条消息（截断 `[session.title] snippet_max_chars` 字符）喂给 LLM
//! - 模型优先级：`[models.fast]` → 当前引擎模型 → 放弃
//! - 从引擎级共享的 ProviderRegistry 取 Provider 实例（fast 可能跨 Provider）
//! - 用非流式 `provider.chat()`（标题是短文本，无需流式增量）
//! - 只生成标题文本；落库（`SessionStore::update_title`）与事件发布由调用方负责（职责分离）
//!
//! # 三者独立（禁止绑定）
//!
//! `[models.fast]` / `Provider::chat` / 标题生成是**三个毫不相关的东西**，仅在本模块恰好同框：
//! - `[models.fast]`：一条模型配置（含 model / thinking_type / reasoning_effort 三字段），独立存在
//! - `Provider::chat`：Provider 的非流式方法（带 options），独立存在
//! - 标题生成：一个功能，恰好读 `[models.fast]` 的 model_id 选模型，恰好调 `chat()` 走非流式
//!
//! 标题生成**只取 fast 的 model_id，不取其 thinking**；调 `chat()` 恒传默认 options
//! （`thinking_type=None` + `reasoning_effort=None`）——标题固定不思考，是标题自身的设计，
//! 与 fast 配置、chat 的 options 能力均无关。禁止把三者绑成一个「标题专用链路」。

use fuyao_api::{AgentPaths, get_config};
use fuyao_provider::{ChatMessage, ChatRequest, ProviderRegistry, StreamOptions, parse_model_id};

/// 标题生成系统提示词（仅基于用户首条消息）
const TITLE_PROMPT: &str = "根据用户的第一条消息生成一个简短的描述性标题（3-7 个词）。\
标题应概括用户的主要意图或主题。使用与用户消息相同的语言撰写标题。\
只返回标题文本，不要加引号、不要加标点结尾、不要加前缀。";

/// 生成会话标题
///
/// 模型优先级：`[models.fast]` → 当前引擎模型 → 放弃（返回 None）。
/// fast 未配置或调用失败时自动回退到当前引擎模型；两者都失败则不重命名。
///
/// **只取 fast 的 model_id**（不取其 thinking）；标题固定不思考（见模块顶部「三者独立」）。
///
/// # 参数
/// - `user_message`：用户首条消息原文（内部截断）
/// - `main_model_id`：当前引擎使用的模型 ID（格式 provider_id/model_id），fast 不可用时回退
/// - `providers`：引擎级共享的 Provider 实例注册表（fast 可能跨 Provider 配置）
/// - `agent_paths`：Agent 三层目录（注册表查找用，保留以备未来扩展）
///
/// # 返回
/// - `Some(String)`：清洗后的标题（已去引号/前缀/限长）
/// - `None`：fast 与主模型都失败，或生成结果为空
pub async fn maybe_generate_title(
    user_message: &str,
    main_model_id: &str,
    providers: &ProviderRegistry,
    _agent_paths: &AgentPaths,
) -> Option<String> {
    let config = get_config();

    // 1. 优先用 fast 模型（便宜快速）——只取 model_id，不取 thinking
    if let Some(fast_ref) = config.models.fast.as_ref()
        && !fast_ref.model.is_empty()
    {
        match generate_title(user_message, &fast_ref.model, providers).await {
            Some(title) => return Some(title),
            None => {
                tracing::warn!(
                    fast_model = %fast_ref.model,
                    "fast 模型生成标题失败，回退到当前引擎模型"
                );
            }
        }
    }

    // 2. 回退到当前引擎模型；3. 再失败则放弃（返回 None）
    generate_title(user_message, main_model_id, providers).await
}

/// 调用 LLM 生成标题（单次尝试）
///
/// 内部完成：按 model_id 从 ProviderRegistry 取 Provider → 截断输入 →
/// 发非流式请求 → 清洗标题。
///
/// **标题固定不思考**：恒传 `StreamOptions::default()`（thinking 两字段为 None）。
/// 这是标题自身的设计，与 fast 配置、chat 的 options 能力无关（见模块顶部「三者独立」）。
///
/// 失败情形（返回 None）：
/// - model_id 格式错误（无 `/`）
/// - Provider 实例未注册（init 时该 provider 创建失败）
/// - LLM 调用失败（网络 / 鉴权 / 模型不存在等）
/// - 响应为空或清洗后为空
async fn generate_title(
    user_message: &str,
    model_id: &str,
    providers: &ProviderRegistry,
) -> Option<String> {
    // 解析 "provider_id/model_id" → 取 Provider 实例 + 裸模型名
    let (provider_id, model_name) = parse_model_id(model_id).ok()?;
    let provider = providers.get(&provider_id)?;

    let title_cfg = &get_config().session.title;
    let user_snippet = truncate_chars(user_message, title_cfg.snippet_max_chars);

    let request = ChatRequest {
        system: Some(TITLE_PROMPT.to_string()),
        messages: vec![ChatMessage {
            role: fuyao_api::MessageRole::User,
            content: Some(format!("用户: {user_snippet}")),
            ..ChatMessage::default()
        }],
    };

    // 标题固定不思考：默认 options（thinking_type=None / reasoning_effort=None）
    let response = provider
        .chat(request, &model_name, StreamOptions::default())
        .await
        .ok()?;
    let content = response.content?;
    let title = clean_title(&content);
    if title.is_empty() { None } else { Some(title) }
}

/// 按字符数截断（不切断多字节字符）
fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// 清洗标题文本
///
/// 去除首尾空白、引号、前缀（"标题:" / "Title:"），限制最大长度。
fn clean_title(raw: &str) -> String {
    let max_len = get_config().session.title.max_len;
    let title = raw.trim();
    // 去引号（中英文）
    let title = title.trim_matches(|c| matches!(c, '"' | '\'' | '「' | '」' | '『' | '』'));
    // 去 "标题:" / "Title:" 前缀（兼容中英文冒号）
    let lower = title.to_lowercase();
    let title = if let Some(stripped) = lower
        .strip_prefix("标题:")
        .or_else(|| lower.strip_prefix("标题："))
        .or_else(|| lower.strip_prefix("title:"))
        .or_else(|| lower.strip_prefix("title："))
    {
        &title[title.len() - stripped.len()..]
    } else {
        title
    };
    let title = title.trim();
    // 限制长度（按字符计，多字节安全）
    if title.chars().count() > max_len {
        let truncated: String = title.chars().take(max_len).collect();
        format!("{truncated}...")
    } else {
        title.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_handles_multibyte() {
        assert_eq!(truncate_chars("你好世界", 2), "你好");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("", 5), "");
    }

    #[test]
    fn clean_title_strips_quotes() {
        assert_eq!(clean_title("\"测试标题\""), "测试标题");
        assert_eq!(clean_title("'标题'"), "标题");
        assert_eq!(clean_title("「标题」"), "标题");
        assert_eq!(clean_title("『标题』"), "标题");
    }

    #[test]
    fn clean_title_strips_prefix() {
        assert_eq!(clean_title("标题: 测试"), "测试");
        assert_eq!(clean_title("Title: test"), "test");
        assert_eq!(clean_title("标题：测试"), "测试");
        assert_eq!(clean_title("title：test"), "test");
    }

    #[test]
    fn clean_title_truncates_long() {
        let long = "a".repeat(100);
        let result = clean_title(&long);
        assert_eq!(result.chars().count(), 80 + 3); // 默认 max_len=80 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn clean_title_empty_after_strip() {
        assert_eq!(clean_title("   "), "");
        assert_eq!(clean_title("\"  \""), "");
    }

    #[test]
    fn clean_title_preserves_normal() {
        assert_eq!(
            clean_title("  关于 Rust 异步的讨论  "),
            "关于 Rust 异步的讨论"
        );
    }
}
