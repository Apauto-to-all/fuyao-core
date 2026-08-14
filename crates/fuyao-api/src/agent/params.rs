//! 引擎交互参数三件套
//!
//! 按归属层把引擎交互参数拆解成三个 Params：
//! - [`EngineParams`]：引擎级，启动时定死
//! - [`SessionParams`]：对话级，创建对话时提供，运行时可通过引擎接口更新（见下）
//! - [`ModelConfig`]：模型运行配置，归属 [`SessionParams`]，整 session 共享一份
//!
//! 三者都是拓展容器：目前装的字段后续可按需增加，不改函数签名、不动调用方。
//!
//! 关于 [`SessionParams`] 的更新：整 session 全程只有一份，存在 `SessionCtx`（共享可变）。
//! 要用就读最新，要改就调 [`Engine::update_session_params`](../../../fuyao_core/struct.Engine.html)
//! 写最新——没有"生效时机"概念，消费点（跑 turn、压缩）每次现读现用。
//! 注意 `agent_config` 一旦定死就不应改（改了会重建 system_prompt 冲掉前缀缓存），
//! 目前该字段更新未实现（见 `update_session_params` 的 TODO）。

use crate::agent::AgentPaths;
use crate::provider::ThinkingType;

/// 模型运行配置（session 级共享一份）
///
/// 聚合「用哪个模型」+「怎么思考」三个运行时模型参数。
/// model_id 必填（构造时必须显式提供非空值，空值引擎拒绝对话），故本结构不实现
/// `Default`——杜绝 `ModelConfig::default()` 产生空 model_id 的漏洞，强制所有构造点显式提供；
/// thinking 两字段 `None` 时请求体不发对应字段，走模型自身默认行为。
///
/// 归属：装入 [`SessionParams`]，整 session 共享一份——消费点（跑 turn、压缩）
/// 每次现读 `SessionParams` 取值，更新即通过引擎接口写回去，不存在"生效时机"问题。
#[derive(Debug, Clone)]
pub struct ModelConfig {
    /// 模型 ID（格式：provider_id/model_id），必填：空串时引擎拒绝对话
    pub model_id: String,

    /// 思考开关（对应 thinking.type 字段）。None 时不发，走模型默认
    pub thinking_type: Option<ThinkingType>,

    /// 思考强度档位名（用户自定义字符串，透传给服务器）。None 时不发，走模型默认
    pub reasoning_effort: Option<String>,
}

/// 内置默认主 Agent 的定义名（出厂人格，对应编译期嵌入的 `defaults/primary/default.md`）
///
/// 「default」是框架保证恒存在的唯一定义名：无任何用户文件时它仍可用（内置表最低
/// 优先级注入），用户 `agents/default.md` 可覆盖其内容但不能让这个名字失效。调用方
/// 需要出厂默认人格时显式使用本常量，禁止散写魔法字符串。
pub const DEFAULT_DEFINITION_NAME: &str = "default";

/// Agent 运行配置（对话级参数的内层结构）
///
/// 承载 definition 及未来所有「不碰路径」的 agent 配置。
/// 与 [`AgentPaths`] 正交：AgentPaths 管「数据在哪」，AgentConfig 管「agent 怎么配」。
///
/// 归属：装入 [`SessionParams`]，创建对话时定死且不可变——改了会冲掉前缀缓存，
/// 代价大。这是前缀缓存红线定的：Agent 配置动不得。
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 定义提示词名（加载 `agents/{definition}.md`）。
    /// 必填：未知名（四层目录与内置表均未命中）引擎直接报错，不静默替换人格；
    /// 要出厂默认人格显式传 [`DEFAULT_DEFINITION_NAME`]。
    pub definition: String,
}

/// 引擎启动参数（引擎级，启动时定死）
///
/// 引擎启动时提供，目前装 [`AgentPaths`]（agent 三层目录的身份证明）。
/// 后续要加新字段直接往里塞，不改函数签名、不动调用方。
///
/// 启动引擎的入参：引擎用 agent_paths 找到并打开数据库，把数据库访问能力作为
/// 引擎级共享，所有 Session 共享同一个库。
#[derive(Debug, Clone)]
pub struct EngineParams {
    /// Agent 三层目录的身份证明，默认使用全局层
    pub agent_paths: AgentPaths,
}

/// 创建对话时的参数（对话级，整 session 共享一份，可更新）
///
/// 创建对话时提供，装 [`AgentConfig`]（人格/系统提示词）+ [`ModelConfig`]（模型运行配置）。
/// 后续要加新字段直接往里塞，不改函数签名、不动调用方。
///
/// **整 session 全程只有一份**，存在 `SessionCtx`（共享可变）。消费点（跑 turn、压缩）
/// 每次现读现用；要改就调引擎的 `update_session_params` 写回——没有"生效时机"概念。
///
/// 边界（前缀缓存红线）：
/// - `agent_config`：创建时定死不应改——改了要重建 system_prompt，冲掉前缀缓存。
/// - `model_config`：可随时更新（切模型不破坏前缀缓存语义，下一轮自然用新模型）。
///
/// 不实现 `Default`：`ModelConfig` 已不实现 `Default`（model_id 必填），
/// 故本结构连带不实现 `Default`，强制构造点显式提供 model_config。
#[derive(Debug, Clone)]
pub struct SessionParams {
    /// Agent 运行配置（definition 选择 + 未来扩展，创建时定死不应改）
    pub agent_config: AgentConfig,
    /// 模型运行配置（整 session 共享一份，可随时更新）
    pub model_config: ModelConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_holds_definition_name() {
        let config = AgentConfig {
            definition: "reviewer".to_string(),
        };
        assert_eq!(config.definition, "reviewer");
    }

    #[test]
    fn default_definition_name_is_default() {
        assert_eq!(DEFAULT_DEFINITION_NAME, "default");
    }

    #[test]
    fn session_params_model_id_set() {
        let params = SessionParams {
            agent_config: AgentConfig {
                definition: DEFAULT_DEFINITION_NAME.to_string(),
            },
            model_config: ModelConfig {
                model_id: "deepseek/deepseek-v4-flash".to_string(),
                thinking_type: None,
                reasoning_effort: None,
            },
        };
        assert_eq!(params.model_config.model_id, "deepseek/deepseek-v4-flash");
    }

    #[test]
    fn session_params_definition_set() {
        let params = SessionParams {
            agent_config: AgentConfig {
                definition: "coder".to_string(),
            },
            model_config: ModelConfig {
                model_id: "test/model".to_string(),
                thinking_type: None,
                reasoning_effort: None,
            },
        };
        assert_eq!(params.agent_config.definition, "coder");
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
