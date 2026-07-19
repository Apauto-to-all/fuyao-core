//! 窗口算法：保留最近窗口的 token 预算切分 + 整 turn 完整性
//!
//! 算法（融合 opencode 简单做法 + 归档 `expand_for_integrity`）：
//! 1. 反向遍历 messages 累加 token 估算，直到达到 keep_tokens 预算 → 切分点
//! 2. 切分点处的 assistant+tool 块若不完整，向前扩大至完整块边界
//! 3. 强制保留最后一条 user + assistant（防活跃任务丢失、防 UI 看不见上一条回复）

use fuyao_api::{Message, MessageKind};

/// 4 字符 ≈ 1 token 的粗估（对齐 zeroclaw + opencode V2）
const CHARS_PER_TOKEN: usize = 4;

/// 窗口切分结果
#[derive(Debug)]
pub struct Window<'a> {
    /// 被压缩的旧部分（喂摘要 LLM）
    pub to_compress: &'a [Message],
    /// 保留的近端窗口（原样留在可见消息流里）
    pub keep_recent: &'a [Message],
}

/// 估算单条消息的 token 数
fn estimate_message_tokens(msg: &Message) -> usize {
    let text_len =
        msg.content.as_deref().unwrap_or("").len() + msg.reasoning.as_deref().unwrap_or("").len();
    // ponytail: 不解析 tool_calls JSON 深度估算——保留粗估，压缩触发偏保守没问题
    let tool_len = msg
        .tool_calls
        .as_ref()
        .map(|v| v.to_string().len())
        .unwrap_or(0);
    text_len.div_ceil(CHARS_PER_TOKEN) + tool_len.div_ceil(CHARS_PER_TOKEN) + 4
}

/// 估算一批消息的总 token 数
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

/// 选择保留窗口：返回 (to_compress, keep_recent)
///
/// 步骤：
/// 1. 反向累加 token 到 keep_tokens → 找切分点
/// 2. 扩大至 turn 完整边界（不切断 assistant+tool_result 块）
/// 3. 强制最后一条 user/assistant 留 tail
pub fn select_recent<'a>(messages: &'a [Message], keep_tokens: usize) -> Window<'a> {
    if messages.is_empty() {
        return Window {
            to_compress: &[],
            keep_recent: &[],
        };
    }

    // 反向累加找切分点
    let mut accumulated = 0usize;
    let mut cut = messages.len();
    for (i, msg) in messages.iter().enumerate().rev() {
        let cost = estimate_message_tokens(msg);
        if accumulated + cost > keep_tokens && i < messages.len() - 1 {
            cut = i + 1;
            break;
        }
        accumulated += cost;
        cut = i;
    }

    // 扩大至整 turn 完整边界
    let cut = expand_for_integrity(messages, cut);

    // ponytail: 不再单独强制最后一条 user/assistant——
    // 反向累加必然包含 messages[len-1]，且 expand_for_integrity 保证 tool 块完整

    Window {
        to_compress: &messages[..cut],
        keep_recent: &messages[cut..],
    }
}

