//! 选择支持的公共类型
//!
//! 集中定义「列举可选项」的数据结构，供 fuyao-prompt（agent_id / Agent 定义列举）
//! 与 fuyao-app（供应商列举）共用，避免类型分散在各 crate。统一原则：
//! - `id` 为纯身份（不带来源/供应商前缀），前缀语义由独立字段承载；
//! - 复用既有领域类型（[`crate::AgentDefinition`] / [`crate::Model`]），不重复平铺其字段；
//! - 供应商定义只存在于全局层 `fuyao.toml`（单一事实源），管理列表
//!   （[`ProviderOption`] / [`ProviderModelOption`]）直接反映落盘内容。

use crate::{AgentDefinition, ApiProtocol, Model};

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

/// 可选供应商（管理列表项，含旗下模型）
///
/// 供应商粒度的管理视图：`id` 为 `[providers.<id>]` 键；`models` 为其旗下模型的
/// 同构列表。不携带 API Key 明文（敏感信息红线）——`api_key_env_vars` 只给指针名。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderOption {
    /// 供应商 id（`[providers.<id>]` 键，身份锚点，创建后不可变）
    pub id: String,
    /// 供应商显示名
    pub name: String,
    /// API 协议（wire 方言，与管理载荷 / 落盘配置同枚举）
    pub api_protocol: ApiProtocol,
    /// 自定义 base URL（None = 未配置，走供应商默认）
    pub base_url: Option<String>,
    /// API Key 环境变量指针名列表（只给变量名，不给明文）
    pub api_key_env_vars: Vec<String>,
    /// 旗下模型列表（组内按模型 id 字母序）
    pub models: Vec<ProviderModelOption>,
}

/// 可选模型（管理列表项）
///
/// 模型粒度的管理视图：`id` 为纯模型名（`[providers.<id>.models.<mid>]` 键），
/// 调用方按需拼成 `provider_id/id` 设给 model_id。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderModelOption {
    /// 纯模型名（`[providers.<id>.models.<mid>]` 键，身份锚点，创建后不可变）
    pub id: String,
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

    /// 管理列表载荷（ProviderOption / ProviderModelOption）整链可序列化，
    /// 且不出现 api_key 明文字段。
    #[test]
    fn provider_option_serializes_without_secret() {
        let option = ProviderOption {
            id: "deepseek".to_string(),
            name: "深度求索".to_string(),
            api_protocol: ApiProtocol::OpenaiCompletions,
            base_url: Some("https://api.deepseek.com".to_string()),
            api_key_env_vars: vec!["DEEPSEEK_API_KEY".to_string()],
            models: vec![ProviderModelOption {
                id: "deepseek-v4-flash".to_string(),
                model: crate::Model {
                    name: "deepseek-v4-flash".to_string(),
                    cost: crate::ModelCost::default(),
                    limit: crate::ModelLimit::default(),
                    reasoning_efforts: vec![],
                    modalities: crate::ModelModalities::default(),
                },
            }],
        };
        let json = serde_json::to_string(&option).unwrap();
        assert!(json.contains("\"id\":\"deepseek\""), "id 应进 JSON：{json}");
        assert!(
            json.contains("\"api_protocol\":\"openai-completions\""),
            "API 协议应进 JSON（kebab-case）：{json}"
        );
        assert!(
            json.contains("\"api_key_env_vars\":[\"DEEPSEEK_API_KEY\"]"),
            "指针名应进 JSON：{json}"
        );
        assert!(
            !json.contains("\"api_key\""),
            "不得出现 api_key 明文字段：{json}"
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

        // ProviderModelOption：内嵌 Model（连带 ModelCost / ModelLimit / PriceTier），
        // 验证整链可序列化
        let model = ProviderModelOption {
            id: "deepseek-v4-flash".to_string(),
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
            json.contains("\"name\":\"DeepSeek V4 Flash\""),
            "Model.name 应进 JSON：{json}"
        );
    }
}
