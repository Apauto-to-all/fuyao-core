//! 窗口算法：保留最近窗口的 token 预算切分 + 整 turn 完整性
//!
//! 仅用于决定 apply 时「保留多少近账」（keep_recent），不再参与摘要请求构造——
//! 摘要请求把全部消息原样发给 LLM，不切窗口、不序列化，前缀缓存完整命中。

use fuyao_api::{Message, MessageKind, MessageRole};

/// 4 字符 ≈ 1 token 的粗估
const CHARS_PER_TOKEN: usize = 4;

/// 窗口切分结果
#[derive(Debug)]
pub struct Window<'a> {
    /// 保留的近端窗口（apply 时原样留在可见消息流里）
    pub keep_recent: &'a [Message],
}

/// 估算单条消息的 token 数
fn estimate_message_tokens(msg: &Message) -> usize {
    // 每张图固定占用 token（不按 base64 字符数估算，避免撑爆触发误压缩）
    let tokens_per_image = fuyao_api::get_config().session.compression.tokens_per_image;
    let text_len =
        msg.content.as_deref().unwrap_or("").len() + msg.reasoning.as_deref().unwrap_or("").len();
    // 不解析 tool_calls JSON 深度估算——保留粗估，压缩触发偏保守没问题
    let tool_len = msg
        .tool_calls
        .as_ref()
        .map(|v| v.to_string().len())
        .unwrap_or(0);
    text_len.div_ceil(CHARS_PER_TOKEN)
        + tool_len.div_ceil(CHARS_PER_TOKEN)
        + 4
        + msg.images.len() * tokens_per_image
}

/// 选择保留窗口：返回 keep_recent
///
/// 步骤：
/// 1. 反向累加 token 到 keep_tokens → 找切分点
/// 2. 扩大至 turn 完整边界（不切断 assistant+tool_result 块）
///
/// `keep_tokens` 语义：从消息列表尾部向前，最多保留这么多 token 的消息。
/// 传 0 表示一条都不保留（keep_recent 为空切片）。
pub fn select_recent<'a>(messages: &'a [Message], keep_tokens: usize) -> Window<'a> {
    if messages.is_empty() {
        return Window { keep_recent: &[] };
    }

    // 反向累加找切分点：从最后一条向前累加 token，累计超过 keep_tokens 就在该位置切分
    let mut accumulated = 0usize;
    let mut cut = messages.len();
    for (i, msg) in messages.iter().enumerate().rev() {
        let cost = estimate_message_tokens(msg);
        if accumulated + cost > keep_tokens {
            cut = i + 1;
            break;
        }
        accumulated += cost;
        cut = i;
    }

    // 扩大至整 turn 完整边界
    let cut = expand_for_integrity(messages, cut);

    Window {
        keep_recent: &messages[cut..],
    }
}

/// 向前扩大窗口，确保 assistant+tool 块完整
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
    if !matches!(first.role, MessageRole::Tool | MessageRole::Assistant) {
        return cut;
    }
    // 普通文本 assistant（无 tool_calls）：本身是安全边界
    if matches!(first.role, MessageRole::Assistant) && first.tool_calls.is_none() {
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

        if matches!(msg.role, MessageRole::Tool) {
            // tool 消息被切断了，需要向前找对应的 assistant
            scan -= 1;
            continue;
        }

        if matches!(msg.role, MessageRole::Assistant) {
            if let Some(ref tool_calls) = msg.tool_calls {
                // 检查这个 assistant 的所有 tool_call_id 是否在后续消息中都有 result
                // 解析出 owned id 列表避免借用冲突
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

                // 收集 cut 之后的 tool result id
                let result_ids: std::collections::HashSet<&str> = messages[scan + 1..total]
                    .iter()
                    .filter(|m| matches!(m.role, MessageRole::Tool))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_basic() {
        let msg = Message::user("12345678".to_string()); // 8 字符 → 2 token + 4 开销 = 6
        assert_eq!(estimate_message_tokens(&msg), 6);
    }

    #[test]
    fn estimate_tokens_with_images_adds_fixed_per_image() {
        let tokens_per_image = fuyao_api::get_config().session.compression.tokens_per_image;
        // 无图：8 字符 → 2 + 4 开销 = 6
        let msg = Message::user("12345678".to_string());
        assert_eq!(estimate_message_tokens(&msg), 6);

        // 带 2 图：文本部分不变，每图固定 +1000
        let img = fuyao_api::ImageContent {
            mime_type: "image/png".into(),
            data: "x".repeat(100_000), // base64 巨大也不按字符算——固定值
        };
        let msg_with_images =
            Message::user_with_images("12345678".to_string(), vec![img.clone(), img]);
        assert_eq!(
            estimate_message_tokens(&msg_with_images),
            6 + 2 * tokens_per_image
        );
    }

    #[test]
    fn select_recent_keeps_tail_within_budget() {
        let msgs: Vec<Message> = (0..4)
            .map(|i| Message::user(format!("消息_{i}_{}", "x".repeat(20))))
            .collect();
        let w = select_recent(&msgs, 25);
        assert!(!w.keep_recent.is_empty());
    }

    #[test]
    fn select_recent_returns_empty_for_empty_input() {
        let w: Window<'_> = select_recent(&[], 1000);
        assert!(w.keep_recent.is_empty());
    }

    #[test]
    fn select_recent_keeps_everything_when_budget_large() {
        let msgs: Vec<Message> = (0..3).map(|i| Message::user(format!("m{i}"))).collect();
        let w = select_recent(&msgs, 100_000);
        assert_eq!(w.keep_recent.len(), 3);
    }

    #[test]
    fn expand_for_integrity_safe_at_user_boundary() {
        let msgs = vec![
            Message::user("u1".to_string()),
            Message::assistant(Some("a1".to_string())),
            Message::user("u2".to_string()),
        ];
        assert_eq!(expand_for_integrity(&msgs, 2), 2);
    }

    #[test]
    fn expand_for_integrity_moves_past_tool_result() {
        let mut assistant_with_tc = Message::assistant(None);
        assistant_with_tc.tool_calls = Some(serde_json::json!([{"id": "call_1"}]));
        let msgs = vec![
            Message::user("u1".to_string()),
            assistant_with_tc,
            Message::tool_result("call_1".into(), "结果".into()),
            Message::user("u2".to_string()),
        ];
        assert_eq!(expand_for_integrity(&msgs, 2), 1);
    }
}
