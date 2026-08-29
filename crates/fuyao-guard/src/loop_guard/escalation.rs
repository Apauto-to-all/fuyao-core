//! 循环检测升级策略（纯函数）
//!
//! 把「基础严重程度 × 中断次数 → 最终严重程度 + 计数 + 警告文案」的升级策略集中，
//! 供工具检测与文本检测两条路径共用。策略与状态机解耦后可独立单测，
//! 改阈值/文案只动此处。

use crate::loop_guard::types::LoopSeverity;

/// 升级策略适用的检测通道（决定警告文案前缀）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectKind {
    /// 工具调用循环
    Tool,
    /// 文本内容循环
    Text,
}

/// 升级判定结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscalationOutcome {
    /// 最终严重程度（可能由 Interrupt 升级为 Abort）
    pub severity: LoopSeverity,
    /// 用于生成警告文案的中断计数（Interrupt 时为 interrupt_count + 1）
    pub effective_count: usize,
}

/// 纯策略：基础严重程度 × 中断次数 → 最终结果
///
/// - 若基础为 Interrupt 且已中断 ≥3 次，升级为 Abort
/// - Interrupt 的 effective_count = interrupt_count + 1（先递增再生成警告）
/// - 其余情况 effective_count = interrupt_count
pub fn resolve(base_severity: LoopSeverity, interrupt_count: usize) -> EscalationOutcome {
    let severity = if base_severity == LoopSeverity::Interrupt && interrupt_count >= 3 {
        LoopSeverity::Abort
    } else {
        base_severity
    };
    let effective_count = if matches!(severity, LoopSeverity::Interrupt) {
        interrupt_count + 1
    } else {
        interrupt_count
    };
    EscalationOutcome {
        severity,
        effective_count,
    }
}

/// 是否应中断（Interrupt 或 Abort）
pub fn should_interrupt(severity: LoopSeverity) -> bool {
    matches!(severity, LoopSeverity::Interrupt | LoopSeverity::Abort)
}

/// 按检测通道与中断计数生成升级警告文案
///
/// 分段策略：≥3 终止提示、==1 首次提示、其余多次提示。
pub fn interrupt_warning(kind: DetectKind, effective_count: usize) -> String {
    match effective_count {
        c if c >= 3 => match kind {
            DetectKind::Tool => "[循环检测] AI 在多次干预后仍持续循环执行工具，已彻底终止。请手动调整任务或重新开始。",
            DetectKind::Text => "[循环检测] AI 在多次干预后仍持续输出重复内容，已彻底终止。请手动调整任务或重新开始。",
        },
        1 => match kind {
            DetectKind::Tool => "[循环检测] 你在重复执行相同的工具操作。请检查工具参数，尝试不同的方法完成任务。",
            DetectKind::Text => "[循环检测] 你的输出内容在持续逐字重复。若是在誊写或引用既有材料，请收束本段并立即推进任务；若确在原地打转，请直接给出结论。",
        },
        _ => match kind {
            DetectKind::Tool => "[循环检测] 你已多次重复相同的工具操作。请立即停止当前工具，换用其他工具或方法。",
            DetectKind::Text => "[循环检测] 你已多次输出重复内容。请立即停止重复，用一句话总结核心结果。",
        },
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_keeps_warn_unchanged() {
        let o = resolve(LoopSeverity::Warn, 5);
        assert_eq!(o.severity, LoopSeverity::Warn);
        assert_eq!(o.effective_count, 5);
    }

    #[test]
    fn resolve_keeps_inject_unchanged() {
        let o = resolve(LoopSeverity::Inject, 2);
        assert_eq!(o.severity, LoopSeverity::Inject);
        assert_eq!(o.effective_count, 2);
    }

    #[test]
    fn resolve_interrupt_below_threshold_increments_count() {
        let o = resolve(LoopSeverity::Interrupt, 1);
        assert_eq!(o.severity, LoopSeverity::Interrupt);
        assert_eq!(o.effective_count, 2);
    }

    #[test]
    fn resolve_interrupt_at_threshold_upgrades_to_abort() {
        // interrupt_count >= 3 → Abort，effective_count 不递增
        let o = resolve(LoopSeverity::Interrupt, 3);
        assert_eq!(o.severity, LoopSeverity::Abort);
        assert_eq!(o.effective_count, 3);
    }

    #[test]
    fn resolve_interrupt_above_threshold_upgrades_to_abort() {
        let o = resolve(LoopSeverity::Interrupt, 5);
        assert_eq!(o.severity, LoopSeverity::Abort);
        assert_eq!(o.effective_count, 5);
    }

    #[test]
    fn should_interrupt_only_for_interrupt_or_abort() {
        assert!(!should_interrupt(LoopSeverity::Warn));
        assert!(!should_interrupt(LoopSeverity::Inject));
        assert!(should_interrupt(LoopSeverity::Interrupt));
        assert!(should_interrupt(LoopSeverity::Abort));
    }

    #[test]
    fn warning_abort_text_at_count_ge_3() {
        assert!(interrupt_warning(DetectKind::Tool, 3).contains("彻底终止"));
        assert!(interrupt_warning(DetectKind::Text, 4).contains("彻底终止"));
    }

    #[test]
    fn warning_first_text_at_count_1() {
        assert!(interrupt_warning(DetectKind::Tool, 1).contains("重复执行"));
        assert!(interrupt_warning(DetectKind::Text, 1).contains("推进任务"));
    }

    #[test]
    fn warning_repeated_text_at_count_2() {
        assert!(interrupt_warning(DetectKind::Tool, 2).contains("多次重复"));
        assert!(interrupt_warning(DetectKind::Text, 2).contains("多次输出"));
    }
}
