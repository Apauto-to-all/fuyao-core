//! 会话标题自动生成
//!
//! 首轮用户消息后立即异步生成简短标题（不等 AI 回复，避免长回复拖累延迟）：
//! - 仅用用户首条消息（截断 `[session.title] snippet_max_chars` 字符）喂给 LLM
//! - 模型优先级：`[models.fast]` → 当前引擎模型 → 放弃
//! - 从引擎级共享的 ProviderRegistry 取 Provider 实例（fast 可能跨 Provider）
//! - 用非流式 `provider.chat()`（标题是短文本，无需流式增量）
//! - 只生成标题文本；落库（`SessionStore::update_session`）与事件发布由调用方负责（职责分离）
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
///
/// 结构：身份锚定 + 任务 + 规则 + 示例 四段。核心防跑偏点：
/// - 首句钉死「只输出标题」，压制把用户任务当成待执行指令而顺势接话
/// - 长度用字符数表达（「词」对中文无边界，约束无效）
/// - 绝不回应问题 / 标题禁含过程词（summarizing、generating）
/// - 寒暄类输入也要产出有意义结果，不允许拒绝或抱怨
const TITLE_PROMPT: &str = "你是标题生成器，只输出一个会话标题，除此之外不输出任何内容。\n\
<任务>\n\
生成一个简短标题，帮助用户日后找到这次对话。\n\
遵守 <规则> 中的全部规则。\n\
参考 <示例> 了解好标题的样子。\n\
输出必须是：\n\
- 单行\n\
- 不超过 50 个字符\n\
- 无任何解释\n\
</任务>\n\
<规则>\n\
- 必须使用与被命名消息相同的语言\n\
- 标题语法正确、读起来自然，禁止词藻堆砌\n\
- 标题中不得出现工具名（如 \"read tool\"、\"bash tool\"、\"edit tool\"）\n\
- 聚焦用户日后需要检索的主要主题或问题\n\
- 变换措辞，避免重复模式（如总是以 \"Analyzing\" 开头）\n\
- 提到文件时，聚焦用户想对这个文件做什么，而非仅仅提到了文件\n\
- 原样保留：技术术语、数字、文件名、HTTP 状态码\n\
- 删除虚词：the、this、my、a、an\n\
- 不臆测技术栈\n\
- 不使用工具\n\
- 绝不回应问题，只生成对话标题\n\
- 标题中绝不出现 \"summarizing\"、\"generating\" 这类过程词\n\
- 不要声称无法生成标题，也不要抱怨输入\n\
- 即使输入极少，也始终输出有意义的内容\n\
- 用户消息很短或只是寒暄（如 \"hello\"、\"lol\"、\"hey\"）时，生成反映其语气或意图的标题（如问候、快速确认、闲聊等）\n\
</规则>\n\
<示例>\n\
「帮我调试生产环境的 500 错误」→ 调试生产环境 500 错误\n\
「帮我把 user service 重构一下」→ 重构 user service\n\
「为什么 app.js 一直失败」→ app.js 失败排查\n\
「实现一下接口限流」→ 接口限流实现\n\
「怎么把 postgres 接到我的 API 上」→ postgres 接入 API\n\
「React hooks 有哪些最佳实践」→ React hooks 最佳实践\n\
「@src/auth.ts 能不能加上 refresh token 支持」→ auth 增加 refresh token 支持\n\
「@utils/parser.ts 这个写坏了」→ parser 修复\n\
「看看 @config.json」→ config 检查\n\
「@App.tsx 加个暗色模式开关」→ App 暗色模式开关\n\
</示例>";

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
            content: Some(format!("请为以下对话生成标题：\n{user_snippet}")),
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
/// 剥离思考段 → 取首个非空行 → 去除首尾空白、引号、前缀（"标题:" / "Title:"）、
/// 句尾标点，限制最大长度。
fn clean_title(raw: &str) -> String {
    let max_len = get_config().session.title.max_len;
    // 剥离思考型模型可能混入 content 的 <think>...</think> 段
    let no_think = strip_think_blocks(raw);
    // 多行输出只取首个非空行，丢弃其余解释性内容
    let title = no_think
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
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
    // 去句尾标点（中英文句号 / 叹号 / 问号，含省略号与重复形态；交替剥离尾随空白）
    let title = title.trim_end_matches(|c| {
        matches!(c, '。' | '．' | '.' | '！' | '!' | '？' | '?' | ' ' | '\t')
    });
    // 限制长度（按字符计，多字节安全）
    if title.chars().count() > max_len {
        let truncated: String = title.chars().take(max_len).collect();
        format!("{truncated}...")
    } else {
        title.to_string()
    }
}

/// 剥离 `<think>...</think>` 思考段
///
/// 无闭合标签时，`<think>` 起的全部剩余内容均视为思考段丢弃，
/// 避免半截推理文本混入标题。
fn strip_think_blocks(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("<think>") {
        result.push_str(&rest[..start]);
        match rest[start..].find("</think>") {
            Some(rel_end) => rest = &rest[start + rel_end + "</think>".len()..],
            None => return result,
        }
    }
    result.push_str(rest);
    result
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
    fn clean_title_takes_first_nonempty_line() {
        assert_eq!(clean_title("标题\n后续解释行"), "标题");
        assert_eq!(clean_title("\n\n   \n跳过空行后的标题"), "跳过空行后的标题");
        assert_eq!(clean_title("第一行\n第二行\n第三行"), "第一行");
    }

    #[test]
    fn clean_title_strips_trailing_punctuation() {
        assert_eq!(clean_title("修复登录崩溃。"), "修复登录崩溃");
        assert_eq!(clean_title("修复登录崩溃!"), "修复登录崩溃");
        assert_eq!(clean_title("为什么报错？"), "为什么报错");
        assert_eq!(clean_title("等等..."), "等等");
        assert_eq!(clean_title("标题。。 "), "标题");
    }

    #[test]
    fn strip_think_blocks_removes_closed_blocks() {
        assert_eq!(strip_think_blocks("<think>推理过程</think>标题"), "标题");
        assert_eq!(
            strip_think_blocks("前<think>a</think>中<think>b</think>后"),
            "前中后"
        );
    }

    #[test]
    fn strip_think_blocks_drops_unclosed_tail() {
        assert_eq!(strip_think_blocks("<think>未闭合的推理"), "");
        assert_eq!(strip_think_blocks("标题在前<think>未闭合"), "标题在前");
    }

    #[test]
    fn clean_title_strips_think_then_takes_title() {
        assert_eq!(
            clean_title("<think>先分析用户意图……</think>\n真正标题"),
            "真正标题"
        );
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
