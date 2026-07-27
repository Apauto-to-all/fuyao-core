//! 子任务 session 生命周期事件
//!
//! 派生子任务 session（子代理 / 后台记忆任务 / 经验总结等）时，引擎发出此事件
//! 通知前端「子任务 session 已启动 / 已结束」+ 关联关系（哪个父 session 派生、
//! 由哪个 tool_call 触发、child_session_id 是谁）。
//!
//! 设计动机：派生子 session 是有意义的生命周期信号，独立成事件变体（强类型 + 自描述），
//! 不挂在 ToolCall 的 metadata blob 上——与项目「消息驱动 + 类型驱动」铁律一致。
//!
//! 前端消费：
//! - 收到 `ChildSession(Started)` → 知道 `child_session_id`，开辟独立渲染区
//! - 后续 `base.session_id == child_session_id` 的事件（Chunk / ToolCall / ...）
//!   渲染到该子任务的区
//! - 收到 `ChildSession(Ended)` 或父 ToolResult → 关闭渲染区

use crate::message::EventBase;

/// 子任务 session 事件 envelope
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChildSessionMessage {
    /// 事件元信息（id/timestamp/session_id）
    ///
    /// `base.session_id` 盖父 session 的标签（事件由父 session 上下文发出，
    /// 经父的 per-session 出站通道流转到 UI 出口）。
    pub base: EventBase,
    /// 子任务 session 载荷
    pub payload: ChildSessionPayload,
}

/// 子任务 session 载荷
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChildSessionPayload {
    /// 父 session id（派生方）
    pub parent_session_id: String,
    /// 子 session id（被派生方——前端据此过滤后续事件流）
    pub child_session_id: String,
    /// 派生来源（区分子代理 / 后台任务 / 记忆总结等，便于前端按来源渲染）
    pub origin: ChildSessionOrigin,
    /// 生命周期状态
    pub state: ChildSessionState,
    /// 触发本次派生的 tool_call id（由工具派生时填，非工具派生时为 None）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// 任务描述（来自工具参数或调用方提供，供前端显示）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// 派生来源
///
/// 区分子 session 的用途——前端按来源选择渲染策略（子代理可内联显示进度，
/// 记忆任务可能只显示状态指示器）。
///
/// 加新来源时在此枚举加变体即可，不影响序列化兼容（tag-based）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum ChildSessionOrigin {
    /// 子代理工具派生（同步等结果回喂父 ReAct）
    Subagent,
    // 未来扩展：
    // - 记忆总结任务
    // - 经验提取任务
    // - fire-and-forget 后台任务
}

/// 生命周期状态
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum ChildSessionState {
    /// 已启动（子 session 已创建，即将 / 已经开始执行）
    Started,
    /// 已结束（子 session 已 end_session，不再产事件）
    Ended,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_payload_holds_fields() {
        let msg = ChildSessionMessage {
            base: EventBase::default(),
            payload: ChildSessionPayload {
                parent_session_id: "parent-1".into(),
                child_session_id: "child-1".into(),
                origin: ChildSessionOrigin::Subagent,
                state: ChildSessionState::Started,
                tool_call_id: Some("call_1".into()),
                description: "搜索代码".into(),
            },
        };
        assert_eq!(msg.payload.parent_session_id, "parent-1");
        assert_eq!(msg.payload.child_session_id, "child-1");
        assert_eq!(msg.payload.origin, ChildSessionOrigin::Subagent);
        assert_eq!(msg.payload.state, ChildSessionState::Started);
    }

    #[test]
    fn ended_payload_optional_fields() {
        let msg = ChildSessionMessage {
            base: EventBase::default(),
            payload: ChildSessionPayload {
                parent_session_id: "p".into(),
                child_session_id: "c".into(),
                origin: ChildSessionOrigin::Subagent,
                state: ChildSessionState::Ended,
                tool_call_id: None,
                description: String::new(),
            },
        };
        let json = serde_json::to_string(&msg).expect("序列化失败");
        // tool_call_id=None 与 description=空 都应 skip
        assert!(!json.contains("tool_call_id"));
        assert!(!json.contains("description"));
    }

    #[test]
    fn origin_state_serde_round_trip() {
        let msg = ChildSessionMessage {
            base: EventBase::default(),
            payload: ChildSessionPayload {
                parent_session_id: "p".into(),
                child_session_id: "c".into(),
                origin: ChildSessionOrigin::Subagent,
                state: ChildSessionState::Started,
                tool_call_id: Some("t".into()),
                description: "d".into(),
            },
        };
        let json = serde_json::to_string(&msg).expect("序列化失败");
        let decoded: ChildSessionMessage = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(decoded.payload.origin, ChildSessionOrigin::Subagent);
        assert_eq!(decoded.payload.state, ChildSessionState::Started);
    }
}
