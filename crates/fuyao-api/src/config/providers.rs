//! Provider 配置解析（数值容错 + 必填校验）
//!
//! - 数值容错：TOML 区分整数 (`2`) 与浮点数 (`2.0`)，serde 的 `f64` 只接受浮点字面量，
//!   用户写 `input = 2` 时价格会被静默丢弃。`toml_number_as_f64` 同时处理两种类型，
//!   避免此问题。因此 Provider 段单独走本模块解析，`FuyaoConfig` 的 `providers`
//!   字段以 `#[serde(skip)]` 跳过 serde。
//! - 必填校验：每个模型条目必须声明 `limit.context` 且为正整数——缺失 / 为 0 /
//!   类型不符直接判为配置错误（fail-loud），错误信息带 `provider_id/model_id` 定位，
//!   整个配置加载失败，由引擎启动时暴露给用户。
//! - 条目残缺报错：Provider / Model 条目存在但残缺（条目不是 table、缺 `name`、
//!   `name` 非字符串）同样直接判为配置错误（fail-loud），错误信息带
//!   `provider_id/model_id` 定位——残缺条目静默消失会让写坏的配置无声丢失，
//!   与必填校验在同一处解析，语义保持一致：一律在加载期拦下。
//! - 段级结构校验：顶层 `providers` 段本身必须是 table——键存在但写成标量 /
//!   数组（如 `providers = "deepseek"`）直接判为配置错误（fail-loud），错误信息
//!   指明实际值的类型；顶层完全没有 `providers` 键是合法状态（未配置任何供应商）。
//!
//! 配置文件结构示例：
//! ```toml
//! [providers.aliyun]
//! name = "阿里云百炼"
//! api_protocol = "openai-completions"
//! api_key_env_vars = ["DASHSCOPE_API_KEY"]
//! [providers.aliyun.models.qwen3.6-plus]
//! name = "qwen3.6-plus"
//! cost = { input = 2, output = 12, cache = 0.4 }
//! limit = { context = 1000000, output = 65536 }
//! ```
//!
//! - `api_protocol` 必填校验：每个供应商必须声明 API 协议且为三选一
//!   （`openai-completions` / `openai-responses` / `anthropic-messages`）——缺失 /
//!   非字符串 / 未知值直接判为配置错误（fail-loud），错误信息带 `provider_id`
//!   定位并列出全部合法取值。

use std::collections::HashMap;

use crate::config::error::ConfigError;
use crate::provider::{
    ApiProtocol, InputModality, Model, ModelCost, ModelLimit, ModelModalities, OutputModality,
    PriceTier, Provider, ProviderOptions,
};

/// 将 TOML 数值转换为 f64（兼容整数和浮点）
///
/// TOML 区分整数 (`2`) 和浮点数 (`2.0`)，`as_float()` 只匹配浮点类型。
/// 此函数同时处理两种类型，避免用户写 `input = 2` 时价格被静默丢弃。
fn toml_number_as_f64(v: &toml::Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
}

/// 解析 Model 价格信息
///
/// 从 TOML table 解析价格字段：
/// - input/output/reasoning/cache：单价格（价格/M tokens）
/// - tiers：梯度价格区间（按 tokens 数量阶梯计价）
fn parse_cost(cost_data: &toml::Value) -> ModelCost {
    let mut cost = ModelCost::default();

    if let Some(table) = cost_data.as_table() {
        // 解析单价格字段
        if let Some(input) = table.get("input").and_then(toml_number_as_f64) {
            cost.input = Some(input);
        }
        if let Some(output) = table.get("output").and_then(toml_number_as_f64) {
            cost.output = Some(output);
        }
        if let Some(reasoning) = table.get("reasoning").and_then(toml_number_as_f64) {
            cost.reasoning = Some(reasoning);
        }
        if let Some(cache) = table.get("cache").and_then(toml_number_as_f64) {
            cost.cache = Some(cache);
        }

        // 解析梯度价格区间
        if let Some(tiers_arr) = table.get("tiers").and_then(|v| v.as_array()) {
            for tier_val in tiers_arr {
                // 必须有 max_tokens 才是有效的 tier
                if let Some(tier_table) = tier_val.as_table()
                    && let Some(max_tokens) =
                        tier_table.get("max_tokens").and_then(|v| v.as_integer())
                {
                    let tier = PriceTier {
                        max_tokens: max_tokens as u32,
                        input: tier_table.get("input").and_then(toml_number_as_f64),
                        output: tier_table.get("output").and_then(toml_number_as_f64),
                        reasoning: tier_table.get("reasoning").and_then(toml_number_as_f64),
                        cache: tier_table.get("cache").and_then(toml_number_as_f64),
                    };
                    cost.tiers.push(tier);
                }
            }
        }
    }

    cost
}

