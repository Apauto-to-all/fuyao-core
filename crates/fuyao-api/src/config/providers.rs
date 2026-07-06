//! Provider 配置容错解析
//!
//! 容错加载：跳过无效的 Provider/Model，加载有效的部分。
//!
//! 为何不走 serde 默认 Deserialize：TOML 区分整数 (`2`) 与浮点数 (`2.0`)，
//! serde 的 `f64` 只接受浮点字面量，用户写 `input = 2` 时价格会被静默丢弃。
//! `toml_number_as_f64` 同时处理两种类型，避免此问题。因此 Provider 段单独走
//! 本模块的容错解析，`FuyaoConfig` 的 `providers` 字段以 `#[serde(skip)]` 跳过 serde。
//!
//! 配置文件结构示例：
//! ```toml
//! [providers.aliyun]
//! name = "阿里云百炼"
//! api_key_env_vars = ["DASHSCOPE_API_KEY"]
//! [providers.aliyun.models.qwen3.6-plus]
//! name = "qwen3.6-plus"
//! cost = { input = 2, output = 12, cache = 0.4 }
//! limit = { context = 1000000, output = 65536 }
//! ```

use std::collections::HashMap;

use crate::provider::{
    Model, ModelCost, ModelLimit, ModelModalities, PriceTier, Provider, ProviderOptions,
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
fn default_modalities_input() -> Vec<String> {
    vec!["text".to_string()]
}

/// 默认输出模态：text
fn default_modalities_output() -> Vec<String> {
    vec!["text".to_string()]
}

/// 解析单个 Model 配置
///
/// - name：必须字段，缺失则返回 None
/// - cost：可选，缺失使用默认值
/// - limit：可选，缺失使用默认值
/// - modalities：可选，缺失使用 ["text"]
fn parse_model(_model_id: &str, model_data: &toml::Value) -> Option<Model> {
    let table = model_data.as_table()?;

    // name 是必须字段
    let name = table.get("name").and_then(|v| v.as_str())?;
    let name = name.to_string();

    // cost 可选
    let cost = table.get("cost").map(parse_cost).unwrap_or_default();

    // limit 可选
    let limit = table
        .get("limit")
        .and_then(|v| {
            let limit_table = v.as_table()?;
            Some(ModelLimit {
                context: limit_table
                    .get("context")
                    .and_then(|v| v.as_integer())
                    .unwrap_or(0) as u32,
                input: limit_table
                    .get("input")
                    .and_then(|v| v.as_integer())
                    .map(|v| v as u32),
                output: limit_table
                    .get("output")
                    .and_then(|v| v.as_integer())
                    .unwrap_or(0) as u32,
            })
        })
        .unwrap_or_default();

    // modalities 可选，默认 ["text"]
    let input = table
        .get("modalities")
        .and_then(|v| v.as_table())
        .and_then(|m| m.get("input").and_then(|v| v.as_array()))
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_else(default_modalities_input);
    let output = table
        .get("modalities")
        .and_then(|v| v.as_table())
        .and_then(|m| m.get("output").and_then(|v| v.as_array()))
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
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

    Some(Model {
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
/// - name：必须字段，缺失则返回 None
/// - models：可选，遍历并解析每个 Model
fn parse_provider(_provider_id: &str, provider_data: &toml::Value) -> Option<Provider> {
    let table = provider_data.as_table()?;

    // name 是必须字段
    let name = table.get("name").and_then(|v| v.as_str())?;
    let name = name.to_string();

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

    // 解析 models（可选）
    let mut models = HashMap::new();
    if let Some(models_table) = table.get("models").and_then(|v| v.as_table()) {
        for (model_id, model_data) in models_table {
            // 容错：跳过无效 Model
            if let Some(model) = parse_model(model_id, model_data) {
                models.insert(model_id.clone(), model);
            }
        }
    }

    Some(Provider {
        name,
        models,
        options,
        api_key_env_vars,
    })
}

/// 加载 Provider 配置
///
/// 容错加载：跳过无效的 Provider/Model，加载有效的部分。
pub fn load_providers(providers_data: &toml::Value) -> HashMap<String, Provider> {
    let empty_table = toml::Table::new();
    let table = providers_data.as_table().unwrap_or(&empty_table);

    let mut valid_providers = HashMap::new();
    for (provider_id, provider_data) in table {
        if let Some(provider) = parse_provider(provider_id, provider_data) {
            valid_providers.insert(provider_id.clone(), provider);
        }
    }

    valid_providers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_providers() {
        let value = toml::Value::Table(toml::Table::new());
        let providers = load_providers(&value);
        assert!(providers.is_empty());
    }

    #[test]
    fn parse_single_provider() {
        let toml_str = r#"
            [providers.aliyun]
            name = "阿里云百炼"
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table);

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
            [providers.deepseek.models.deepseek-v4-flash]
            name = "deepseek-v4-flash"
            [providers.deepseek.models.deepseek-v4-flash.cost]
            input = 1.0
            output = 2.0
            cache = 0.2
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table);

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
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
            [[providers.aliyun.models."qwen3.6-plus".cost.tiers]]
            max_tokens = 256000
            input = 2.0
            output = 12.0
            cache = 0.4
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table);

        let model = &providers["aliyun"].models["qwen3.6-plus"];
        assert_eq!(model.cost.tiers.len(), 1);
        assert_eq!(model.cost.tiers[0].max_tokens, 256000);
        assert_eq!(model.cost.tiers[0].input, Some(2.0));
        assert_eq!(model.cost.tiers[0].output, Some(12.0));
        assert_eq!(model.cost.tiers[0].cache, Some(0.4));
    }

    #[test]
    fn skip_invalid_provider_missing_name() {
        let toml_str = r#"
            [providers.bad]
            # no name field
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table);

        assert!(!providers.contains_key("bad"));
    }

    #[test]
    fn parse_provider_with_api_key_env_vars() {
        let toml_str = r#"
            [providers.aliyun]
            name = "阿里云百炼"
            api_key_env_vars = ["DASHSCOPE_API_KEY"]
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table);

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
            [providers.aliyun.options]
            base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1"
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table);

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
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
            [providers.aliyun.models."qwen3.6-plus".cost]
            input = 2
            output = 12
            reasoning = 4
            cache = 0.4
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table);

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
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
            [[providers.aliyun.models."qwen3.6-plus".cost.tiers]]
            max_tokens = 256000
            input = 2
            output = 12
            cache = 0.4
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table);

        let model = &providers["aliyun"].models["qwen3.6-plus"];
        assert_eq!(model.cost.tiers.len(), 1);
        assert_eq!(model.cost.tiers[0].input, Some(2.0));
        assert_eq!(model.cost.tiers[0].output, Some(12.0));
        assert_eq!(model.cost.tiers[0].cache, Some(0.4));
    }

    #[test]
    fn toml_number_as_f64_handles_integer_and_float() {
        let int_val: toml::Value = 42.into();
        let float_val: toml::Value = 3.14.into();
        let str_val: toml::Value = "hello".into();

        assert_eq!(toml_number_as_f64(&int_val), Some(42.0));
        assert_eq!(toml_number_as_f64(&float_val), Some(3.14));
        assert_eq!(toml_number_as_f64(&str_val), None);
    }

    #[test]
    fn parse_model_defaults_reasoning_false_when_absent() {
        let toml_str = r#"
            [providers.aliyun]
            name = "阿里云百炼"
            [providers.aliyun.models."qwen3.6-plus"]
            name = "qwen3.6-plus"
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table);

        let model = &providers["aliyun"].models["qwen3.6-plus"];
        assert!(model.reasoning_efforts.is_empty());
    }

    #[test]
    fn parse_model_parses_reasoning_and_efforts() {
        let toml_str = r#"
            [providers.deepseek]
            name = "DeepSeek"
            [providers.deepseek.models.deepseek-v4-flash]
            name = "deepseek-v4-flash"
            reasoning_efforts = ["low", "medium", "high", "max"]
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table);

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
            [providers.somevendor.models.weird-model]
            name = "weird-model"
            reasoning_efforts = ["big", "max", "turbo"]
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table);

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
            [providers.deepseek.models.test-model]
            name = "test-model"
            reasoning_efforts = ["high", 123, "max"]
        "#;
        let value: toml::Value = toml::from_str(toml_str).unwrap();
        let providers_table = value.get("providers").unwrap();
        let providers = load_providers(providers_table);

        let model = &providers["deepseek"].models["test-model"];
        assert_eq!(
            model.reasoning_efforts,
            vec!["high".to_string(), "max".to_string()]
        );
    }
}
