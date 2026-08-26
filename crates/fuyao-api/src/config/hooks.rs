//! Hooks 配置段
//!
//! hook 引擎执行参数（超时等）。

use serde::Deserialize;

/// Hooks 配置段
///
/// 对应 `[hooks]` 顶层段。所有字段 `#[serde(default)]`，缺失时走 `Default`。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HooksConfig {
    /// 单个 hook 执行超时（秒），默认 5；0 表示不超时
    ///
    /// 该超时是 observe 钩子「挂起」（future 永不完成）的唯一保底：observe
    /// 串行 await 在引擎内联路径上，钩子挂起会冻结整个 session 的事件管道
    /// 且无诊断信息。置 0 即放弃该保底，仅当确认所有 observe 钩子都
    /// 不可能挂起时才可接受。
    pub timeout_secs: u64,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self { timeout_secs: 5 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timeout_is_5_secs() {
        let c = HooksConfig::default();
        assert_eq!(c.timeout_secs, 5);
    }

    #[test]
    fn deserialize_full_config() {
        let toml_str = r#"timeout_secs = 10"#;
        let c: HooksConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(c.timeout_secs, 10);
    }

    #[test]
    fn deserialize_empty_uses_default() {
        // 空 TOML 段，所有字段走 default
        let toml_str = "";
        let c: HooksConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(c.timeout_secs, 5);
    }
}
