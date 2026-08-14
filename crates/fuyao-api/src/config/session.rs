//! 会话配置（上下文压缩 + SQLite 存储）
//!
//! 迁移自 `fuyao-session/src/compressor`（压缩阈值/窗口/fallback）与
//! `fuyao-session/src/store.rs`（busy_timeout / 连接数）的硬编码。

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
///
/// 保留窗口按模型上下文比例动态计算（替代固定 `keep_tokens`）：
/// ```text
/// 实际保留 = min(context_length × keep_ratio, keep_tokens_max)
/// ```
/// - 小上下文模型（如 32K）按比例保留较少，避免撑爆
/// - 大上下文模型（如 200K+）受 `keep_tokens_max` 上限保护，避免保留过多
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CompressionConfig {
    /// 是否启用上下文压缩
    pub enabled: bool,
    /// 触发阈值（0.0~1.0），prompt_tokens / (context_length - summary_max_tokens) 超过此值时触发
    pub threshold: f64,
    /// 保留窗口的相对比例（0.0~1.0），实际保留 = min(context_length × keep_ratio, keep_tokens_max)
    pub keep_ratio: f64,
    /// 保留窗口的 token 上限（tail 段，防止超长上下文模型保留过多）
    pub keep_tokens_max: usize,
    /// 摘要 LLM 输出上限（token）
    pub summary_max_tokens: usize,
    /// 压缩 token 估算：每张图固定占用 token（不按 base64 字符数，避免撑爆触发误压缩），原 `compressor/window.rs TOKENS_PER_IMAGE = 1000`
    pub tokens_per_image: usize,
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
            keep_ratio: 0.05,
            keep_tokens_max: 8000,
            summary_max_tokens: 4096,
            tokens_per_image: 1000,
            skip_child: true,
        }
    }
}

impl CompressionConfig {
    /// 按模型上下文比例计算实际保留 token 数
    ///
    /// 公式：`min(context_length × keep_ratio, keep_tokens_max)`
    ///
    /// - 小上下文模型（如 32K × 0.05 = 1600）：保留较少
    /// - 大上下文模型（如 200K × 0.05 = 10000）：被 `keep_tokens_max`（默认 8000）截断
    ///
    /// 当 `context_length` 或 `keep_ratio` 为 0 时返回 0；下游 `select_recent` 内部
    /// 保证至少保留最后一条消息，不会因此丢失活跃任务。
    pub fn effective_keep_tokens(&self, context_length: u32) -> usize {
        let ratio_amount = (context_length as f64 * self.keep_ratio) as usize;
        ratio_amount.min(self.keep_tokens_max)
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
    /// 是否跳过子 session（有 parent_session_id）的标题生成
    ///
    /// 子任务 session 用 parent_session_id 表达归属，重命名反而扰乱父/子分组
    /// 与前端过滤。默认 true：子 session 不自动生成标题。
    pub skip_child: bool,
    /// 输入截断长度（字符数），用户消息与 AI 回答各截取前 N 字符喂给 LLM
    pub snippet_max_chars: usize,
    /// 标题最大长度（字符数），超长截断并加省略号
    pub max_len: usize,
}

impl Default for TitleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            skip_child: true,
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
        assert!((c.keep_ratio - 0.05).abs() < f64::EPSILON);
        assert_eq!(c.keep_tokens_max, 8000);
        assert_eq!(c.summary_max_tokens, 4096);
        assert_eq!(c.tokens_per_image, 1000);
        assert!(c.skip_child);
    }

    #[test]
    fn effective_keep_tokens_uses_ratio_for_small_context() {
        // 小上下文：32K × 0.05 = 1600，未被 max 截断
        let cfg = CompressionConfig::default();
        assert_eq!(cfg.effective_keep_tokens(32_000), 1600);
    }

    #[test]
    fn effective_keep_tokens_capped_by_max_for_large_context() {
        // 大上下文：200K × 0.05 = 10000，被 max=8000 截断
        let cfg = CompressionConfig::default();
        assert_eq!(cfg.effective_keep_tokens(200_000), 8000);
    }

    #[test]
    fn effective_keep_tokens_returns_zero_for_zero_context() {
        // context_length=0 → 返回 0；select_recent 内部保证至少保留最后一条
        let cfg = CompressionConfig::default();
        assert_eq!(cfg.effective_keep_tokens(0), 0);
    }

    #[test]
    fn effective_keep_tokens_returns_zero_for_zero_ratio() {
        // 用户极端配置：keep_ratio=0 → 永远返回 0
        let cfg = CompressionConfig {
            keep_ratio: 0.0,
            ..CompressionConfig::default()
        };
        assert_eq!(cfg.effective_keep_tokens(128_000), 0);
    }

    #[test]
    fn effective_keep_tokens_uses_custom_ratio_and_max() {
        // 用户调高比例到 0.1，max 调到 12000
        let cfg = CompressionConfig {
            keep_ratio: 0.1,
            keep_tokens_max: 12_000,
            ..CompressionConfig::default()
        };
        // 64K × 0.1 = 6400（未被 max 截断）
        assert_eq!(cfg.effective_keep_tokens(64_000), 6400);
        // 200K × 0.1 = 20000，被 max=12000 截断
        assert_eq!(cfg.effective_keep_tokens(200_000), 12_000);
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
        assert!(c.skip_child);
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
tokens_per_image = 2000
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
        assert!((w.session.compression.keep_ratio - 0.05).abs() < f64::EPSILON);
        assert_eq!(w.session.compression.keep_tokens_max, 8000);
        assert_eq!(w.session.compression.tokens_per_image, 2000);
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
