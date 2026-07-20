//! 会话标题自动生成
//!
//! 首轮对话后异步生成简短标题：
//! - 用用户消息 + AI 回答（各截断 `[session.title] snippet_max_chars` 字符）喂给 LLM
//! - 模型优先级：`[models.fast]` → 当前引擎模型 → 放弃
//! - 内部自建 Provider（`create_provider_with_model`），支持 fast 跨 Provider 配置
//! - 用非流式 `provider.chat()`（标题是短文本，无需流式增量）
//! - 只生成标题文本；落库（`SessionStore::update_title`）与事件发布由调用方负责（职责分离）

use fuyao_api::{AgentPaths, get_config};
use fuyao_provider::{ChatMessage, ChatRequest, Provider, create_provider_with_model};

/// 标题生成系统提示词
const TITLE_PROMPT: &str = "为以下对话生成一个简短的描述性标题（3-7 个词）。\
标题应概括对话的主要主题或意图。使用与用户相同的语言撰写标题。\
只返回标题文本，不要加引号、不要加标点结尾、不要加前缀。";

/// 生成会话标题
///
/// 模型优先级：`[models.fast]` → 当前引擎模型 → 放弃（返回 None）。
/// fast 未配置或调用失败时自动回退到当前引擎模型；两者都失败则不重命名。
///
/// # 参数
/// - `user_message`：用户消息原文（内部截断）
/// - `assistant_response`：AI 回答原文（内部截断）
/// - `main_model_id`：当前引擎使用的模型 ID（格式 provider_id/model_id），fast 不可用时回退
/// - `agent_paths`：Agent 三层目录（用于 Provider 创建与注册表查找）
///
/// # 返回
/// - `Some(String)`：清洗后的标题（已去引号/前缀/限长）
/// - `None`：fast 与主模型都失败，或生成结果为空
pub async fn maybe_generate_title(
    user_message: &str,
    assistant_response: &str,
    main_model_id: &str,
    agent_paths: &AgentPaths,
) -> Option<String> {
    let config = get_config();

    // 1. 优先用 fast 模型（便宜快速）
    if let Some(fast_ref) = config.models.fast.as_ref()
        && !fast_ref.model.is_empty()
    {
        match generate_title(
            user_message,
            assistant_response,
            &fast_ref.model,
            agent_paths,
        )
        .await
        {
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
    generate_title(user_message, assistant_response, main_model_id, agent_paths).await
}

/// 调用 LLM 生成标题（单次尝试）
///
/// 内部完成：创建 Provider → 截断输入 → 发非流式请求 → 清洗标题。
async fn generate_title(
    user_message: &str,
    assistant_response: &str,
    model_id: &str,
    agent_paths: &AgentPaths,
) -> Option<String> {
    let (_provider_id, model_name, provider) = create_provider_with_model(model_id, agent_paths)?;

    let title_cfg = &get_config().session.title;
    let user_snippet = truncate_chars(user_message, title_cfg.snippet_max_chars);
    let assistant_snippet = truncate_chars(assistant_response, title_cfg.snippet_max_chars);

    let request = ChatRequest {
        system: Some(TITLE_PROMPT.to_string()),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(format!("用户: {user_snippet}\n\n助手: {assistant_snippet}")),
            ..ChatMessage::default()
        }],
    };

    let response = provider.chat(request, &model_name).await.ok()?;
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
