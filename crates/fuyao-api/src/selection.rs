//! 选择支持的公共类型
//!
//! 集中定义「列举可选项」的数据结构，供 fuyao-prompt（agent_id / Agent 定义列举）
//! 与 fuyao-app（model 列举）共用，避免类型分散在各 crate。统一原则：
//! - `id` 为纯身份（不带来源/供应商前缀），来源由独立字段承载；
//! - 复用既有领域类型（[`crate::AgentDefinition`] / [`crate::Model`]），不重复平铺其字段。

use crate::{AgentDefinition, Model};

/// 可选项来源层
///
/// 归并 agent_id 与 Agent 定义两种来源语义：
/// - agent_id 列举只产出 [`Source::Global`] / [`Source::Workspace`]；
/// - Agent 定义列举产出 Workspace / Agent / Global / Extra / Builtin 全集；
/// - model 无来源（fuyao.toml 多层合并，无单一层来源），故 [`ModelOption`] 不带 source。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Source {
    /// 工作目录层
    Workspace,
    /// Agent 目录层（`{agent_root}/`，仅当提供 agent_id）
    Agent,
    /// 全局层（`~/.fuyao/`）
    Global,
    /// 额外目录层（插件根等）
    Extra,
    /// 内置定义（编译期嵌入）
    Builtin,
}

/// 可选 agent_id（数据隔离身份）
///
/// `id` 为纯文件夹名（不带 `global/` / `workspace/` 前缀），来源由 `source` 承载；
/// 调用方按需自行拼成 `global/{id}` / `workspace/{id}` 设给 agent_id。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentIdOption {
    /// 纯文件夹名，如 "coder"
    pub id: String,
    /// 来源层（Global / Workspace）
    pub source: Source,
}

/// 可选 Agent 定义（人格）
///
/// 复用 [`AgentDefinition`]（含 name / description / mode / tools / system_prompt 等），
/// 不重复平铺其字段。`id` 为 file stem（设给 `AgentConfig.definition` 的值），
/// 与 `definition.name`（frontmatter 显示名）职责分开。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DefinitionOption {
    /// file stem，设给 `AgentConfig.definition` 的值
    pub id: String,
    /// 来源层
    pub source: Source,
    /// 完整 Agent 定义（复用领域类型）
    pub definition: AgentDefinition,
}

/// 可选 model
///
/// `id` 为纯模型名（不带 `provider/` 前缀），供应商由 `provider` 独立承载；
/// 调用方按需拼成 `provider/id` 设给 `ModelConfig.model_id`。
/// 无 `source`：model 配置在 fuyao.toml 多层合并，无单一层来源。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelOption {
    /// 纯模型名，如 "deepseek-v4-flash"
    pub id: String,
    /// 供应商 id，如 "deepseek"
    pub provider: String,
    /// 完整模型元信息（复用领域类型）
    pub model: Model,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_serializes_pascal_case() {
        assert_eq!(
            serde_json::to_string(&Source::Workspace).unwrap(),
            "\"Workspace\""
        );
        assert_eq!(serde_json::to_string(&Source::Agent).unwrap(), "\"Agent\"");
        assert_eq!(
            serde_json::to_string(&Source::Global).unwrap(),
            "\"Global\""
        );
        assert_eq!(serde_json::to_string(&Source::Extra).unwrap(), "\"Extra\"");
        assert_eq!(
            serde_json::to_string(&Source::Builtin).unwrap(),
            "\"Builtin\""
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
            source: Source::Workspace,
        };
        let json = serde_json::to_string(&agent_id).unwrap();
        assert!(json.contains("\"id\":\"coder\""), "id 应进 JSON：{json}");
        assert!(
            json.contains("\"source\":\"Workspace\""),
            "source 应进 JSON：{json}"
        );

        // DefinitionOption：内嵌 AgentDefinition，验证整条链可序列化
        let definition = DefinitionOption {
            id: "coder".to_string(),
            source: Source::Global,
            definition: crate::AgentDefinition::new("coder", "编码 agent", "你是编码助手"),
        };
        let json = serde_json::to_string(&definition).unwrap();
        assert!(json.contains("\"id\":\"coder\""), "id 应进 JSON：{json}");
        assert!(
            json.contains("\"source\":\"Global\""),
            "source 应进 JSON：{json}"
        );
        assert!(
            json.contains("\"name\":\"coder\""),
            "AgentDefinition.name 应进 JSON：{json}"
        );
        assert!(
            json.contains("\"system_prompt\":\"你是编码助手\""),
            "AgentDefinition.system_prompt 应进 JSON：{json}"
        );

        // ModelOption：内嵌 Model（连带 ModelCost / ModelLimit / PriceTier），验证整条链可序列化
        let model = ModelOption {
            id: "deepseek-v4-flash".to_string(),
            provider: "deepseek".to_string(),
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
            json.contains("\"provider\":\"deepseek\""),
            "provider 应进 JSON：{json}"
        );
        assert!(
            json.contains("\"name\":\"DeepSeek V4 Flash\""),
            "Model.name 应进 JSON：{json}"
        );
    }
}
