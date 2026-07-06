//! 工具循环检测器
//!
//! 检测工具调用的连续重复和循环序列模式，根据严重程度升级处理。

use std::collections::VecDeque;

use crate::loop_guard::detectors::{detect_tool_repetition, detect_tool_sequence_pattern};
use crate::loop_guard::types::{LoopSeverity, ToolCallRecord};
use fuyao_api::LoopGuardConfig;

/// 工具循环检测结果
#[derive(Debug)]
pub(crate) struct ToolDetectResult {
    /// 检测到的严重程度
    pub severity: LoopSeverity,
    /// 检测描述消息
    pub message: String,
}

/// 工具循环检测状态机
///
/// 维护工具调用历史，检测连续重复和循环序列模式。
/// 每次检测返回 `ToolDetectResult`，由协调层决定后续动作。
pub(crate) struct ToolLoopGuard {
    /// 循环检测配置
    config: LoopGuardConfig,
    /// 检测窗口大小（偶数）
    window: usize,
    /// 工具调用历史记录队列
    pub(crate) tool_history: VecDeque<ToolCallRecord>,
    /// 工具循环检测的升级计数器
    tool_escalation: usize,
}

impl ToolLoopGuard {
    pub fn new(config: LoopGuardConfig) -> Self {
        // 限制窗口大小上限，防止过大影响性能
        let threshold = if config.tool_alternate_threshold > 20 {
            20
        } else {
            config.tool_alternate_threshold
        };
        let window = if threshold.is_multiple_of(2) {
            threshold
        } else {
            threshold + 1
        };

        Self {
            config,
            window,
            tool_history: VecDeque::new(),
            tool_escalation: 0,
        }
    }

    /// 处理工具调用事件，返回检测结果
    pub fn handle_tool_call(
        &mut self,
        tool_name: &str,
        canonical_args: &str,
        interrupt_count: usize,
    ) -> Option<ToolDetectResult> {
        let record = ToolCallRecord {
            tool_name: tool_name.to_string(),
            canonical_args: canonical_args.to_string(),
        };

        // 检测连续重复
        let rep_result = detect_tool_repetition(
            &self.tool_history,
            &record,
            self.config.tool_repeat_threshold,
        );
        // 检测循环序列
        let seq_result = detect_tool_sequence_pattern(&self.tool_history, self.window);
        let detected = rep_result.or(seq_result);

        let result = if let Some(msg) = detected {
            let severity = self.escalate_tool();
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

            Some(ToolDetectResult {
                severity,
                message: if matches!(severity, LoopSeverity::Interrupt | LoopSeverity::Abort) {
                    self.get_interrupt_warning(effective_count)
                } else {
                    msg
                },
            })
        } else {
            self.tool_escalation = 0;
            None
        };

        // 加入历史记录
        self.tool_history.push_back(record);
        if self.tool_history.len() > self.window + 10 {
            self.tool_history.pop_front();
        }

        result
    }

    /// 重置检测状态（轮次结束后调用）
    pub fn reset(&mut self) {
        self.tool_history.clear();
        self.tool_escalation = 0;
    }

    /// 工具循环升级逻辑
    fn escalate_tool(&mut self) -> LoopSeverity {
        self.tool_escalation += 1;
        if self.tool_escalation >= 3 {
            LoopSeverity::Interrupt
        } else if self.tool_escalation >= 2 {
            LoopSeverity::Inject
        } else {
            LoopSeverity::Warn
        }
    }

    /// 根据中断次数生成升级警告
    fn get_interrupt_warning(&self, interrupt_count: usize) -> String {
        if interrupt_count >= 3 {
            return "[循环检测] AI 在多次干预后仍持续循环执行工具，已彻底终止。请手动调整任务或重新开始。"
                .to_string();
        }

        if interrupt_count == 1 {
            "[循环检测] 你在重复执行相同的工具操作。请检查工具参数，尝试不同的方法完成任务。"
                .to_string()
        } else {
            "[循环检测] 你已多次重复相同的工具操作。请立即停止当前工具，换用其他工具或方法。"
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config() -> LoopGuardConfig {
        LoopGuardConfig {
            tool_repeat_threshold: 3,
            ..Default::default()
        }
    }

    #[test]
    fn no_detection_below_threshold() {
        let mut guard = ToolLoopGuard::new(make_config());
        // threshold=3，前 2 次调用 count=1/2，不触发
        for _ in 0..2 {
            let result = guard.handle_tool_call("bash", r#"{"command":"ls"}"#, 0);
            assert!(result.is_none());
        }
    }

    #[test]
    fn detects_repetition_at_threshold() {
        let mut guard = ToolLoopGuard::new(make_config());
        // 前 2 次填充历史，第 3 次 count=3 >= threshold=3
        for _ in 0..2 {
            guard.handle_tool_call("bash", r#"{"command":"ls"}"#, 0);
        }
        let result = guard.handle_tool_call("bash", r#"{"command":"ls"}"#, 0);
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.severity, LoopSeverity::Warn);
    }

    #[test]
    fn escalation_warn_inject_interrupt() {
        let mut guard = ToolLoopGuard::new(LoopGuardConfig {
            tool_repeat_threshold: 2,
            ..Default::default()
        });
        // 第 1 次：count=1 < 2，不触发
        assert!(guard.handle_tool_call("bash", "ls", 0).is_none());
        // 第 2 次：count=2 >= 2，Warn
        let r1 = guard.handle_tool_call("bash", "ls", 0);
        assert_eq!(r1.unwrap().severity, LoopSeverity::Warn);
        // 第 3 次：Inject
        let r2 = guard.handle_tool_call("bash", "ls", 0);
        assert_eq!(r2.unwrap().severity, LoopSeverity::Inject);
        // 第 4 次：Interrupt
        let r3 = guard.handle_tool_call("bash", "ls", 0);
        assert_eq!(r3.unwrap().severity, LoopSeverity::Interrupt);
    }

    #[test]
    fn escalation_to_abort_after_3_interrupts() {
        let mut guard = ToolLoopGuard::new(LoopGuardConfig {
            tool_repeat_threshold: 2,
            ..Default::default()
        });
        guard.handle_tool_call("bash", "ls", 0);
        guard.handle_tool_call("bash", "ls", 0);
        guard.handle_tool_call("bash", "ls", 0);
        // interrupt_count=3 时，Interrupt 升级为 Abort
        let r = guard.handle_tool_call("bash", "ls", 3);
        assert_eq!(r.unwrap().severity, LoopSeverity::Abort);
    }

    #[test]
    fn no_detection_resets_escalation() {
        let mut guard = ToolLoopGuard::new(LoopGuardConfig {
            tool_repeat_threshold: 2,
            ..Default::default()
        });
        guard.handle_tool_call("bash", "ls", 0);
        // 不同调用重置
        let r = guard.handle_tool_call("read", "file.rs", 0);
        assert!(r.is_none());
    }

    #[test]
    fn window_capped_at_20() {
        let guard = ToolLoopGuard::new(LoopGuardConfig {
            tool_alternate_threshold: 30,
            ..Default::default()
        });
        // window 上限 20，tool_history 最大容量 = window + 10 = 30
        // 填充 35 条记录，验证只保留 30 条
        let mut guard = guard;
        for i in 0..35 {
            guard.handle_tool_call("bash", &format!("cmd_{i}"), 0);
        }
        assert_eq!(guard.tool_history.len(), 30);
    }
}