/// 默认输入模态：text
fn default_modalities_input() -> Vec<InputModality> {
    vec![InputModality::Text]
}

/// 默认输出模态：text
fn default_modalities_output() -> Vec<OutputModality> {
    vec![OutputModality::Text]
}

/// 将字符串解析为 [`InputModality`]
///
/// 仅识别 `"text"` / `"image"`（不区分大小写）；未知值返回 `None`，
/// 由调用方决定跳过或记录。
fn parse_input_modality(s: &str) -> Option<InputModality> {
    match s.trim().to_ascii_lowercase().as_str() {
        "text" => Some(InputModality::Text),
        "image" => Some(InputModality::Image),
        _ => None,
    }
}

/// 将字符串解析为 [`OutputModality`]
///
/// 仅识别 `"text"`（不区分大小写）；未知值返回 `None`，由调用方决定跳过或记录。
fn parse_output_modality(s: &str) -> Option<OutputModality> {
    match s.trim().to_ascii_lowercase().as_str() {
        "text" => Some(OutputModality::Text),
        _ => None,
    }
}

/// 解析单个 Model 配置
///
/// - 条目必须是 table：写成标量 / 数组等非 table 形态返回 `Err`（配置错误，
///   fail-loud），错误信息带 `{provider_id}/{model_id}` 定位
/// - name：必须字段且必须为字符串——缺失 / 类型不符返回 `Err`（配置错误，
///   fail-loud），错误信息带 `{provider_id}/{model_id}` 定位
/// - cost：可选，缺失使用默认值
/// - limit.context：**必须**为正整数——缺失 / 为 0 / 类型不符返回 `Err`（配置错误，
///   fail-loud，整个加载失败），错误信息带 `{provider_id}/{model_id}` 定位
/// - limit.input / limit.output：可选，缺省不设限
/// - modalities：可选，缺失使用 ["text"]
fn parse_model(
    provider_id: &str,
    model_id: &str,
    model_data: &toml::Value,
) -> Result<Model, String> {
    let Some(table) = model_data.as_table() else {
        return Err(format!(
            "{provider_id}/{model_id} 的条目不是 table：模型必须以\
             [providers.{provider_id}.models.\"{model_id}\"] 表形式声明"
        ));
    };

    // name 是必须字段
    let Some(name_value) = table.get("name") else {
        return Err(format!(
            "{provider_id}/{model_id} 的 name 缺失：必须声明为字符串\
             （模型显示名，如 name = \"{model_id}\"）"
        ));
    };
    let Some(name) = name_value.as_str() else {
        return Err(format!(
            "{provider_id}/{model_id} 的 name 类型错误：必须为字符串"
        ));
    };
    let name = name.to_string();

    // cost 可选
    let cost = table.get("cost").map(parse_cost).unwrap_or_default();

    // limit.context 必填且为正整数：压缩触发公式直接消费该值，缺声明会导致
    // usable=0、阈值恒真、每轮必压缩，因此在加载期拦下而非运行期兜底
    let limit_table = table.get("limit").and_then(|v| v.as_table());
    let context = match limit_table.and_then(|lt| lt.get("context")) {
        // 缺 limit 段或缺 context 键同报缺失
        None => {
            return Err(format!(
                "{provider_id}/{model_id} 的 limit.context 缺失：必须声明为正整数\
                 （上下文窗口 tokens，如 limit = {{ context = 128000 }}）"
            ));
        }
        Some(v) => match v.as_integer() {
            None => {
                return Err(format!(
                    "{provider_id}/{model_id} 的 limit.context 类型错误：必须为正整数"
                ));
            }
            Some(n) if n > 0 && n <= u32::MAX as i64 => n as u32,
            Some(n) => {
                return Err(format!(
                    "{provider_id}/{model_id} 的 limit.context 非法（{n}）：必须为正整数"
                ));
            }
        },
    };
    let limit = ModelLimit {
        context,
        input: limit_table
            .and_then(|lt| lt.get("input"))
            .and_then(|v| v.as_integer())
            .map(|v| v as u32),
        output: limit_table
            .and_then(|lt| lt.get("output"))
            .and_then(|v| v.as_integer())
            .unwrap_or(0) as u32,
    };

    // modalities 可选，默认 ["text"]；非法模态值静默跳过
    let input = table
        .get("modalities")
        .and_then(|v| v.as_table())
        .and_then(|m| m.get("input").and_then(|v| v.as_array()))
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().and_then(parse_input_modality))
                .collect()
        })
        .unwrap_or_else(default_modalities_input);
    let output = table
        .get("modalities")
        .and_then(|v| v.as_table())
        .and_then(|m| m.get("output").and_then(|v| v.as_array()))
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().and_then(parse_output_modality))
                .collect()
        })
        .unwrap_or_else(default_modalities_output);

    // reasoning_efforts：支持的强度档位名（用户自定义字符串数组，原样收集透传）
    let reasoning_efforts = table
        .get("reasoning_efforts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    Ok(Model {
        name,
        cost,
        limit,
        reasoning_efforts,
        modalities: ModelModalities { input, output },
    })
}

