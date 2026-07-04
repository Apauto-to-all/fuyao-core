//! 循环检测配置

/// 循环检测配置
#[derive(Debug, Clone)]
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
