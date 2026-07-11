//! 会话配置（上下文压缩 + SQLite 存储）
//!
//! 迁移自 `fuyao-session/src/compressor`（压缩阈值/窗口/fallback）与
//! `fuyao-session/src/store.rs`（busy_timeout / 连接数）的硬编码。

use serde::Deserialize;

/// 上下文压缩配置（迁移自 `fuyao-session/src/compressor`）
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CompressionConfig {
    /// 触发压缩的上下文占用阈值（0.0~1.0），原 `tracker.rs:12` = 0.85
    pub threshold: f64,
    /// 最近消息保留窗口大小，原 `compressor/mod.rs:15` MAX_RECENT_WINDOW = 6
    pub recent_window: usize,
    /// 无法解析模型上下文长度时的回退值，原 `tracker.rs:51` = 128000
    pub fallback_context: u32,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            threshold: 0.85,
            recent_window: 6,
            fallback_context: 128_000,
        }
    }
}

/// 会话存储（SQLite）配置（迁移自 `fuyao-session/src/store.rs`）
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SessionStorageConfig {
    /// SQLite busy_timeout（秒），原 `store.rs:119` = 5
    pub busy_timeout_secs: u64,
    /// 连接池最大连接数，原 `store.rs:122` = 5
    pub max_connections: u32,
}

impl Default for SessionStorageConfig {
    fn default() -> Self {
        Self {
            busy_timeout_secs: 5,
            max_connections: 5,
        }
    }
}

/// 会话标题自动生成配置
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TitleConfig {
    /// 是否启用自动生成标题
    pub enabled: bool,
    /// 输入截断长度（字符数），用户消息与 AI 回答各截取前 N 字符喂给 LLM
    pub snippet_max_chars: usize,
    /// 标题最大长度（字符数），超长截断并加省略号
    pub max_len: usize,
}

impl Default for TitleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            snippet_max_chars: 500,
            max_len: 80,
        }
    }
}

/// 会话聚合配置
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SessionConfig {
    /// 上下文压缩子段，对应 TOML `[session.compression]`
    pub compression: CompressionConfig,
    /// 存储子段，对应 TOML `[session.storage]`
    pub storage: SessionStorageConfig,
    /// 标题自动生成子段，对应 TOML `[session.title]`
    pub title: TitleConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression_config_defaults_match_hardcoded() {
        let c = CompressionConfig::default();
        assert!((c.threshold - 0.85).abs() < f64::EPSILON);
        assert_eq!(c.recent_window, 6);
        assert_eq!(c.fallback_context, 128_000);
    }

    #[test]
    fn session_storage_config_defaults_match_hardcoded() {
        let c = SessionStorageConfig::default();
        assert_eq!(c.busy_timeout_secs, 5);
        assert_eq!(c.max_connections, 5);
    }

    #[test]
    fn title_config_defaults() {
        let c = TitleConfig::default();
        assert!(c.enabled);
        assert_eq!(c.snippet_max_chars, 500);
        assert_eq!(c.max_len, 80);
    }

    #[test]
    fn deserialize_session_partial() {
        let toml_str = r#"
[session.storage]
max_connections = 10
[session.compression]
threshold = 0.9
"#;
        #[derive(Deserialize)]
        struct Wrap {
            session: SessionConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.session.storage.max_connections, 10);
        // 缺省字段
        assert_eq!(w.session.storage.busy_timeout_secs, 5);
        assert!((w.session.compression.threshold - 0.9).abs() < f64::EPSILON);
        assert_eq!(w.session.compression.recent_window, 6);
    }
}
