//! Guard 配置（循环检测）
//!
//! `LoopGuardConfig` 是循环检测参数的单一事实源，`fuyao-guard` 引用此定义。

use serde::Deserialize;

/// 循环检测配置
///
/// 文本侧为两级判定：警告线（`text_warn_threshold`）与中断线（`text_interrupt_threshold`），
/// 转写 / 改写类局部重叠至多警告，近乎逐字重合持续多个检查点才中断。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoopGuardConfig {
    /// 工具重复检测阈值：连续 N 次相同操作触发警告
    pub tool_repeat_threshold: usize,
    /// 工具交替循环检测窗口：用于检测 A->B->A->B 模式的序列长度
    pub tool_alternate_threshold: usize,
    /// 文本重复警告线（0.0~1.0）：末尾两窗口相似度超过此值计入连续命中并发出一次警告，
    /// 回落至此值以下清零命中计数
    pub text_warn_threshold: f64,
    /// 文本重复中断线（0.0~1.0）：命中得分达到此线（相邻窗口近乎逐字重合的复读特征）
    /// 且连续命中数达标才中断输出；警告线调到此线以上时，中断判定实际跟随警告线
    pub text_interrupt_threshold: f64,
    /// 中断要求的连续命中检查点数：单个检查点的瞬时尖峰不足以判定循环
    pub text_interrupt_hits: usize,
    /// 流式文本检查间隔：累积文本每增长 N 字节执行一次重复检测
    pub streaming_check_interval: usize,
    /// 文本检测滑动窗口比例：以累积文本末尾 N% 字符为一个窗口，比对最后两个窗口的自相似度
    pub streaming_window_ratio: f64,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            tool_repeat_threshold: 4,
            tool_alternate_threshold: 6,
            text_warn_threshold: 0.6,
            text_interrupt_threshold: 0.85,
            text_interrupt_hits: 3,
            streaming_check_interval: 100,
            streaming_window_ratio: 0.2,
        }
    }
}

/// Guard 聚合配置（为未来扩展留形，当前仅 loop 子段）
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct GuardConfig {
    /// 循环检测子段，对应 TOML `[guard.loop]`
    ///
    /// Rust 字段名 `loop_`（避开关键字），经 `rename` 映射 TOML 键 `loop`。
    #[serde(rename = "loop")]
    pub loop_: LoopGuardConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_guard_config_defaults() {
        let c = LoopGuardConfig::default();
        assert_eq!(c.tool_repeat_threshold, 4);
        assert_eq!(c.tool_alternate_threshold, 6);
        assert!((c.text_warn_threshold - 0.6).abs() < f64::EPSILON);
        assert!((c.text_interrupt_threshold - 0.85).abs() < f64::EPSILON);
        assert_eq!(c.text_interrupt_hits, 3);
        assert_eq!(c.streaming_check_interval, 100);
        assert!((c.streaming_window_ratio - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn deserialize_loop_guard_partial() {
        let toml_str = r#"
[guard.loop]
tool_repeat_threshold = 7
text_warn_threshold = 0.5
text_interrupt_threshold = 0.9
text_interrupt_hits = 5
"#;
        #[derive(Deserialize)]
        struct Wrap {
            guard: GuardConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.guard.loop_.tool_repeat_threshold, 7);
        // 缺省字段走 default
        assert_eq!(w.guard.loop_.tool_alternate_threshold, 6);
        assert!((w.guard.loop_.text_warn_threshold - 0.5).abs() < f64::EPSILON);
        assert!((w.guard.loop_.text_interrupt_threshold - 0.9).abs() < f64::EPSILON);
        assert_eq!(w.guard.loop_.text_interrupt_hits, 5);
        assert_eq!(w.guard.loop_.streaming_check_interval, 100);
    }
}
