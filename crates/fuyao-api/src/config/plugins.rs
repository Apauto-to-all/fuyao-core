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

impl PluginsConfig {
    /// 插件是否被禁用
    ///
    /// - 显式 `false`：返回 `true`（被禁用）
    /// - 未列出或显式 `true`：返回 `false`（默认启用）
    pub fn is_plugin_disabled(&self, name: &str) -> bool {
        self.enabled.get(name) == Some(&false)
    }
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

    /// is_plugin_disabled 三种情况：显式 false / 显式 true / 未列出
    #[test]
    fn is_plugin_disabled_semantics() {
        let mut c = PluginsConfig::default();
        c.enabled.insert("loop_guard".to_string(), false);
        c.enabled.insert("other".to_string(), true);

        // 显式 false → 被禁用
        assert!(c.is_plugin_disabled("loop_guard"));
        // 显式 true → 启用
        assert!(!c.is_plugin_disabled("other"));
        // 未列出 → 默认启用
        assert!(!c.is_plugin_disabled("unlisted"));
    }
}