/// 解析 ProviderOptions
fn parse_provider_options(table: &toml::Table) -> ProviderOptions {
    let Some(options_table) = table.get("options").and_then(|v| v.as_table()) else {
        return ProviderOptions::default();
    };

    let base_url = options_table
        .get("base_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let api_key = options_table
        .get("api_key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    ProviderOptions { base_url, api_key }
}

/// 解析单个 Provider 配置
///
/// - 条目必须是 table：写成标量 / 数组等非 table 形态返回 `Err`（配置错误，
///   fail-loud），错误信息带 `{provider_id}` 定位
/// - name：必须字段且必须为字符串——缺失 / 类型不符返回 `Err`（配置错误，
///   fail-loud），错误信息带 `{provider_id}` 定位
/// - api_protocol：必须字段且为三选一枚举（`openai-completions` /
///   `openai-responses` / `anthropic-messages`）——缺失 / 非字符串 / 未知值
///   返回 `Err`（配置错误，fail-loud），错误信息列出全部合法取值
/// - models：可选，遍历并解析每个 Model；模型条目残缺或 `limit.context` 非法时
///   返回 `Err`（配置错误，整个加载失败）
fn parse_provider(provider_id: &str, provider_data: &toml::Value) -> Result<Provider, String> {
    let Some(table) = provider_data.as_table() else {
        return Err(format!(
            "{provider_id} 的条目不是 table：Provider 必须以\
             [providers.{provider_id}] 表形式声明"
        ));
    };

    // name 是必须字段
    let Some(name_value) = table.get("name") else {
        return Err(format!(
            "{provider_id} 的 name 缺失：必须声明为字符串\
             （Provider 显示名，如 name = \"{provider_id}\"）"
        ));
    };
    let Some(name) = name_value.as_str() else {
        return Err(format!("{provider_id} 的 name 类型错误：必须为字符串"));
    };
    let name = name.to_string();

    // api_protocol 必填且为三选一：协议决定供应商实例构造的分派目标，缺失 /
    // 类型不符 / 未知值在加载期拦下（fail-loud），错误信息列出全部合法取值
    let valid_values = ApiProtocol::ALL_CONFIG_STRS.join(" / ");
    let api_protocol = match table.get("api_protocol") {
        None => {
            return Err(format!(
                "{provider_id} 的 api_protocol 缺失：必须声明为三选一\
                 （{valid_values}，如 api_protocol = \"openai-completions\"）"
            ));
        }
        Some(value) => match value.as_str() {
            None => {
                return Err(format!(
                    "{provider_id} 的 api_protocol 类型错误：必须为字符串（三选一 {valid_values}）"
                ));
            }
            Some(s) => match ApiProtocol::from_config_str(s) {
                Some(protocol) => protocol,
                None => {
                    return Err(format!(
                        "{provider_id} 的 api_protocol 非法（{s}）：合法取值为 {valid_values}\
                         （如 api_protocol = \"openai-completions\"）"
                    ));
                }
            },
        },
    };

    // 解析 api_key_env_vars（可选）
    let api_key_env_vars = table
        .get("api_key_env_vars")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // 解析 options（可选）
    let options = parse_provider_options(table);

    // 解析 models（可选）；条目残缺或 limit.context 非法直接冒泡（fail-loud）
    let mut models = HashMap::new();
    if let Some(models_table) = table.get("models").and_then(|v| v.as_table()) {
        for (model_id, model_data) in models_table {
            let model = parse_model(provider_id, model_id, model_data)?;
            models.insert(model_id.clone(), model);
        }
    }

    Ok(Provider {
        name,
        api_protocol,
        models,
        options,
        api_key_env_vars,
    })
}

/// 给出 TOML 值的中文类型描述，供结构错误信息指明实际写出的类型
///
/// 穷举全部变体：TOML 值类型集合变化时此处强制同步补齐中文名。
fn toml_type_desc(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "字符串",
        toml::Value::Integer(_) => "整数",
        toml::Value::Float(_) => "浮点数",
        toml::Value::Boolean(_) => "布尔值",
        toml::Value::Datetime(_) => "日期时间",
        toml::Value::Array(_) => "数组",
        toml::Value::Table(_) => "table",
    }
}

/// 加载 Provider 配置
///
/// 校验语义见模块注释：`providers` 段非 table 返回
/// [`ConfigError::InvalidProvidersSection`]（指明实际类型）；Provider / Model
/// 条目残缺（非 table、缺 `name`、`name` 非字符串）或模型 `limit.context`
/// 缺失 / 非正整数返回 [`ConfigError::InvalidModel`]（带 `provider_id/model_id`
/// 定位）——均使整个加载失败。
pub fn load_providers(
    providers_data: &toml::Value,
) -> Result<HashMap<String, Provider>, ConfigError> {
    // providers 段必须是 table：键存在但写成标量 / 数组属于结构写错，判为配置
    // 错误（fail-loud）——按空表放行会让结构错误伪装成「未配置任何供应商」，
    // 把排查方向带偏到 API Key 上
    let table = providers_data.as_table().ok_or_else(|| {
        ConfigError::InvalidProvidersSection(format!(
            "providers 段不是 table（实际为{}）：\
             必须以 [providers.<id>] 表形式声明供应商",
            toml_type_desc(providers_data)
        ))
    })?;

    let mut valid_providers = HashMap::new();
    for (provider_id, provider_data) in table {
        let provider =
            parse_provider(provider_id, provider_data).map_err(ConfigError::InvalidModel)?;
        valid_providers.insert(provider_id.clone(), provider);
    }

    Ok(valid_providers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_providers() {
        let value = toml::Value::Table(toml::Table::new());
        let providers = load_providers(&value).unwrap();
        assert!(providers.is_empty());
    }

    #[test]
    fn parse_single_provider() {
        let toml_str = r#"
            [providers.aliyun]
            name = "阿里云百炼"
            api_protocol = "openai-completions"
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
            limit = { context = 131072 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table).unwrap();

        assert_eq!(providers.len(), 1);
        assert!(providers.contains_key("aliyun"));
        assert_eq!(providers["aliyun"].name, "阿里云百炼");
        assert!(providers["aliyun"].models.contains_key("qwen3.6-plus"));
    }

    #[test]
    fn parse_provider_with_cost() {
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = "openai-completions"
            [providers.deepseek.models.deepseek-v4-flash]
            name = "deepseek-v4-flash"
            limit = { context = 128000 }
            [providers.deepseek.models.deepseek-v4-flash.cost]
            input = 1.0
            output = 2.0
            cache = 0.2
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table).unwrap();

        let model = &providers["deepseek"].models["deepseek-v4-flash"];
        assert_eq!(model.cost.input, Some(1.0));
        assert_eq!(model.cost.output, Some(2.0));
        assert_eq!(model.cost.cache, Some(0.2));
    }

    #[test]
    fn parse_provider_with_tiers() {
        let toml_str = r#"
            [providers.aliyun]
            name = "阿里云百炼"
            api_protocol = "openai-completions"
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
            limit = { context = 131072 }
            [[providers.aliyun.models."qwen3.6-plus".cost.tiers]]
            max_tokens = 256000
            input = 2.0
            output = 12.0
            cache = 0.4
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table).unwrap();

        let model = &providers["aliyun"].models["qwen3.6-plus"];
        assert_eq!(model.cost.tiers.len(), 1);
        assert_eq!(model.cost.tiers[0].max_tokens, 256000);
        assert_eq!(model.cost.tiers[0].input, Some(2.0));
        assert_eq!(model.cost.tiers[0].output, Some(12.0));
        assert_eq!(model.cost.tiers[0].cache, Some(0.4));
    }

    #[test]
    fn parse_provider_with_api_key_env_vars() {
        let toml_str = r#"
            [providers.aliyun]
            name = "阿里云百炼"
            api_protocol = "openai-completions"
            api_key_env_vars = ["DASHSCOPE_API_KEY"]
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
            limit = { context = 131072 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table).unwrap();

        assert_eq!(
            providers["aliyun"].api_key_env_vars,
            vec!["DASHSCOPE_API_KEY"]
        );
    }

    #[test]
    fn parse_provider_with_options() {
        let toml_str = r#"
            [providers.aliyun]
            name = "阿里云百炼"
            api_protocol = "openai-completions"
            [providers.aliyun.options]
            base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
            limit = { context = 131072 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table).unwrap();

        assert_eq!(
            providers["aliyun"].options.base_url.as_deref(),
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1")
        );
    }

    #[test]
    fn parse_cost_with_integer_prices() {
        let toml_str = r#"
            [providers.aliyun]
            name = "阿里云百炼"
            api_protocol = "openai-completions"
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
            limit = { context = 131072 }
            [providers.aliyun.models."qwen3.6-plus".cost]
            input = 2
            output = 12
            reasoning = 4
            cache = 0.4
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table).unwrap();

        let model = &providers["aliyun"].models["qwen3.6-plus"];
        assert_eq!(model.cost.input, Some(2.0));
        assert_eq!(model.cost.output, Some(12.0));
        assert_eq!(model.cost.reasoning, Some(4.0));
        assert_eq!(model.cost.cache, Some(0.4));
    }

    #[test]
    fn parse_tiers_with_integer_prices() {
        let toml_str = r#"
            [providers.aliyun]
            name = "阿里云百炼"
            api_protocol = "openai-completions"
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
            limit = { context = 131072 }
            [[providers.aliyun.models."qwen3.6-plus".cost.tiers]]
            max_tokens = 256000
            input = 2
            output = 12
            cache = 0.4
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table).unwrap();

        let model = &providers["aliyun"].models["qwen3.6-plus"];
        assert_eq!(model.cost.tiers.len(), 1);
        assert_eq!(model.cost.tiers[0].input, Some(2.0));
        assert_eq!(model.cost.tiers[0].output, Some(12.0));
        assert_eq!(model.cost.tiers[0].cache, Some(0.4));
    }

    #[test]
    fn toml_number_as_f64_handles_integer_and_float() {
        let int_val: toml::Value = 42.into();
        let float_val: toml::Value = 2.5.into();
        let str_val: toml::Value = "hello".into();

        assert_eq!(toml_number_as_f64(&int_val), Some(42.0));
        assert_eq!(toml_number_as_f64(&float_val), Some(2.5));
        assert_eq!(toml_number_as_f64(&str_val), None);
    }

    #[test]
    fn parse_model_defaults_reasoning_false_when_absent() {
        let toml_str = r#"
            [providers.aliyun]
            name = "阿里云百炼"
            api_protocol = "openai-completions"
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
            limit = { context = 131072 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table).unwrap();

        let model = &providers["aliyun"].models["qwen3.6-plus"];
        assert!(model.reasoning_efforts.is_empty());
    }

    #[test]
    fn parse_model_parses_reasoning_and_efforts() {
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = "openai-completions"
            [providers.deepseek.models.deepseek-v4-flash]
            name = "deepseek-v4-flash"
            limit = { context = 128000 }
            reasoning_efforts = ["low", "medium", "high", "max"]
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table).unwrap();

        let model = &providers["deepseek"].models["deepseek-v4-flash"];
        assert_eq!(
            model.reasoning_efforts,
            vec![
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "max".to_string()
            ]
        );
    }

    #[test]
    fn parse_model_accepts_arbitrary_effort_strings() {
        // 档位名由用户自定义，任意字符串都接受（如 "big" 不在常见枚举内也能配置）
        let toml_str = r#"
            [providers.somevendor]
            name = "SomeVendor"
            api_protocol = "openai-completions"
            [providers.somevendor.models.weird-model]
            name = "weird-model"
            limit = { context = 64000 }
            reasoning_efforts = ["big", "max", "turbo"]
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table).unwrap();

        let model = &providers["somevendor"].models["weird-model"];
        assert_eq!(
            model.reasoning_efforts,
            vec!["big".to_string(), "max".to_string(), "turbo".to_string()]
        );
    }

    #[test]
    fn parse_model_skips_non_string_effort_entries() {
        // 非字符串项（如误写数字）静默跳过，只保留字符串项
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = "openai-completions"
            [providers.deepseek.models.test-model]
            name = "test-model"
            limit = { context = 64000 }
            reasoning_efforts = ["high", 123, "max"]
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table).unwrap();

        let model = &providers["deepseek"].models["test-model"];
        assert_eq!(
            model.reasoning_efforts,
            vec!["high".to_string(), "max".to_string()]
        );
    }

    // ===== limit.context 必填校验 =====

    #[test]
    fn missing_limit_context_fails_with_model_name() {
        // 完全没写 limit 段：配置加载失败，错误信息带 provider_id/model_id 定位
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = "openai-completions"
            [providers.deepseek.models.deepseek-v4-flash]
            name = "deepseek-v4-flash"
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("deepseek/deepseek-v4-flash"),
            "错误信息应含模型名：{msg}"
        );
        assert!(
            msg.contains("limit.context"),
            "错误信息应指向 limit.context：{msg}"
        );
        assert!(msg.contains("缺失"), "错误信息应说明缺失：{msg}");
    }

    #[test]
    fn limit_section_without_context_key_fails() {
        // 写了 [limit] 段但缺 context 键：同样判缺失失败
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = "openai-completions"
            [providers.deepseek.models.deepseek-v4-flash]
            name = "deepseek-v4-flash"
            limit = { output = 8192 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("deepseek/deepseek-v4-flash"),
            "错误信息应含模型名：{msg}"
        );
        assert!(msg.contains("缺失"), "错误信息应说明缺失：{msg}");
    }

    #[test]
    fn zero_limit_context_fails() {
        // context = 0 会使压缩触发公式 usable=0、阈值恒真，加载期必须拦下
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = "openai-completions"
            [providers.deepseek.models.deepseek-v4-flash]
            name = "deepseek-v4-flash"
            limit = { context = 0 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("deepseek/deepseek-v4-flash"),
            "错误信息应含模型名：{msg}"
        );
        assert!(msg.contains("正整数"), "错误信息应指出必须为正整数：{msg}");
    }

    #[test]
    fn negative_limit_context_fails() {
        // 负数同属非法值
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = "openai-completions"
            [providers.deepseek.models.deepseek-v4-flash]
            name = "deepseek-v4-flash"
            limit = { context = -1 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        assert!(err.to_string().contains("正整数"));
    }

    #[test]
    fn non_integer_limit_context_fails() {
        // 类型不符（字符串 / 浮点）失败；浮点虽是 TOML 数值，但 context 必须是整数
        for bad in [r#"context = "128000""#, "context = 128000.5"] {
            let toml_str = format!(
                r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = "openai-completions"
            [providers.deepseek.models.deepseek-v4-flash]
            name = "deepseek-v4-flash"
            limit = {{ {bad} }}
        "#
            );
            let value: toml::Value = toml::from_str(&toml_str).unwrap();
            let err = load_providers(value.get("providers").unwrap()).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("deepseek/deepseek-v4-flash"),
                "错误信息应含模型名：{msg}"
            );
            assert!(
                msg.contains("类型错误") || msg.contains("正整数"),
                "错误信息应指出类型/取值问题：{msg}"
            );
        }
    }

    #[test]
    fn valid_limit_context_parses_with_optional_fields() {
        // 合法声明通过：context 进 ModelLimit；input / output 可选
        let toml_str = r#"
            [providers.aliyun]
            name = "阿里云百炼"
            api_protocol = "openai-completions"
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
            limit = { context = 1000000, input = 900000, output = 65536 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers = load_providers(value.get("providers").unwrap()).unwrap();

        let model = &providers["aliyun"].models["qwen3.6-plus"];
        assert_eq!(model.limit.context, 1_000_000);
        assert_eq!(model.limit.input, Some(900_000));
        assert_eq!(model.limit.output, 65536);
    }

    // ===== api_protocol 必填校验 =====

    /// 三选一全部合法取值可解析进 Provider 配置
    #[test]
    fn parse_all_api_protocol_values() {
        for (config_str, expected) in [
            ("openai-completions", ApiProtocol::OpenaiCompletions),
            ("openai-responses", ApiProtocol::OpenaiResponses),
            ("anthropic-messages", ApiProtocol::AnthropicMessages),
        ] {
            let toml_str = format!(
                r#"
            [providers.vendor]
            name = "V"
            api_protocol = "{config_str}"
            [providers.vendor.models.m]
            name = "m"
            limit = {{ context = 64000 }}
        "#
            );
            let value: toml::Value = toml::from_str(&toml_str).unwrap();
            let providers = load_providers(value.get("providers").unwrap()).unwrap();
            assert_eq!(
                providers["vendor"].api_protocol, expected,
                "合法取值 {config_str} 应解析为对应枚举"
            );
        }
    }

    /// 缺失 api_protocol：加载失败，错误信息带 provider_id 定位并列出全部合法取值
    #[test]
    fn missing_api_protocol_fails_with_valid_values() {
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            [providers.deepseek.models.deepseek-v4-flash]
            name = "deepseek-v4-flash"
            limit = { context = 128000 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("deepseek"), "错误信息应含 provider_id：{msg}");
        assert!(msg.contains("api_protocol"), "错误应指向字段：{msg}");
        assert!(
            msg.contains("openai-completions / openai-responses / anthropic-messages"),
            "错误信息应列出全部合法取值：{msg}"
        );
    }

    /// 未知值（如 "openai"）：错误信息含实际写出值与合法取值
    #[test]
    fn unknown_api_protocol_fails_with_actual_value() {
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = "openai"
            [providers.deepseek.models.deepseek-v4-flash]
            name = "deepseek-v4-flash"
            limit = { context = 128000 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("非法（openai）"),
            "错误信息应含实际写出值：{msg}"
        );
        assert!(
            msg.contains("openai-completions / openai-responses / anthropic-messages"),
            "错误信息应列出全部合法取值：{msg}"
        );
    }

    /// 非字符串（如整数）：判为类型错误
    #[test]
    fn non_string_api_protocol_fails() {
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = 1
            [providers.deepseek.models.deepseek-v4-flash]
            name = "deepseek-v4-flash"
            limit = { context = 128000 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        assert!(
            err.to_string()
                .contains("api_protocol 类型错误：必须为字符串"),
            "非字符串应报类型错误：{err}"
        );
    }

    // ===== 条目残缺必填报错（非 table / name） =====

    #[test]
    fn missing_provider_name_fails_with_provider_id() {
        // Provider 条目存在但缺 name：配置加载失败，错误信息带 provider_id 定位
        let toml_str = r#"
            [providers.bad]
            # no name field
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bad"), "错误信息应含 provider_id：{msg}");
        assert!(msg.contains("name"), "错误信息应指向 name 字段：{msg}");
        assert!(msg.contains("缺失"), "错误信息应说明缺失：{msg}");
    }

    #[test]
    fn non_string_provider_name_fails() {
        // name 写成非字符串（如数字）：判为配置错误而非静默跳过
        let toml_str = r#"
            [providers.bad]
            name = 123
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bad"), "错误信息应含 provider_id：{msg}");
        assert!(msg.contains("类型错误"), "错误信息应指出类型问题：{msg}");
    }

    #[test]
    fn non_table_provider_entry_fails() {
        // Provider 条目写成标量值（非 table）：判为配置错误而非静默消失
        let toml_str = r#"
            [providers]
            broken = "oops"
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("broken"), "错误信息应含 provider_id：{msg}");
        assert!(
            msg.contains("不是 table"),
            "错误信息应指出条目非 table：{msg}"
        );
    }

    #[test]
    fn missing_name_fails_with_model_name() {
        // 模型条目存在但缺 name：配置加载失败，错误信息带 provider_id/model_id 定位
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = "openai-completions"
            [providers.deepseek.models.no-name-model]
            limit = { context = 64000 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("deepseek/no-name-model"),
            "错误信息应含模型名：{msg}"
        );
        assert!(msg.contains("name"), "错误信息应指向 name 字段：{msg}");
        assert!(msg.contains("缺失"), "错误信息应说明缺失：{msg}");
    }

    #[test]
    fn non_string_model_name_fails() {
        // 模型 name 写成非字符串（如数字）：判为配置错误而非静默跳过
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = "openai-completions"
            [providers.deepseek.models.bad-model]
            name = 123
            limit = { context = 64000 }
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("deepseek/bad-model"),
            "错误信息应含模型名：{msg}"
        );
        assert!(msg.contains("类型错误"), "错误信息应指出类型问题：{msg}");
    }

    #[test]
    fn non_table_model_entry_fails() {
        // 模型条目写成标量值（非 table）：判为配置错误而非静默消失
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            api_protocol = "openai-completions"
            [providers.deepseek.models]
            broken = "oops"
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let err = load_providers(value.get("providers").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("deepseek/broken"), "错误信息应含模型名：{msg}");
        assert!(
            msg.contains("不是 table"),
            "错误信息应指出条目非 table：{msg}"
        );
    }

    // ===== providers 段级结构校验 =====

    #[test]
    fn non_table_providers_section_fails_with_actual_type() {
        // 顶层 providers 键存在但值不是 table（字符串 / 整数 / 数组）：判为配置
        // 错误，错误信息指明实际类型与 table 要求
        for (toml_snippet, type_desc) in [
            (r#"providers = "deepseek""#, "字符串"),
            ("providers = 123", "整数"),
            (r#"providers = ["a"]"#, "数组"),
        ] {
            let value: toml::Value = toml::from_str(toml_snippet).unwrap();
            let err = load_providers(value.get("providers").unwrap()).unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidProvidersSection(_)),
                "应为段级结构错误变体：{err}"
            );
            let msg = err.to_string();
            assert!(msg.contains("table"), "错误信息应指向 table：{msg}");
            assert!(
                msg.contains(type_desc),
                "错误信息应含实际类型（{type_desc}）：{msg}"
            );
        }
    }
}
