//! Guard 配置（循环检测）
//!
//! `LoopGuardConfig` 从 `fuyao-guard` 下沉到此，作为单一真相源。
//! `fuyao-guard` 改为引用本处定义。

use serde::Deserialize;

/// 循环检测配置
///
/// 各字段默认值与循环检测插件的硬编码默认值一致（迁移自原硬编码，集中到此可配置化）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoopGuardConfig {
    /// 工具重复检测阈值：连续 N 次相同操作触发警告
    pub tool_repeat_threshold: usize,
    /// 工具交替循环检测窗口：用于检测 A->B->A->B 模式的序列长度
    pub tool_alternate_threshold: usize,
    /// 文本重复率阈值：0.0~1.0，超过此相似度判定为内容重复
    pub text_repeat_threshold: f64,
    /// 流式文本检查间隔：每隔 N 个字符执行一次重复检测，节省性能
    pub streaming_check_interval: usize,
    /// 文本检测滑动窗口比例：取当前累积文本末尾的 N% 进行相似度比对
    pub streaming_window_ratio: f64,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            tool_repeat_threshold: 4,
            tool_alternate_threshold: 6,
            text_repeat_threshold: 0.6,
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
    fn loop_guard_config_defaults_match_hardcoded() {
        let c = LoopGuardConfig::default();
        assert_eq!(c.tool_repeat_threshold, 4);
        assert_eq!(c.tool_alternate_threshold, 6);
        assert!((c.text_repeat_threshold - 0.6).abs() < f64::EPSILON);
        assert_eq!(c.streaming_check_interval, 100);
        assert!((c.streaming_window_ratio - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn deserialize_loop_guard_partial() {
        let toml_str = r#"
[guard.loop]
tool_repeat_threshold = 7
text_repeat_threshold = 0.8
"#;
        #[derive(Deserialize)]
        struct Wrap {
            guard: GuardConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.guard.loop_.tool_repeat_threshold, 7);
        // 缺省字段走 default
        assert_eq!(w.guard.loop_.tool_alternate_threshold, 6);
        assert!((w.guard.loop_.text_repeat_threshold - 0.8).abs() < f64::EPSILON);
        assert_eq!(w.guard.loop_.streaming_check_interval, 100);
    }
}
