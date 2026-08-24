//! 会话配置（上下文压缩 + SQLite 存储）
//!
//! 定义上下文压缩的触发阈值与摘要输出上限、SQLite 存储的
//! busy_timeout / 连接数，以及标题自动生成参数。

use serde::Deserialize;

/// 上下文压缩配置（`[session.compression]`）
///
/// 触发公式：
/// ```text
/// prompt_tokens >= threshold × (context_length - summary_max_tokens)
/// ```
/// - `prompt_tokens` 来自上一轮 LLM 返回的真实 usage（pre-turn 触发时只能用上一轮值）
/// - `context_length` 从模型注册表（`model.limit.context`，加载期已校验必为正整数）
///   解析；模型未注册时调用方无从判定阈值，跳过压缩判定
/// - `summary_max_tokens` 作为输出预留扣除
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CompressionConfig {
    /// 是否启用上下文压缩
    pub enabled: bool,
    /// 触发阈值（0.0~1.0），prompt_tokens / (context_length - summary_max_tokens) 超过此值时触发
    pub threshold: f64,
    /// 摘要 LLM 输出上限（token）
    pub summary_max_tokens: usize,
    /// 是否跳过子 session（有 parent_session_id）的上下文压缩
    ///
    /// 子代理以 Fresh 模式派生、一次性运行，只把最终回复文本作为 tool_result 回喂父 Agent，
    /// 自身完整历史留在子 session 内。若子代理中途压缩，早期工具证据会被摘要替代，
    /// 失真的最终回复会传导给父 Agent 的决策；且压缩需额外调一次摘要 LLM，
    /// 对一次性子代理在成本与质量上均不划算。默认 true：子 session 不自动压缩。
    pub skip_child: bool,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: 0.85,
            summary_max_tokens: 4096,
            skip_child: true,
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
        assert!(c.enabled);
        assert!((c.threshold - 0.85).abs() < f64::EPSILON);
        assert_eq!(c.summary_max_tokens, 4096);
        assert!(c.skip_child);
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
    }

    /// compression 配置不包含任何上下文长度回退项：模型上下文长度只来自模型清单的
    /// `limit.context`（加载期必填校验）。toml 里的残留未知键按 serde 现状
    /// （未知字段忽略）静默丢弃，无兼容读取。
    #[test]
    fn deserialize_compression_ignores_unknown_leftover_keys() {
        let toml_str = r#"
[session.compression]
threshold = 0.7
fallback_context = 96000
"#;
        #[derive(Deserialize)]
        struct Wrap {
            session: SessionConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert!((w.session.compression.threshold - 0.7).abs() < f64::EPSILON);
        assert_eq!(w.session.compression.summary_max_tokens, 4096);
    }
}
