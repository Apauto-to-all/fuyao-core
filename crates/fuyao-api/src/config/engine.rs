//! 引擎运行时通道拓扑配置（`[engine]` 段）
//!
//! 集中管理引擎三条有界通道的容量：
//! - **统一入站通道**（session 级）：User / Control / 插件注入条目共用一条通道，
//!   发送顺序即排队顺序，保证总序
//! - **中断通道**（session 级）：与入站队列正交的中断信号
//! - **fan-out 汇聚通道**（进程级）：全部 session 的事件汇聚到单一出口给上层 UI
//!
//! per-session 出站通道刻意无界、不设容量字段：事件入通道前已落库，
//! 无界保证 emit 不因上层消费慢而反压 ReAct turn 推进——引擎唯一的背压点
//! 统一放在 fan-out 汇聚通道。

use serde::Deserialize;

/// 引擎通道容量配置（`[engine]` 段）
///
/// 三个字段与运行时实际创建通道一一对应：session 创建时读
/// `inbound` / `interrupt` 建两条 session 级通道，应用装配时读
/// `fan_out` 建进程级汇聚通道。修改容量只影响之后创建的通道。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// session 级统一入站通道容量（`QueueEntry` 载荷，User / Control / 插件注入共用）
    pub inbound_channel_capacity: usize,
    /// session 级中断通道容量（中断信号量小且瞬时，小容量即足）
    pub interrupt_channel_capacity: usize,
    /// 进程级 fan-out 汇聚通道容量：全部 session 事件汇聚单出口的缓冲上限，
    /// 是引擎唯一的背压点（消费停滞时上游 send 阻塞）
    pub fan_out_capacity: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            inbound_channel_capacity: 32,
            interrupt_channel_capacity: 8,
            fan_out_capacity: 512,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_config_defaults() {
        let c = EngineConfig::default();
        assert_eq!(c.inbound_channel_capacity, 32);
        assert_eq!(c.interrupt_channel_capacity, 8);
        assert_eq!(c.fan_out_capacity, 512);
    }

    #[test]
    fn deserialize_engine_partial() {
        let toml_str = r#"
[engine]
fan_out_capacity = 1024
"#;
        #[derive(Deserialize)]
        struct Wrap {
            engine: EngineConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.engine.fan_out_capacity, 1024);
        // 缺省字段走 Default
        assert_eq!(w.engine.inbound_channel_capacity, 32);
        assert_eq!(w.engine.interrupt_channel_capacity, 8);
    }
}
