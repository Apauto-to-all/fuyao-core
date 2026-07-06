//! 引擎通道容量配置
//!
//! 迁移自 `fuyao-core/src/engine/dispatcher.rs:49-51` 的 `mpsc::channel` 容量硬编码。

use serde::Deserialize;

/// 引擎通道容量配置（迁移自 `fuyao-core/src/engine/dispatcher.rs`）
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// 输入通道容量，原 `dispatcher.rs:49` = 64
    pub input_channel_capacity: usize,
    /// 输出通道容量，原 `dispatcher.rs:50` = 256
    pub output_channel_capacity: usize,
    /// 命令通道容量，原 `dispatcher.rs:51` = 64
    pub command_channel_capacity: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            input_channel_capacity: 64,
            output_channel_capacity: 256,
            command_channel_capacity: 64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_config_defaults_match_hardcoded() {
        let c = EngineConfig::default();
        assert_eq!(c.input_channel_capacity, 64);
        assert_eq!(c.output_channel_capacity, 256);
        assert_eq!(c.command_channel_capacity, 64);
    }

    #[test]
    fn deserialize_engine_partial() {
        let toml_str = r#"
[engine]
output_channel_capacity = 512
"#;
        #[derive(Deserialize)]
        struct Wrap {
            engine: EngineConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.engine.output_channel_capacity, 512);
        // 缺省字段
        assert_eq!(w.engine.input_channel_capacity, 64);
        assert_eq!(w.engine.command_channel_capacity, 64);
    }
}
