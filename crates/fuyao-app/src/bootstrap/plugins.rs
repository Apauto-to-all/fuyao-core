//! 插件装配：内置插件按 `[plugins.enabled]` 过滤后构造 [`PluginHost`]
//!
//! - [`build_plugin_host`]：读全局配置装配内置插件集，在 `Engine::new` 之前调用，
//!   插件工厂一次性注入
//! - 显式 `false` 的插件跳过注册，未列出或显式 `true` 的正常注册
//! - 配置引用未知插件名（拼错 / 已下架）记 WARN 后忽略，不阻断装配

use fuyao_api::PluginsConfig;
use fuyao_api::get_config;
use fuyao_core::{Plugin, PluginHost};

/// 内置插件工厂全集（新增内置插件在此追加）
fn builtin_plugin_factories() -> Vec<Box<dyn Plugin>> {
    vec![Box::new(fuyao_guard::LoopGuardPlugin::new())]
}

/// 装配内置插件：按 `[plugins.enabled]` 过滤后构造插件宿主
pub fn build_plugin_host() -> PluginHost {
    let config = get_config();
    let plugins = filter_builtin_plugins(&config.plugins);

    // 未知插件名对账：配置 key 不在已注册名集合内即 WARN，拼错的开关不静默失效
    let registered: Vec<String> = plugins.iter().map(|p| p.name().to_string()).collect();
    for name in config.plugins.enabled.keys() {
        if !registered.contains(name) {
            tracing::warn!(
                plugin = %name,
                layer = "global",
                "插件配置引用了未知的插件名，已忽略"
            );
        }
    }

    let mut host = PluginHost::new();
    for plugin in plugins {
        host.add(plugin);
    }
    host
}

/// 过滤后的内置插件工厂清单（纯函数，不读全局配置，供装配与测试共用）
///
/// 被 `[plugins.enabled]` 显式禁用的插件被剔除，保持原有注册顺序。
fn filter_builtin_plugins(plugins_cfg: &PluginsConfig) -> Vec<Box<dyn Plugin>> {
    builtin_plugin_factories()
        .into_iter()
        .filter(|plugin| {
            let disabled = plugins_cfg.is_plugin_disabled(plugin.name());
            if disabled {
                tracing::info!(
                    plugin = plugin.name(),
                    "插件被 [plugins.enabled] 禁用，跳过"
                );
            }
            !disabled
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认配置：全部内置插件注册
    #[test]
    fn filter_keeps_all_builtin_plugins_by_default() {
        let plugins = filter_builtin_plugins(&PluginsConfig::default());
        let names: Vec<&str> = plugins.iter().map(|p| p.name()).collect();
        assert!(names.contains(&"loop_guard"));
    }

    /// 显式 false：对应插件被剔除
    #[test]
    fn filter_drops_explicitly_disabled_plugin() {
        let mut cfg = PluginsConfig::default();
        cfg.enabled.insert("loop_guard".to_string(), false);
        let plugins = filter_builtin_plugins(&cfg);
        let names: Vec<&str> = plugins.iter().map(|p| p.name()).collect();
        assert!(!names.contains(&"loop_guard"));
    }

    /// 显式 true：等价默认启用
    #[test]
    fn filter_keeps_explicitly_enabled_plugin() {
        let mut cfg = PluginsConfig::default();
        cfg.enabled.insert("loop_guard".to_string(), true);
        let plugins = filter_builtin_plugins(&cfg);
        let names: Vec<&str> = plugins.iter().map(|p| p.name()).collect();
        assert!(names.contains(&"loop_guard"));
    }
}
