//! 模型数据类型定义
//!
//! 定义 LLM 模型的配置信息，包括价格、限制和模态支持。

/// 价格梯度区间
///
/// 定义不同 token 数量区间的价格。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ModelLimit {
    /// 最大上下文窗口（tokens）
    pub context: u32,

    /// 最大输入 tokens
    pub input: Option<u32>,

    /// 最大输出 tokens
    pub output: u32,
}

/// 输入模态
///
/// 模型可接受的输入内容类型。以穷尽枚举约束合法值——新增类型时编译器会
/// 强制处理所有匹配分支，避免松散字符串带来的拼写错误与隐式约定。
///
/// 序列化为 snake_case：`Text` → `"text"`、`Image` → `"image"`，
/// 与配置文件中 `modalities.input = ["text", "image"]` 的写法一致。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputModality {
    /// 纯文本
    Text,
    /// 图片
    Image,
}

/// 输出模态
///
/// 模型可产出的内容类型。与 [`InputModality`] 分离建模——两者拓展方向不同，
/// 独立枚举避免输出侧误声明仅输入合法的类型（如 `Image`）。
///
/// 序列化为 snake_case：`Text` → `"text"`。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputModality {
    /// 纯文本
    Text,
}

/// 模型模态支持
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelModalities {
    /// 输入模态
    pub input: Vec<InputModality>,

    /// 输出模态
    pub output: Vec<OutputModality>,
}

impl Default for ModelModalities {
    fn default() -> Self {
        Self {
            input: vec![InputModality::Text],
            output: vec![OutputModality::Text],
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
        assert_eq!(modalities.input, vec![InputModality::Text]);
        assert_eq!(modalities.output, vec![OutputModality::Text]);
    }

    #[test]
    fn input_modality_serializes_snake_case() {
        // 文本序列化为 "text"
        let json = serde_json::to_string(&vec![InputModality::Text]).unwrap();
        assert_eq!(json, "[\"text\"]");
        // 图片序列化为 "image"
        let json = serde_json::to_string(&vec![InputModality::Image]).unwrap();
        assert_eq!(json, "[\"image\"]");
        // 文本+图片保持声明顺序
        let json = serde_json::to_string(&vec![InputModality::Text, InputModality::Image]).unwrap();
        assert_eq!(json, "[\"text\",\"image\"]");
    }

    #[test]
    fn output_modality_serializes_snake_case() {
        let json = serde_json::to_string(&vec![OutputModality::Text]).unwrap();
        assert_eq!(json, "[\"text\"]");
    }

    #[test]
    fn modalities_round_trips_through_json() {
        // 模拟配置文件写法：input = ["text", "image"], output = ["text"]
        let json = r#"{"input":["text","image"],"output":["text"]}"#;
        let modalities: ModelModalities = serde_json::from_str(json).unwrap();
        assert_eq!(
            modalities.input,
            vec![InputModality::Text, InputModality::Image]
        );
        assert_eq!(modalities.output, vec![OutputModality::Text]);
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
