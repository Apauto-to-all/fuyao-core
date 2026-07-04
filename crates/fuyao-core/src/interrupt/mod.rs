//! 中断子系统
//!
//! 管理中断的完整生命周期：检测、分发、处理。
//! 中断输出事件已由 InputDispatcher dispatch 管道统一发出，
//! 各子模块只负责内部状态处理和增量结果。
//!
//! 场景分文件处理：
//! - `llm_output`：LLM 流式输出中中断
//! - `llm_toolcall`：LLM 工具调用流中中断
//! - `tool_exec`：工具执行中中断
//! - `retry_backoff`：LLM 重试/退避期间中断
//! - `idle`：ReAct 循环间隙 / 无活跃轮次中断

pub(crate) mod idle;
pub(crate) mod llm_output;
pub(crate) mod llm_toolcall;
pub(crate) mod retry_backoff;
pub(crate) mod tool_exec;

use fuyao_api::message::event_input::InterruptSource;
use fuyao_api::message::{EventBase, ToolResultData};
use fuyao_provider::{StreamUsage, ToolCallData};

/// 流式阶段（用于中断时判断场景）
pub(crate) enum StreamPhase {
    /// 正在接收 LLM 流式事件
    Streaming,
    /// 重试/退避/上下文溢出等待中
    Backoff,
}

/// 流式累积器（共享状态）
///
/// stream_session 持续更新，TurnExecutor 在中断时读取部分结果。
pub(crate) struct StreamAccumulator {
    /// 累积的文本内容
    pub text: String,
    /// 累积的推理内容
    pub reasoning: String,
    /// 累积的工具调用（从 decoder 同步）
    pub tool_calls: Vec<ToolCallData>,
    /// 累积的 token 用量
    pub usage: StreamUsage,
    /// 当前流式阶段
    pub phase: StreamPhase,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            usage: StreamUsage::default(),
            phase: StreamPhase::Streaming,
        }
    }

    /// 重置所有累积状态（每个 ReAct 循环迭代开始时调用）
    pub fn clear(&mut self) {
        self.text.clear();
        self.reasoning.clear();
        self.tool_calls.clear();
        self.usage = StreamUsage::default();
        self.phase = StreamPhase::Streaming;
    }
}

/// 创建中断专用 ToolResult（content 格式：[中断来源][中断原因]）
pub(crate) fn make_interrupt_tool_result(
    tool_call_id: String,
    tool_name: String,
    source: InterruptSource,
    reason: String,
) -> ToolResultData {
    ToolResultData {
        base: EventBase::default(),
        tool_call_id,
        tool_name,
        content: format!("[{:?}][{}]", source, reason),
    }
}
