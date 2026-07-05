//! 模型数据类型定义
//!
//! 定义 LLM 模型的配置信息，包括价格、限制和模态支持。

/// 价格梯度区间
///
/// 定义不同 token 数量区间的价格。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PriceTier {
    /// 区间上限
    pub max_tokens: u32,

    /// 输入 tokens 价格（价格/M）
    pub input: Option<f64>,

    /// 输出 tokens 价格（价格/M）
    pub output: Option<f64>,

    /// 推理 tokens 价格（价格/M）
    pub reasoning: Option<f64>,

    /// 缓存价格（价格/M）
    pub cache: Option<f64>,
}

/// 模型价格信息
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ModelCost {
    /// 输入 tokens 价格（价格/M）
    pub input: Option<f64>,

    /// 输出 tokens 价格（价格/M）
    pub output: Option<f64>,

    /// 推理 tokens 价格（价格/M）
    pub reasoning: Option<f64>,

    /// 缓存价格（价格/M）
    pub cache: Option<f64>,

    /// 价格梯度区间列表
    pub tiers: Vec<PriceTier>,
}

/// 模型限制信息
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ModelLimit {
    /// 最大上下文窗口（tokens）
    pub context: u32,

    /// 最大输入 tokens
    pub input: Option<u32>,

    /// 最大输出 tokens
    pub output: u32,
}

/// 模型模态支持
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ModelModalities {
    /// 输入模态
    pub input: Vec<String>,

    /// 输出模态
    pub output: Vec<String>,
}

impl Default for ModelModalities {
    fn default() -> Self {
        Self {
            input: vec!["text".to_string()],
            output: vec!["text".to_string()],
        }
    }
}

/// 思考模式开关（对应 OpenAI 兼容协议的 thinking.type 字段）
///
/// 序列化为 snake_case 字符串："enabled" / "disabled"
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingType {
    /// 开启思考
    Enabled,
    /// 关闭思考
    Disabled,
}

/// 模型配置
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Model {
    /// 模型显示名称
    pub name: String,

    /// 价格信息
    pub cost: ModelCost,

    /// 限制信息
    pub limit: ModelLimit,

    /// 支持的思考强度档位列表（用户自定义字符串，透传给服务器；空则只能开关思考）
    ///
    /// 档位名由各供应商自定义（如 "high"/"max"/"big"/"turbo"），
    /// fuyao 不做枚举约束，配置什么就透传什么，服务器自识别。
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,

    /// 模态支持
    pub modalities: ModelModalities,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_cost_default_has_no_prices() {
        let cost = ModelCost::default();
        assert!(cost.input.is_none());
        assert!(cost.output.is_none());
        assert!(cost.reasoning.is_none());
        assert!(cost.cache.is_none());
        assert!(cost.tiers.is_empty());
    }

    #[test]
    fn model_limit_default_has_zero_values() {
        let limit = ModelLimit::default();
        assert_eq!(limit.context, 0);
        assert!(limit.input.is_none());
        assert_eq!(limit.output, 0);
    }

    #[test]
    fn model_modalities_default_has_text_only() {
        let modalities = ModelModalities::default();
        assert_eq!(modalities.input, vec!["text".to_string()]);
        assert_eq!(modalities.output, vec!["text".to_string()]);
    }

    #[test]
    fn model_requires_name_field() {
        let model = Model {
            name: "qwen3.6-plus".to_string(),
            cost: ModelCost::default(),
            limit: ModelLimit::default(),
            reasoning_efforts: vec![],
            modalities: ModelModalities::default(),
        };
        assert_eq!(model.name, "qwen3.6-plus");
        assert!(model.reasoning_efforts.is_empty());
    }

    #[test]
    fn thinking_type_serializes_snake_case() {
        let json = serde_json::to_string(&ThinkingType::Enabled).unwrap();
        assert_eq!(json, "\"enabled\"");
        let json = serde_json::to_string(&ThinkingType::Disabled).unwrap();
        assert_eq!(json, "\"disabled\"");
    }
}
