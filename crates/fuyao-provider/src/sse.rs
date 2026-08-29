//! SSE 流式解码底座（协议无关，纯函数）
//!
//! 两段职责：字节 → 完整行（[`LineAssembler`] 增量组装，跨 chunk 半行缓冲）；
//! 行 → data 载荷 → JSON（[`data_payload`] / [`parse_data_json`]，坏行跳过并
//! WARN 的统一容错策略）。「JSON 载荷 → 事件」的语义解析归各协议模块自行
//! 实现（openai 侧认 `[DONE]` 终止标记，anthropic 侧按 JSON `type` 字段分发）。

/// SSE 字节流增量组装器：吃原始网络 chunk，吐完整 SSE 行
///
/// 在原始字节上按 `\n` 切行——`\n` 是单字节且不会出现在 UTF-8 多字节序列
/// 内部，先切行后解码天然安全：跨 chunk 的半行 / 半个多字节字符自然滞留
/// 缓冲等待续包；行内非法 UTF-8 以替换字符（U+FFFD）顶替，坏字节会在下游
/// JSON 解析处显式报错，绝不因无法消费而停滞。游标推进代替逐行搬移，
/// 整 chunk 只做一次前缀释放。
pub(crate) struct LineAssembler {
    /// 未切出完整行的尾部字节（可能含不完整的多字节字符）
    buf: Vec<u8>,
}

impl LineAssembler {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// 喂入一个网络 chunk，返回其中所有完整行（不含换行符，可能带尾随 `\r`）
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut lines = Vec::new();
        let mut cursor = 0;
        while let Some(rel) = self.buf[cursor..].iter().position(|&b| b == b'\n') {
            let nl = cursor + rel;
            lines.push(String::from_utf8_lossy(&self.buf[cursor..nl]).into_owned());
            cursor = nl + 1;
        }
        // 释放已切出的前缀，只留不完整行尾
        self.buf.drain(..cursor);
        lines
    }
}

impl Default for LineAssembler {
    fn default() -> Self {
        Self::new()
    }
}

/// 提取 SSE 行的 data 载荷：空行 / 注释行（`:` 开头）/ 非 `data:` 行返回 None
pub(crate) fn data_payload(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return None;
    }
    line.strip_prefix("data:").map(str::trim)
}

/// data 载荷解析为 JSON：解析失败的坏行跳过并 WARN
///
/// 容错降级：单行损坏（代理注入垃圾 / 传输截断）只丢一行并留痕，
/// 不中断整条流——后续行照常解码。
pub(crate) fn parse_data_json(data: &str) -> Option<serde_json::Value> {
    match serde_json::from_str(data) {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!(cause = %e, "SSE data 行 JSON 解析失败，跳过该行");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单 chunk 多行：一次切出全部完整行
    #[test]
    fn assembler_multiple_lines_in_one_chunk() {
        let mut a = LineAssembler::new();
        let lines = a.push(b"data: {\"a\":1}\ndata: {\"b\":2}\n\n");
        assert_eq!(
            lines,
            vec![
                "data: {\"a\":1}".to_string(),
                "data: {\"b\":2}".to_string(),
                String::new()
            ]
        );
    }

    /// chunk 边界切断一行：半行滞留缓冲，下个 chunk 到齐后切出
    #[test]
    fn assembler_line_split_across_chunks() {
        let mut a = LineAssembler::new();
        // 半行无换行符：滞留缓冲
        assert!(a.push(b"data: {\"a\"").is_empty());
        // 续包补齐换行符：完整行切出
        assert_eq!(a.push(b":1}\n"), vec!["data: {\"a\":1}".to_string()]);
        assert_eq!(a.push(b"data: [DONE]\n"), vec!["data: [DONE]".to_string()]);
    }

    /// 多字节 UTF-8 字符被 chunk 边界切断：滞留缓冲等待续包，不产出坏行
    #[test]
    fn assembler_multibyte_char_split_across_chunks() {
        let mut a = LineAssembler::new();
        // "你好" = E4 BD A0 E5 A5 BD，在两 chunk 间切断
        assert!(a.push(b"data: \xE4\xBD").is_empty());
        let lines = a.push(b"\xA0\xE5\xA5\xBD\n");
        assert_eq!(lines, vec!["data: 你好".to_string()]);
    }

    /// 尾部无换行符的最后一行：滞留缓冲（SSE 规范每事件以空行结尾，此路径不产出）
    #[test]
    fn assembler_trailing_partial_line_stays_buffered() {
        let mut a = LineAssembler::new();
        let lines = a.push(b"data: tail-no-newline");
        assert!(lines.is_empty());
        assert_eq!(a.push(b"\n"), vec!["data: tail-no-newline".to_string()]);
    }

    /// 行内非法 UTF-8 字节不得使解码停滞：坏字节以替换字符顶替照常切行，
    /// 后续行不受影响继续解码——组装器永不得因坏字节静默吞流
    #[test]
    fn assembler_invalid_utf8_never_stalls() {
        let mut a = LineAssembler::new();
        // 首字节 0xFF 是非法 UTF-8 起始且非不完整序列
        let lines = a.push(b"\xff\xfe\xfd junk\n");
        assert_eq!(lines.len(), 1, "非法字节行必须切出而非滞留");
        assert!(lines[0].contains('\u{FFFD}'), "非法字节以替换字符顶替");
        // 后续行不受影响，继续正常解码
        assert_eq!(
            a.push(b"data: {\"ok\":1}\n"),
            vec!["data: {\"ok\":1}".to_string()]
        );
    }

    /// CRLF 行尾：\r 随行带出，去除 \r 归语义解析侧的 trim 处理
    #[test]
    fn assembler_keeps_trailing_cr() {
        let mut a = LineAssembler::new();
        assert_eq!(a.push(b"data: x\r\n"), vec!["data: x\r".to_string()]);
    }

    /// 空 chunk：无行切出、无 panic，缓冲保持
    #[test]
    fn assembler_empty_chunk() {
        let mut a = LineAssembler::new();
        assert!(a.push(b"data: {\"a\"").is_empty());
        assert!(a.push(b"").is_empty());
        assert_eq!(a.push(b":1}\n"), vec!["data: {\"a\":1}".to_string()]);
    }

    /// data 载荷提取：空行 / 注释 / 非 data 行无载荷；data 行去前缀去空白
    #[test]
    fn data_payload_extracts_stripped_payload() {
        assert_eq!(data_payload(""), None);
        assert_eq!(data_payload(": keep-alive"), None);
        assert_eq!(data_payload("event: ping"), None);
        assert_eq!(data_payload("data: {\"a\":1}"), Some("{\"a\":1}"));
        assert_eq!(data_payload("data:no-space"), Some("no-space"));
        assert_eq!(data_payload("data: tail\r"), Some("tail"));
    }

    /// 坏行策略：JSON 解析失败的载荷跳过返回 None，合法载荷照常解析
    #[test]
    fn parse_data_json_skips_bad_line_and_parses_good() {
        assert!(parse_data_json("{invalid}").is_none());
        assert!(parse_data_json("not json").is_none());
        let value = parse_data_json(r#"{"type":"ping"}"#).unwrap();
        assert_eq!(value["type"], "ping");
    }
}
