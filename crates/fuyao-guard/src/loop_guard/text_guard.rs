//! 文本循环检测器
//!
//! 检测流式文本内容的自相似度（重复），根据严重程度升级处理。

use fuyao_api::LoopGuardConfig;
use crate::loop_guard::detectors::detect_text_self_similarity;
use crate::loop_guard::types::LoopSeverity;

/// 文本循环检测结果
#[derive(Debug)]
pub(crate) struct TextDetectResult {
    /// 检测到的严重程度
    pub severity: LoopSeverity,
    /// 检测描述消息
    pub message: String,
}

/// 当前文本阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurrentPhase {
    Content,
    Reasoning,
}

/// 文本循环检测状态机
///
/// 累积流式文本内容，按间隔执行自相似度检测。
/// 每次检测返回 `TextDetectResult`，由协调层决定后续动作。
pub(crate) struct TextLoopGuard {
    /// 循环检测配置
    config: LoopGuardConfig,
    /// 当前轮次累积的流式文本内容
    pub(crate) accumulated_text: String,
    /// 文本重复检测的升级计数器
    text_escalation: usize,
    /// 上一次检查时的文本长度
    last_check_len: usize,
    /// 当前阶段：content 或 reasoning
    current_phase: Option<CurrentPhase>,
}

impl TextLoopGuard {
    pub fn new(config: LoopGuardConfig) -> Self {
        Self {
            config,
            accumulated_text: String::new(),
            text_escalation: 0,
            last_check_len: 0,
            current_phase: None,
        }
    }

    /// 处理流式内容块，返回检测结果（可能为 None）
    pub fn handle_chunk(
        &mut self,
        content: Option<&str>,
        reasoning: Option<&str>,
        interrupt_count: usize,
    ) -> Option<TextDetectResult> {
        let content_str = content.unwrap_or("");
        let reasoning_str = reasoning.unwrap_or("");

        if content_str.is_empty() && reasoning_str.is_empty() {
            return None;
        }

        // 确定当前阶段，阶段切换则重置累积状态
        let current = if !reasoning_str.is_empty() {
            CurrentPhase::Reasoning
        } else {
            CurrentPhase::Content
        };

        if self.current_phase != Some(current) {
            self.accumulated_text.clear();
            self.last_check_len = 0;
            self.text_escalation = 0;
            self.current_phase = Some(current);
        }

        // 累加文本
        match current {
            CurrentPhase::Reasoning => self.accumulated_text.push_str(reasoning_str),
            CurrentPhase::Content => self.accumulated_text.push_str(content_str),
        }

        // 未达到检查间隔，跳过检测
        if self.accumulated_text.len() - self.last_check_len < self.config.streaming_check_interval
        {
            return None;
        }

        // 执行文本自相似度检测
        let result = detect_text_self_similarity(
            &self.accumulated_text,
            self.config.text_repeat_threshold,
            self.config.streaming_window_ratio,
        );

        self.last_check_len = self.accumulated_text.len();

        if let Some(msg) = result {
            let severity = self.escalate_text();
            let should_interrupt = severity == LoopSeverity::Interrupt;
            // 已中断 3 次以上，升级为终止
            let (severity, _) = if should_interrupt && interrupt_count >= 3 {
                (LoopSeverity::Abort, true)
            } else {
                (severity, should_interrupt)
            };

            // Interrupt 级别使用递增后的计数（与 Python 版一致：先递增再生成警告）
            let effective_count = if matches!(severity, LoopSeverity::Interrupt) {
                interrupt_count + 1
            } else {
                interrupt_count
            };

            Some(TextDetectResult {
                severity,
                message: if matches!(severity, LoopSeverity::Interrupt | LoopSeverity::Abort) {
                    self.get_interrupt_warning(effective_count)
                } else {
                    msg
                },
            })
        } else {
            None
        }
    }

    /// 重置检测状态（轮次结束后调用）
    pub fn reset(&mut self) {
        self.accumulated_text.clear();
        self.text_escalation = 0;
        self.last_check_len = 0;
        self.current_phase = None;
    }

    /// 文本重复升级逻辑
    fn escalate_text(&mut self) -> LoopSeverity {
        self.text_escalation += 1;
        if self.text_escalation >= 2 {
            LoopSeverity::Interrupt
        } else {
            LoopSeverity::Warn
        }
    }

    /// 根据中断次数生成升级警告
    fn get_interrupt_warning(&self, interrupt_count: usize) -> String {
        if interrupt_count >= 3 {
            return "[循环检测] AI 在多次干预后仍持续输出重复内容，已彻底终止。请手动调整任务或重新开始。"
                .to_string();
        }

        if interrupt_count == 1 {
            "[循环检测] 你的输出内容在重复。请直接给出结论，不要再展开细节。".to_string()
        } else {
            "[循环检测] 你已多次输出重复内容。请立即停止重复，用一句话总结核心结果。".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(interval: usize) -> LoopGuardConfig {
        LoopGuardConfig {
            streaming_check_interval: interval,
            ..Default::default()
        }
    }

    #[test]
    fn accumulates_text() {
        let mut guard = TextLoopGuard::new(make_config(10));
        guard.handle_chunk(Some("hello world"), None, 0);
        assert_eq!(guard.accumulated_text, "hello world");
    }

    #[test]
    fn phase_switch_resets() {
        let mut guard = TextLoopGuard::new(make_config(10));
        guard.handle_chunk(Some("content"), None, 0);
        assert_eq!(guard.current_phase, Some(CurrentPhase::Content));
        guard.handle_chunk(None, Some("reasoning"), 0);
        assert_eq!(guard.current_phase, Some(CurrentPhase::Reasoning));
        assert_eq!(guard.accumulated_text, "reasoning");
    }

    #[test]
    fn no_detection_below_interval() {
        let mut guard = TextLoopGuard::new(make_config(1000));
        let result = guard.handle_chunk(Some("短文本"), None, 0);
        assert!(result.is_none());
    }

    #[test]
    fn escalation_warn_then_interrupt() {
        let mut guard = TextLoopGuard::new(make_config(10));
        // 构造高重复文本触发检测
        let text = "这是一段重复的内容这是一段重复的内容这是一段重复的内容这是一段重复的内容";
        let r1 = guard.handle_chunk(Some(text), None, 0);
        let r1 = r1.unwrap();
        assert_eq!(r1.severity, LoopSeverity::Warn);

        let r2 = guard.handle_chunk(Some(text), None, 0);
        let r2 = r2.unwrap();
        assert_eq!(r2.severity, LoopSeverity::Interrupt);
    }

    #[test]
    fn escalation_to_abort_after_3_interrupts() {
        let mut guard = TextLoopGuard::new(make_config(10));
        let text = "这是一段重复的内容这是一段重复的内容这是一段重复的内容这是一段重复的内容";
        guard.handle_chunk(Some(text), None, 0);
        let r = guard.handle_chunk(Some(text), None, 3);
        assert_eq!(r.unwrap().severity, LoopSeverity::Abort);
    }

    #[test]
    fn empty_chunk_returns_none() {
        let mut guard = TextLoopGuard::new(make_config(10));
        let result = guard.handle_chunk(Some(""), None, 0);
        assert!(result.is_none());
    }
}