/// 向前扩大窗口，确保 assistant+tool 块完整（搬自归档 `compressor/mod.rs:70`）
///
/// 从切分点 `cut` 向前扫描：若 messages[cut] 是 tool 或带 tool_calls 的 assistant，
/// 说明这一块还没完，需要继续向前找到块的起点。
/// 扫描上限：messages.len()（最差就保留所有消息——这时不该压缩）。
pub fn expand_for_integrity(messages: &[Message], cut: usize) -> usize {
    let total = messages.len();
    if cut >= total || cut == 0 {
        return cut;
    }

    // 切分点处是安全消息（user/system/普通 assistant）：无需扩大
    let first = &messages[cut];
    if first.role != "tool" && first.role != "assistant" {
        return cut;
    }
    // 普通文本 assistant（无 tool_calls）：本身是安全边界
    if first.role == "assistant" && first.tool_calls.is_none() {
        return cut;
    }
    // 压缩边界消息：安全边界
    if first.kind == MessageKind::Compaction {
        return cut;
    }

    // 向前找 assistant+tool 块的完整起点
    let mut scan = cut;
    while scan > 0 {
        let msg = &messages[scan];

        if msg.role == "tool" {
            // tool 消息被切断了，需要向前找对应的 assistant
            scan -= 1;
            continue;
        }

        if msg.role == "assistant" {
            if let Some(ref tool_calls) = msg.tool_calls {
                // 检查这个 assistant 的所有 tool_call_id 是否在后续消息中都有 result
                // ponytail: 解析出 owned id 列表避免借用冲突（call_ids 跨整个 if 块使用）
                let call_ids: Vec<String> =
                    serde_json::from_value::<Vec<serde_json::Value>>(tool_calls.clone())
                        .map(|calls| {
                            calls
                                .iter()
                                .filter_map(|tc| {
                                    tc.get("id").and_then(|v| v.as_str()).map(String::from)
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                let call_id_refs: std::collections::HashSet<&str> =
                    call_ids.iter().map(String::as_str).collect();

                // 收集 cut 之后的 tool result id（注：这里看 messages[scan+1..total]）
                let result_ids: std::collections::HashSet<&str> = messages[scan + 1..total]
                    .iter()
                    .filter(|m| m.role == "tool")
                    .filter_map(|m| m.tool_call_id.as_deref())
                    .collect();

                // 所有 tool_call 都有 result → 块完整，scan 是合法切分点
                if call_id_refs.is_subset(&result_ids) {
                    return scan;
                }
                // 缺 result，继续向前扩大
                scan -= 1;
                continue;
            }
            // assistant 无 tool_calls → 安全边界
            return scan;
        }

        // user/system/compaction → 安全边界
        return scan;
    }

    0
}

/// 把一批消息序列化为喂摘要 LLM 的纯文本
///
/// 格式：`[role]: content`，对齐 opencode `serialize()`。
/// tool_calls 用 JSON 字符串简短表示。tool 输出超过 2000 字符截断（防止巨大结果污染摘要）。
const TOOL_OUTPUT_MAX_CHARS: usize = 2000;

pub fn serialize_for_summary(messages: &[Message]) -> String {
    let mut parts = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.kind == MessageKind::Compaction {
            // 历史压缩边界：摘要正文直接呈现
            let content = msg.content.as_deref().unwrap_or("");
            parts.push(format!("[历史摘要]:\n{}", content));
            continue;
        }
        let role_label = match msg.role.as_str() {
            "user" => "[User]",
            "assistant" => "[Assistant]",
            "tool" => "[Tool result]",
            "system" => "[System]",
            _ => "[Other]",
        };
        let mut line = match msg.role.as_str() {
            "tool" => {
                // tool 输出过长截断
                let content = msg.content.as_deref().unwrap_or("");
                let truncated = if content.len() > TOOL_OUTPUT_MAX_CHARS {
                    format!("{}...(已截断)", &content[..TOOL_OUTPUT_MAX_CHARS])
                } else {
                    content.to_string()
                };
                format!("{role_label}: {truncated}")
            }
            _ => {
                let content = msg.content.as_deref().unwrap_or("");
                let tool_info = msg
                    .tool_calls
                    .as_ref()
                    .map(|tc| format!(" [tool_calls: {tc}]"))
                    .unwrap_or_default();
                format!("{role_label}: {content}{tool_info}")
            }
        };
        if let Some(reasoning) = msg.reasoning.as_deref()
            && !reasoning.is_empty()
        {
            line.push_str(&format!("\n  [推理]: {reasoning}"));
        }
        parts.push(line);
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_basic() {
        let msg = Message::user("12345678".to_string()); // 8 字符 → 2 token + 4 开销 = 6
        assert_eq!(estimate_message_tokens(&msg), 6);
    }

    #[test]
    fn select_recent_keeps_tail_within_budget() {
        // 4 条消息，每条 ~10 token；预算 25 token（够装 ~2 条）
        let msgs: Vec<Message> = (0..4)
            .map(|i| Message::user(format!("消息_{i}_{}", "x".repeat(20))))
            .collect();
        let w = select_recent(&msgs, 25);
        // tail 至少保留最后 1-2 条
        assert!(!w.keep_recent.is_empty());
        // head + tail = 全集
        assert_eq!(w.to_compress.len() + w.keep_recent.len(), msgs.len());
    }

    #[test]
    fn select_recent_returns_empty_for_empty_input() {
        let w: Window<'_> = select_recent(&[], 1000);
        assert!(w.to_compress.is_empty());
        assert!(w.keep_recent.is_empty());
    }

    #[test]
    fn select_recent_keeps_everything_when_budget_large() {
        let msgs: Vec<Message> = (0..3).map(|i| Message::user(format!("m{i}"))).collect();
        let w = select_recent(&msgs, 100_000);
        assert!(w.to_compress.is_empty());
        assert_eq!(w.keep_recent.len(), 3);
    }

    #[test]
    fn expand_for_integrity_safe_at_user_boundary() {
        let msgs = vec![
            Message::user("u1".to_string()),
            Message::assistant(Some("a1".to_string())),
            Message::user("u2".to_string()), // 切分点在这之后安全
        ];
        // cut=2 指向 "u2"，是安全边界
        assert_eq!(expand_for_integrity(&msgs, 2), 2);
    }

    #[test]
    fn expand_for_integrity_moves_past_tool_result() {
        let mut assistant_with_tc = Message::assistant(None);
        assistant_with_tc.tool_calls = Some(serde_json::json!([{"id": "call_1"}]));
        let msgs = vec![
            Message::user("u1".to_string()),
            assistant_with_tc, // assistant + tool_call
            Message::tool_result("call_1".into(), "结果".into()), // tool result
            Message::user("u2".to_string()),
        ];
        // cut=2 指向 "tool result"，需要向前扩大到 assistant 之前的 user
        // assistant 的 tool_call 在 messages[1+1..] = messages[2..] 里有结果（call_1）
        // 所以 messages[1] (assistant) 的块完整 → 切分点 = 1
        assert_eq!(expand_for_integrity(&msgs, 2), 1);
    }

    #[test]
    fn serialize_includes_role_labels() {
        let msgs = vec![
            Message::user("你好".to_string()),
            Message::assistant(Some("回复".to_string())),
        ];
        let s = serialize_for_summary(&msgs);
        assert!(s.contains("[User]: 你好"));
        assert!(s.contains("[Assistant]: 回复"));
    }

    #[test]
    fn serialize_truncates_long_tool_output() {
        let long = "y".repeat(3000);
        let msgs = vec![Message::tool_result("c1".into(), long)];
        let s = serialize_for_summary(&msgs);
        assert!(s.contains("已截断"));
    }
}
