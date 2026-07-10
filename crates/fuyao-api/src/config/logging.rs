//! 日志配置（级别 / stderr 开关 / 轮转策略）
//!
//! 对应 `fuyao.toml` 的 `[logging]` 段。写法对齐 `SessionStorageConfig`：
//! 小结构体 + `#[serde(default)]` + 手动 `Default`。

use serde::Deserialize;

/// 日志文件轮转策略
///
/// 映射 `tracing-appender::rolling::Rotation`。TOML 用小写：`daily` / `hourly` / `never`。
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogRotation {
    /// 按天滚动（默认）
    #[default]
    Daily,
    /// 按小时滚动
    Hourly,
    /// 不滚动，单文件持续追加
    Never,
}

/// 日志聚合配置
///
/// 对应 TOML `[logging]` 段。所有字段 `#[serde(default)]`，缺失走默认值。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// 日志级别，EnvFilter 指令语法（如 `"info"`、`"debug"`、`"fuyao_provider=warn"`）
    ///
    /// 默认 `"info"`。运行时可用 `RUST_LOG` 环境变量覆盖
    /// （`EnvFilter::try_from_env` 优先于本字段）。
    pub level: String,

    /// 是否同时输出到 stderr（彩色，开发友好；生产环境可关）
    ///
    /// 默认 `true`。文件层始终输出，此项仅控制 stderr 层。
    pub console: bool,

    /// 日志文件轮转策略
    ///
    /// 默认按天滚动（`daily`）。
    pub rotation: LogRotation,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            console: true,
            rotation: LogRotation::Daily,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logging_config_defaults() {
        let c = LoggingConfig::default();
        assert_eq!(c.level, "info");
        assert!(c.console);
        assert_eq!(c.rotation, LogRotation::Daily);
    }

    #[test]
    fn log_rotation_default_is_daily() {
        assert_eq!(LogRotation::default(), LogRotation::Daily);
    }

    #[test]
    fn deserialize_logging_full() {
        let toml_str = r#"
[logging]
level = "debug"
console = false
rotation = "hourly"
"#;
        #[derive(Deserialize)]
        struct Wrap {
            logging: LoggingConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.logging.level, "debug");
        assert!(!w.logging.console);
        assert_eq!(w.logging.rotation, LogRotation::Hourly);
    }

    #[test]
    fn deserialize_logging_partial_uses_defaults() {
        let toml_str = r#"
[logging]
level = "warn"
"#;
        #[derive(Deserialize)]
        struct Wrap {
            logging: LoggingConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.logging.level, "warn");
        // 缺省字段走默认
        assert!(w.logging.console);
        assert_eq!(w.logging.rotation, LogRotation::Daily);
    }

    #[test]
    fn deserialize_logging_rotation_never() {
        let toml_str = r#"
[logging]
rotation = "never"
"#;
        #[derive(Deserialize)]
        struct Wrap {
            logging: LoggingConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.logging.rotation, LogRotation::Never);
        // 其余默认
        assert_eq!(w.logging.level, "info");
        assert!(w.logging.console);
    }

    #[test]
    fn deserialize_logging_invalid_rotation_fails() {
        let toml_str = r#"
[logging]
rotation = "weekly"
"#;
        #[derive(Deserialize)]
        struct Wrap {
            #[allow(dead_code)]
            logging: LoggingConfig,
        }
        assert!(toml::from_str::<Wrap>(toml_str).is_err());
    }
}
