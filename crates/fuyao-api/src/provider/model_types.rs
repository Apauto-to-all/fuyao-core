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

/// 模型配置
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Model {
    /// 模型显示名称
    pub name: String,

    /// 价格信息
    pub cost: ModelCost,

    /// 限制信息
    pub limit: ModelLimit,

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
            modalities: ModelModalities::default(),
        };
        assert_eq!(model.name, "qwen3.6-plus");
    }
}
