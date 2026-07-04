//! 插件系统相关类型

/// 拦截点
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum InterceptPoint {
    /// 用户输入处理前
    Input,
    /// LLM 调用前（session 插件返回消息列表）
    BeforeLlm,
    /// 输出消息发送前
    Output,
}

/// 观察点
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ObservePoint {
    /// 用户输入处理后（持久化）
    Input,
    /// 输出消息发送后（持久化）
    Output,
}

/// 插件清单
#[derive(Debug, Clone)]
pub struct PluginManifest {
    /// 插件名称
    pub name: String,
    /// 插件描述
    pub description: String,
    /// 版本
    pub version: Option<String>,
    /// 作者
    pub author: Option<String>,
    /// 插件配置
    pub config: serde_json::Value,
}

impl Default for PluginManifest {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            version: None,
            author: None,
            config: serde_json::Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intercept_point_equality() {
        assert_eq!(InterceptPoint::Input, InterceptPoint::Input);
        assert_ne!(InterceptPoint::Input, InterceptPoint::BeforeLlm);
    }

    #[test]
    fn observe_point_hashable() {
        let mut set = std::collections::HashSet::new();
        set.insert(ObservePoint::Input);
        set.insert(ObservePoint::Output);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn plugin_manifest_default() {
        let m = PluginManifest::default();
        assert!(m.name.is_empty());
        assert!(m.version.is_none());
    }
}
