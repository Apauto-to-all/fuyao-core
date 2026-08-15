//! 工具调用数据
//!
//! 一次工具调用的中立表示，流式聚合、非流式解析、历史落库与回放共用；
//! 供应商协议嵌套 wire 形态的构造与解析属于协议实现层职责，本类型不承载。

/// 一次工具调用的中立表示：id / 名称 / 参数 JSON 字符串，跨引擎与供应商接缝通用
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCallData {
    /// 工具调用 ID（关联 tool 结果消息）
    pub id: String,
    /// 工具名称
    pub name: String,
    /// 工具参数（JSON 字符串原串，由消费方按需解析）
    pub arguments: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_data_serde_roundtrip() {
        // 序列化为 flat 三字段形态，反序列化无损还原
        let tc = ToolCallData {
            id: "call_1".into(),
            name: "bash".into(),
            arguments: r#"{"cmd":"ls"}"#.into(),
        };
        let json = serde_json::to_string(&tc).unwrap();
        assert_eq!(
            json,
            r#"{"id":"call_1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}"#
        );
        let back: ToolCallData = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, tc.id);
        assert_eq!(back.name, tc.name);
        assert_eq!(back.arguments, tc.arguments);
    }

    #[test]
    fn tool_call_data_clone_preserves_fields() {
        let tc = ToolCallData {
            id: "call_2".into(),
            name: "read".into(),
            arguments: "{}".into(),
        };
        let cloned = tc.clone();
        assert_eq!(cloned.id, tc.id);
        assert_eq!(cloned.name, tc.name);
        assert_eq!(cloned.arguments, tc.arguments);
    }
}
