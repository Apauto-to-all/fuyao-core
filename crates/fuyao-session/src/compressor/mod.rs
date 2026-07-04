//! 上下文压缩模块
//!
//! 压缩走引擎内流程，并入 session 管理钩子：
//! 触发后注入压缩引导消息（Guide 队列），引擎自动完成当前工具调用后消费引导消息，
//! LLM 生成摘要，session hooks 在 output_observe 中捕获摘要响应后触发 split_session。
//!
//! 子模块：
//! - tracker：压缩触发追踪器，简化阈值检测

pub mod tracker;

use fuyao_api::Message;

/// 保留窗口上限：最多保留最近 6 条消息
pub(crate) const MAX_RECENT_WINDOW: usize = 6;

/// 保留窗口硬上限：完整性扩大后最多不超过此值
pub(crate) const MAX_RECENT_WINDOW_HARD: usize = MAX_RECENT_WINDOW * 2;

/// 压缩系统提示词
pub(crate) const COMPRESSION_SYSTEM_PROMPT: &str = r#"你是一个摘要代理，负责创建上下文检查点。你的输出将作为参考资料注入给另一个继续对话的助手，替换被压缩的对话历史。

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

/// 向前扩大窗口，确保 assistant+tool 块完整
///
/// 从 recent_start 位置向前扫描，确保分割点处的 assistant+tool 块不被截断。
/// 扫描范围受 MAX_RECENT_WINDOW_HARD 限制。
pub(crate) fn expand_for_integrity(messages: &[Message], recent_start: usize) -> usize {
    let total = messages.len();
    let hard_limit = total.saturating_sub(MAX_RECENT_WINDOW_HARD);

    // 边界检查
    if recent_start >= total {
        return total;
    }

    // 当前分割点已经是最前面，无法再扩大
    if recent_start <= hard_limit {
        return recent_start;
    }

    // 无需扩大：分割点处是安全消息（user/system），后面没有不完整的块
    let first = &messages[recent_start];
    if first.role != "tool" && first.role != "assistant" {
        return recent_start;
    }

    // 向前找 assistant+tool 块的完整起点
    let mut scan = recent_start;
    while scan > hard_limit {
        let msg = &messages[scan];

        if msg.role == "tool" {
            // tool 消息被切断了，需要向前找对应的 assistant
            scan -= 1;
            continue;
        }

        if msg.role == "assistant" {
            if let Some(ref tool_calls) = msg.tool_calls {
                // 检查这个 assistant 的所有 tool_call_id 是否在后续消息中都有 result
                let call_ids: std::collections::HashSet<String> = if let Ok(calls) =
                    serde_json::from_value::<Vec<serde_json::Value>>(tool_calls.clone())
                {
                    calls
                        .iter()
                        .filter_map(|tc| tc.get("id").and_then(|v| v.as_str()))
                        .map(|s| s.to_string())
                        .collect()
                } else {
                    std::collections::HashSet::new()
                };

                // 扫描后续消息收集已有的 tool result id
                let mut result_ids = std::collections::HashSet::new();
                for m in &messages[scan + 1..total] {
                    if m.role == "tool"
                        && let Some(ref tc_id) = m.tool_call_id
                    {
                        result_ids.insert(tc_id.clone());
                    }
                }

                // 所有 tool_call 都有 result → 这个块完整
                if call_ids.is_subset(&result_ids) {
                    return scan;
                }
                // 缺少 result，继续向前扩大
                scan -= 1;
                continue;
            }
            // assistant 无 tool_calls → 安全边界
            return scan;
        }

        // user/system 消息 → 安全边界
        return scan;
    }

    hard_limit
}
