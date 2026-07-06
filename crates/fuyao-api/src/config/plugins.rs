//! Plugins 配置段
//!
//! 插件开关配置。

use std::collections::HashMap;

use serde::Deserialize;

/// Plugins 配置段
///
/// 对应 `[plugins]` 顶层段。所有字段 `#[serde(default)]`，缺失时走 `Default`。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct PluginsConfig {
    /// 插件开关，key 为插件名称（对应 `Plugin::name()`），value 为是否启用。
    /// 未列出的插件默认启用。
    pub enabled: HashMap<String, bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enabled_is_empty() {
        let c = PluginsConfig::default();
        assert!(c.enabled.is_empty());
    }

    #[test]
    fn deserialize_with_enabled() {
        let toml_str = r#"
[enabled]
loop_guard = false
session = true
"#;
        let c: PluginsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(c.enabled.get("loop_guard"), Some(&false));
        assert_eq!(c.enabled.get("session"), Some(&true));
    }

    #[test]
    fn deserialize_empty_uses_default() {
        let toml_str = "";
        let c: PluginsConfig = toml::from_str(toml_str).unwrap();
        assert!(c.enabled.is_empty());
    }
}
