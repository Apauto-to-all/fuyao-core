//! 引擎交互参数三件套
//!
//! 按归属层把引擎交互参数拆解成三个 Params：
//! - [`EngineParams`]：引擎级，启动时定死
//! - [`SessionParams`]：对话级，创建对话时定死且不可变（前缀缓存红线）
//! - [`MessageParams`]：消息级，每条消息自带
//!
//! 三者都是拓展容器：目前装的字段后续可按需增加，不改函数签名、不动调用方。
//!
//! 内层 struct [`ModelConfig`] / [`AgentConfig`] 字段不变，只是外层容器从单一
//! AgentContext 拆成各自归属的 Params。

use crate::agent::AgentPaths;
use crate::provider::ThinkingType;

/// 模型运行配置（消息级参数的内层结构）
///
/// 聚合「用哪个模型」+「怎么思考」三个运行时模型参数。
/// 三字段默认 `None`，请求体不发对应字段，走模型自身默认行为。
///
/// 归属：装入 [`MessageParams`]，跟着每条消息走——同一对话里上一条用 A 模型、
/// 这一条用 B 模型完全自由，因为模型选择是消息的属性，不是对话的属性。
#[derive(Debug, Clone, Default)]
pub struct ModelConfig {
    /// 模型 ID，如 aliyun/qwen3.6-plus。None 时自动使用默认模型
    pub model_id: Option<String>,

    /// 思考开关（对应 thinking.type 字段）。None 时不发，走模型默认
    pub thinking_type: Option<ThinkingType>,

    /// 思考强度档位名（用户自定义字符串，透传给服务器）。None 时不发，走模型默认
    pub reasoning_effort: Option<String>,
}

/// Agent 运行配置（对话级参数的内层结构）
///
/// 承载 definition 及未来所有「不碰路径」的 agent 配置。
/// 与 [`AgentPaths`] 正交：AgentPaths 管「数据在哪」，AgentConfig 管「agent 怎么配」。
///
/// 归属：装入 [`SessionParams`]，创建对话时定死且不可变——改了会冲掉前缀缓存，
/// 代价大。这是前缀缓存红线定的：Agent 配置动不得。
#[derive(Debug, Clone, Default)]
pub struct AgentConfig {
    /// 定义提示词名（加载 `agents/{definition}.md`）。
    /// None 时用 `"default"`（系统默认定义）。
    pub definition: Option<String>,
}

/// 引擎启动参数（引擎级，启动时定死）
///
/// 引擎启动时提供，目前装 [`AgentPaths`]（agent 三层目录的身份证明）。
/// 后续要加新字段直接往里塞，不改函数签名、不动调用方。
///
/// 设计文档「动作一·启动引擎」的入参：引擎用 agent_paths 找到并打开数据库，
/// 把数据库访问能力作为引擎级共享，所有 Session 共享同一个库。
#[derive(Debug, Clone)]
pub struct EngineParams {
    /// Agent 三层目录的身份证明，默认使用全局层
    pub agent_paths: AgentPaths,
}

/// 创建对话时的参数（对话级，创建时定死，不可变）
///
/// 创建对话时提供，目前装 [`AgentConfig`]（系统提示词等）。
/// 后续要加新字段直接往里塞，不改函数签名、不动调用方。
///
/// Agent 配置创建时定死，不可变——改了会冲掉前缀缓存，代价大。
#[derive(Debug, Clone, Default)]
pub struct SessionParams {
    /// Agent 运行配置（definition 选择 + 未来扩展）
    pub agent_config: AgentConfig,
}

/// 发送消息时的配置（消息级，每条消息自带）
///
/// 每发一条消息都带一个 [`MessageParams`]，决定**这一轮**用哪个模型跑、怎么思考。
/// 后续要加新字段直接往里塞，不改函数签名、不动调用方。
///
/// model id 是消息的属性，不是对话的属性——同一对话里每条消息能换模型，互不干扰。
#[derive(Debug, Clone, Default)]
pub struct MessageParams {
    /// 模型运行配置（聚合模型选择 + 思考控制）
    pub model_config: ModelConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_config_default_all_none() {
        let config = ModelConfig::default();
        assert!(config.model_id.is_none());
        assert!(config.thinking_type.is_none());
        assert!(config.reasoning_effort.is_none());
    }

    #[test]
    fn agent_config_default_definition_is_none() {
        let config = AgentConfig::default();
        assert!(config.definition.is_none());
    }

    #[test]
    fn agent_config_definition_set() {
        let config = AgentConfig {
            definition: Some("reviewer".to_string()),
        };
        assert_eq!(config.definition.as_deref(), Some("reviewer"));
    }

    #[test]
    fn message_params_default_uses_default_model_config() {
        let params = MessageParams::default();
        assert!(params.model_config.model_id.is_none());
        assert!(params.model_config.thinking_type.is_none());
        assert!(params.model_config.reasoning_effort.is_none());
    }

    #[test]
    fn message_params_model_id_set() {
        let params = MessageParams {
            model_config: ModelConfig {
                model_id: Some("deepseek/deepseek-v4-flash".to_string()),
                ..Default::default()
            },
        };
        assert_eq!(
            params.model_config.model_id.as_deref(),
            Some("deepseek/deepseek-v4-flash")
        );
    }

    #[test]
    fn session_params_default_uses_default_agent_config() {
        let params = SessionParams::default();
        assert!(params.agent_config.definition.is_none());
    }

    #[test]
    fn session_params_definition_set() {
        let params = SessionParams {
            agent_config: AgentConfig {
                definition: Some("coder".to_string()),
            },
        };
        assert_eq!(params.agent_config.definition.as_deref(), Some("coder"));
    }

    #[test]
    fn engine_params_holds_agent_paths() {
        let params = EngineParams {
            agent_paths: AgentPaths::default(),
        };
        // Default AgentPaths 使用全局层（agent_id 为 None）
        assert!(params.agent_paths.agent_id.is_none());
    }
}
