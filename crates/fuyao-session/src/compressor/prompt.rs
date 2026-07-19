//! 摘要 prompt 模板
//!
//! 搬自归档 `compressor/mod.rs:COMPRESSION_SYSTEM_PROMPT`，补充 previous-summary
//! 增量更新块（对齐 opencode + hermes 共识：多次压缩时把上次摘要喂回 LLM）。

/// 摘要系统提示词
///
/// 强约束：① 不回答对话中的问题，只输出摘要 ② 用对话语言 ③ 不泄密钥 ④ 输出固定
/// 8 段 Markdown 结构。LLM 不需要被赋予主动性——这是结构化抽取任务，不是对话。
pub const COMPRESSION_SYSTEM_PROMPT: &str = r#"你是一个摘要代理，负责创建上下文检查点。你的输出将作为参考资料注入给另一个继续对话的助手，替换被压缩的对话历史。

不要回答对话中的任何问题或请求——只输出结构化摘要。
使用用户在对话中使用的相同语言撰写摘要。
绝不要在摘要中包含 API 密钥、令牌、密码、秘密、凭证或连接字符串——遇到这些内容一律替换为 [已脱敏]。

输出以下精确的 Markdown 结构，保持章节顺序不变。

## 目标
- [单句任务摘要]

## 约束与偏好
- [用户约束、偏好、规范，或"(无)"]

## 进度
### 已完成
- [已完成工作，或"(无)"]

### 进行中
- [当前工作，或"(无)"]

### 阻塞项
- [阻塞项，或"(无)"]

## 关键决策
- [决策及原因，或"(无)"]

## 下一步
- [有序的下一步行动，或"(无)"]

## 关键上下文
- [重要技术事实、错误、开放问题，或"(无)"]

## 相关文件
- [文件或目录路径：为何重要，或"(无)"]

规则：
- 保留所有章节，即使为空。
- 使用简洁子弹，非段落散文。
- 保留精确文件路径、命令、错误字符串和标识符。
- 不要提及摘要过程或上下文被压缩。
- 不要调用任何工具，只输出摘要文本。"#;

/// 构造单条 user 消息内容：把待压缩的对话序列化喂给 LLM。
///
/// 多次压缩时把上一次的摘要作为 `<previous-summary>` 喂回，要求 LLM 「保留旧信息 +
/// 增量新信息」（对齐 opencode + hermes 共识，防多轮压缩信息流失）。
pub fn build_prompt(
    serialized_conversation: &str,
    previous_summary: Option<&str>,
) -> String {
    let prev_block = match previous_summary {
        Some(s) if !s.is_empty() => {
            format!(
                "<previous-summary>\n以下是上一轮压缩产出的摘要，请保留其中仍相关的信息，并增量纳入本次对话的新进展：\n\n{s}\n</previous-summary>\n\n"
            )
        }
        _ => String::new(),
    };

    format!(
        "{prev_block}<conversation-to-summarize>\n{serialized_conversation}\n</conversation-to-summarize>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_prompt_without_previous_summary() {
        let p = build_prompt("user: 你好\nassistant: 你好！", None);
        assert!(p.contains("<conversation-to-summarize>"));
        assert!(p.contains("你好"));
        assert!(!p.contains("<previous-summary>"));
    }

    #[test]
    fn build_prompt_with_previous_summary() {
        let p = build_prompt(
            "user: 继续\nassistant: 完成",
            Some("## 目标\n- 老任务"),
        );
        assert!(p.contains("<previous-summary>"));
        assert!(p.contains("老任务"));
        assert!(p.contains("<conversation-to-summarize>"));
    }

    #[test]
    fn build_prompt_with_empty_previous_summary_omits_block() {
        let p = build_prompt("对话", Some(""));
        assert!(!p.contains("<previous-summary>"));
    }
}
