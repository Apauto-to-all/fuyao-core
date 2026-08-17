//! 选择支持的公共类型
//!
//! 集中定义「列举可选项」的数据结构，供 fuyao-prompt（agent_id / Agent 定义列举）
//! 与 fuyao-app（model 列举）共用，避免类型分散在各 crate。统一原则：
//! - `id` 为纯身份（不带来源/供应商前缀），前缀语义由独立字段承载；
//! - 复用既有领域类型（[`crate::AgentDefinition`] / [`crate::Model`]），不重复平铺其字段；
//! - Agent 定义与 model 均为按名覆盖 / 多层合并语义——同名互斥、优先级胜出，
//!   加载时按固定优先级链整体重解析，来源不参与身份，故不携带任何来源字段。

use crate::{AgentDefinition, Model};

/// agent_id 来源层
///
/// 仅服务于 agent_id 列举：agent_id 的身份字符串带层前缀（`global/{id}` /
/// `workspace/{id}`），引擎靠前缀定位数据域目录——同名文件夹可在全局层与项目层
/// 并存且都是合法目标，列举时必须区分层才能拼出唯一身份。
/// Agent 定义按文件名做优先级覆盖（放同名文件即覆盖，无「选哪一层」的问题）、
/// model 配置在 fuyao.toml 多层合并——两者均无来源概念，不用本枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum AgentIdSource {
    /// 全局层（`~/.fuyao/fuyao-agents/`）
    Global,
    /// 项目层（`{workspace}/.fuyao/fuyao-agents/`）
    Workspace,
}

/// 可选 agent_id（数据隔离身份）
///
/// `id` 为纯文件夹名（不带 `global/` / `workspace/` 前缀），来源由 `source` 承载；
/// 调用方按需自行拼成 `global/{id}` / `workspace/{id}` 设给 agent_id
/// （来源前缀大小写不敏感，`AgentIdSource` 的序列化值可直接作前缀）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentIdOption {
    /// 纯文件夹名，如 "coder"
    pub id: String,
    /// 来源层（Global / Workspace）
    pub source: AgentIdSource,
}

/// 可选 Agent 定义（人格）
///
/// 复用 [`AgentDefinition`]（含 name / description / mode / tools / system_prompt 等），
/// 不重复平铺其字段。`id` 为 file stem（设给 `AgentConfig.definition` 的值），
/// 与 `definition.name`（frontmatter 显示名）职责分开。
/// 无来源字段：定义按 `agents/{id}.md` 优先级链解析（workspace > agent > global >
/// extra > 内置），同名互斥、高优先级层胜出，存储与列举都只需纯名。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DefinitionOption {
    /// file stem，设给 `AgentConfig.definition` 的值
    pub id: String,
    /// 完整 Agent 定义（复用领域类型）
    pub definition: AgentDefinition,
}

/// 可选 model
///
/// `id` 为纯模型名（不带 `provider/` 前缀），供应商由 `provider_id` 独立承载；
/// 调用方按需拼成 `provider_id/id` 设给 `ModelConfig.model_id`。
/// 无来源字段：model 配置在 fuyao.toml 多层合并，无单一层来源。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelOption {
    /// 纯模型名，如 "deepseek-v4-flash"
    pub id: String,
    /// 供应商 id，如 "deepseek"（身份，用于拼 model_id 路由）
    pub provider_id: String,
    /// 供应商显示名，如 "商汤 SenseNova"（来自 Provider 注册配置，仅展示用途）
    pub provider_name: String,
    /// 完整模型元信息（复用领域类型）
    pub model: Model,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_source_serializes_pascal_case() {
        assert_eq!(
            serde_json::to_string(&AgentIdSource::Global).unwrap(),
            "\"Global\""
        );
        assert_eq!(
            serde_json::to_string(&AgentIdSource::Workspace).unwrap(),
            "\"Workspace\""
        );
    }

    /// 三个 option 类型连同内嵌领域类型（AgentDefinition / Model）整链可序列化。
    ///
    /// 回归守卫：后续给这些类型新增字段时，若误加不可序列化字段，本测试即编译失败，
    /// 及早暴露问题。
    #[test]
    fn option_types_serialize_with_embedded_domain_types() {
        // AgentIdOption：纯 id + source
        let agent_id = AgentIdOption {
            id: "coder".to_string(),
            source: AgentIdSource::Workspace,
        };
        let json = serde_json::to_string(&agent_id).unwrap();
        assert!(json.contains("\"id\":\"coder\""), "id 应进 JSON：{json}");
        assert!(
            json.contains("\"source\":\"Workspace\""),
            "source 应进 JSON：{json}"
        );

        // DefinitionOption：内嵌 AgentDefinition，验证整条链可序列化（且不带来源）
        let definition = DefinitionOption {
            id: "coder".to_string(),
            definition: crate::AgentDefinition::new("coder", "编码 agent", "你是编码助手"),
        };
        let json = serde_json::to_string(&definition).unwrap();
        assert!(json.contains("\"id\":\"coder\""), "id 应进 JSON：{json}");
        assert!(
            !json.contains("\"source\""),
            "定义不携带来源，JSON 不应出现 source 字段：{json}"
        );
        assert!(
            json.contains("\"name\":\"coder\""),
            "AgentDefinition.name 应进 JSON：{json}"
        );
        assert!(
            json.contains("\"system_prompt\":\"你是编码助手\""),
            "AgentDefinition.system_prompt 应进 JSON：{json}"
        );

        // ModelOption：内嵌 Model（连带 ModelCost / ModelLimit / PriceTier），验证整链可序列化
        let model = ModelOption {
            id: "deepseek-v4-flash".to_string(),
            provider_id: "deepseek".to_string(),
            provider_name: "深度求索 DeepSeek".to_string(),
            model: crate::Model {
                name: "DeepSeek V4 Flash".to_string(),
                cost: crate::ModelCost::default(),
                limit: crate::ModelLimit::default(),
                reasoning_efforts: vec![],
                modalities: crate::ModelModalities::default(),
            },
        };
        let json = serde_json::to_string(&model).unwrap();
        assert!(
            json.contains("\"id\":\"deepseek-v4-flash\""),
            "id 应进 JSON：{json}"
        );
        assert!(
            json.contains("\"provider_id\":\"deepseek\""),
            "provider_id 应进 JSON：{json}"
        );
        assert!(
            json.contains("\"provider_name\":\"深度求索 DeepSeek\""),
            "provider_name 应进 JSON：{json}"
        );
        assert!(
            json.contains("\"name\":\"DeepSeek V4 Flash\""),
            "Model.name 应进 JSON：{json}"
        );
    }
}
