//! 领域对象 → TOML 表的序列化
//!
//! 渲染形态取手写配置的常见写法：`limit` / 仅标量价格的 `cost` / 非默认
//! `modalities` 用内联表，含梯度的 `cost` 用表头 + `[[...cost.tiers]]`（内联表
//! 无法跨行容纳数组表）。缺省字段（无价格的 cost、默认 text 模态、空档位
//! 列表）不写——读回路径按同一默认值补齐，写盘前后语义一致。

use fuyao_api::{InputModality, Model, ModelModalities, OutputModality};
use toml_edit::{Array, InlineTable, Item, Table, Value, value};

use super::spec::ProviderSpecData;

/// 供应商段写入：新建完整的 `[providers.<id>]` 表体（含 models 子表）
///
/// api_key 明文不进 toml（密钥隔离红线）：段内只写 `api_key_env_vars` 指针
/// （用户自设变量名，单值所见即所得），明文由调用方负责 upsert 进 .env。
pub(crate) fn provider_to_table(spec: &ProviderSpecData) -> Table {
    let mut table = Table::new();
    table.insert("name", value(spec.name.clone()));
    if let Some(base_url) = &spec.base_url {
        let mut options = Table::new();
        options.insert("base_url", value(base_url.clone()));
        table.insert("options", Item::Table(options));
    }
    if let Some(env_var) = &spec.api_key_env_var {
        let mut vars = Array::new();
        vars.push(Value::from(env_var.clone()));
        table.insert("api_key_env_vars", toml_edit::value(vars));
    }
    replace_models_table(&mut table, &spec.models);
    table
}

/// models 子表整体替换：清空既有条目后按载荷全量重建
///
/// 整体替换语义：载荷未携带的模型 id 随替换消失，载荷内的 id 即落盘键
/// （模型 id 因此可随整体替换变更）；空载荷整体移除 `models` 键，保持段整洁。
pub(crate) fn replace_models_table(provider_table: &mut Table, models: &[(String, Model)]) {
    provider_table.remove("models");
    if models.is_empty() {
        return;
    }
    let mut table = Table::new();
    table.set_implicit(true);
    for (model_id, model) in models {
        table.insert(model_id, Item::Table(model_to_table(model)));
    }
    provider_table.insert("models", Item::Table(table));
}

/// 模型段写入：构造 `[providers.<id>.models.<mid>]` 表体
pub(crate) fn model_to_table(model: &Model) -> Table {
    let mut table = Table::new();
    table.insert("name", value(model.name.clone()));

    // limit：context 必写；input / output 仅在有值时写（读回默认无限制 / 0）
    let mut limit = InlineTable::new();
    limit.insert("context", Value::from(model.limit.context as i64));
    if let Some(input) = model.limit.input {
        limit.insert("input", Value::from(input as i64));
    }
    if model.limit.output > 0 {
        limit.insert("output", Value::from(model.limit.output as i64));
    }
    table.insert("limit", toml_edit::value(limit));

    // cost：有任一价格或梯度才写段
    let cost = &model.cost;
    let has_scalar = cost.input.is_some()
        || cost.output.is_some()
        || cost.reasoning.is_some()
        || cost.cache.is_some();
    if has_scalar && cost.tiers.is_empty() {
        // 纯标量价格：内联表（匹配手写示例 cost = { input = 2, output = 12 }）
        let mut inline = InlineTable::new();
        if let Some(v) = cost.input {
            inline.insert("input", Value::from(v));
        }
        if let Some(v) = cost.output {
            inline.insert("output", Value::from(v));
        }
        if let Some(v) = cost.reasoning {
            inline.insert("reasoning", Value::from(v));
        }
        if let Some(v) = cost.cache {
            inline.insert("cache", Value::from(v));
        }
        table.insert("cost", toml_edit::value(inline));
    } else if !cost.tiers.is_empty() {
        // 含梯度：表头 + 数组表（内联表装不下跨行的 tiers）
        let mut cost_table = Table::new();
        if let Some(v) = cost.input {
            cost_table.insert("input", value(v));
        }
        if let Some(v) = cost.output {
            cost_table.insert("output", value(v));
        }
        if let Some(v) = cost.reasoning {
            cost_table.insert("reasoning", value(v));
        }
        if let Some(v) = cost.cache {
            cost_table.insert("cache", value(v));
        }
        let mut tiers = toml_edit::ArrayOfTables::new();
        for tier in &cost.tiers {
            // 数组表元素必须是表头形态（ArrayOfTables 只收 Table）：渲染为
            // [[providers.<id>.models.<mid>.cost.tiers]] 下的逐项键值
            let mut row = Table::new();
            row.insert("max_tokens", value(tier.max_tokens as i64));
            if let Some(v) = tier.input {
                row.insert("input", value(v));
            }
            if let Some(v) = tier.output {
                row.insert("output", value(v));
            }
            if let Some(v) = tier.reasoning {
                row.insert("reasoning", value(v));
            }
            if let Some(v) = tier.cache {
                row.insert("cache", value(v));
            }
            tiers.push(row);
        }
        cost_table.insert("tiers", Item::ArrayOfTables(tiers));
        table.insert("cost", Item::Table(cost_table));
    }

    // reasoning_efforts：非空才写（读回默认为空表）
    if !model.reasoning_efforts.is_empty() {
        let mut efforts = Array::new();
        for effort in &model.reasoning_efforts {
            efforts.push(Value::from(effort.clone()));
        }
        table.insert("reasoning_efforts", toml_edit::value(efforts));
    }

    // modalities：偏离默认（text/text）才写
    let default_modalities = ModelModalities::default();
    if model.modalities.input != default_modalities.input
        || model.modalities.output != default_modalities.output
    {
        let mut modalities = InlineTable::new();
        let mut input = Array::new();
        for m in &model.modalities.input {
            input.push(Value::from(input_modality_str(m)));
        }
        let mut output = Array::new();
        for m in &model.modalities.output {
            output.push(Value::from(output_modality_str(m)));
        }
        modalities.insert("input", Value::Array(input));
        modalities.insert("output", Value::Array(output));
        table.insert("modalities", toml_edit::value(modalities));
    }

    table
}

/// 输入模态 → 配置字符串（与读回解析的识别集一致：text / image）
fn input_modality_str(m: &InputModality) -> &str {
    match m {
        InputModality::Text => "text",
        InputModality::Image => "image",
    }
}

/// 输出模态 → 配置字符串（与读回解析的识别集一致：text）
fn output_modality_str(m: &OutputModality) -> &str {
    match m {
        OutputModality::Text => "text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::{ModelCost, ModelLimit};

    fn sample_model() -> Model {
        Model {
            name: "deepseek-v4-flash".to_string(),
            cost: ModelCost {
                input: Some(1.0),
                output: Some(2.0),
                reasoning: None,
                cache: Some(0.2),
                tiers: Vec::new(),
            },
            limit: ModelLimit {
                context: 128000,
                input: Some(120000),
                output: 8192,
            },
            reasoning_efforts: vec!["low".to_string(), "high".to_string()],
            modalities: ModelModalities::default(),
        }
    }

    #[test]
    fn model_to_table_writes_core_fields() {
        let table = model_to_table(&sample_model());
        assert_eq!(
            table
                .get("name")
                .and_then(|i| i.as_value())
                .and_then(|v| v.as_str()),
            Some("deepseek-v4-flash")
        );
        let limit = table
            .get("limit")
            .and_then(|i| i.as_value())
            .and_then(|v| v.as_inline_table())
            .unwrap()
            .clone();
        assert_eq!(
            limit.get("context").and_then(|v| v.as_integer()),
            Some(128000)
        );
        assert_eq!(
            limit.get("input").and_then(|v| v.as_integer()),
            Some(120000)
        );
        assert_eq!(limit.get("output").and_then(|v| v.as_integer()), Some(8192));
        // cost 纯标量 → 内联表
        let cost = table.get("cost").and_then(|i| i.as_value()).unwrap();
        let inline = cost.as_inline_table().unwrap();
        assert_eq!(inline.get("input").and_then(|v| v.as_float()), Some(1.0));
        assert_eq!(inline.get("cache").and_then(|v| v.as_float()), Some(0.2));
        assert!(inline.get("reasoning").is_none());
        // reasoning_efforts 非空 → 写数组
        let efforts = table.get("reasoning_efforts").unwrap().as_array().unwrap();
        assert_eq!(efforts.len(), 2);
        // modalities 默认 → 不写
        assert!(table.get("modalities").is_none());
    }

    #[test]
    fn model_to_table_writes_tiers_as_array_of_tables() {
        let mut model = sample_model();
        model.cost = ModelCost {
            input: Some(2.0),
            output: None,
            reasoning: None,
            cache: None,
            tiers: vec![fuyao_api::PriceTier {
                max_tokens: 256000,
                input: Some(2.0),
                output: Some(12.0),
                reasoning: None,
                cache: Some(0.4),
            }],
        };
        let table = model_to_table(&model);
        // 含梯度 → cost 为表头形式（非内联）
        assert!(table.get("cost").unwrap().as_table().is_some());
        let tiers = table
            .get("cost")
            .unwrap()
            .as_table()
            .unwrap()
            .get("tiers")
            .unwrap()
            .as_array_of_tables()
            .unwrap();
        assert_eq!(tiers.len(), 1);
        // 渲染为合法 TOML 且可被 toml_edit 重新解析
        let mut doc = toml_edit::DocumentMut::new();
        let mut providers = Table::new();
        let mut provider = Table::new();
        let mut models = Table::new();
        models.insert("m", Item::Table(model_to_table(&model)));
        provider.insert("models", Item::Table(models));
        providers.insert("p", Item::Table(provider));
        doc.insert("providers", Item::Table(providers));
        let rendered = doc.to_string();
        assert!(rendered.contains("[[providers.p.models.m.cost.tiers]]"));
        rendered.parse::<toml_edit::DocumentMut>().unwrap();
    }

    #[test]
    fn model_to_table_writes_nondefault_modalities() {
        let mut model = sample_model();
        model.modalities.input = vec![InputModality::Text, InputModality::Image];
        let table = model_to_table(&model);
        let modalities = table
            .get("modalities")
            .and_then(|i| i.as_value())
            .unwrap()
            .as_inline_table()
            .unwrap()
            .clone();
        let input = modalities.get("input").unwrap().as_array().unwrap();
        assert_eq!(input.get(0).unwrap().as_str(), Some("text"));
        assert_eq!(input.get(1).unwrap().as_str(), Some("image"));
    }

    #[test]
    fn model_to_table_omits_default_cost_and_efforts() {
        let mut model = sample_model();
        model.cost = ModelCost::default();
        model.reasoning_efforts = Vec::new();
        let table = model_to_table(&model);
        assert!(table.get("cost").is_none());
        assert!(table.get("reasoning_efforts").is_none());
    }

    // ===== models 子表整体替换 =====

    #[test]
    fn replace_models_table_rebuilds_keys_from_payload() {
        let mut provider = Table::new();
        // 预置旧 models（含一个载荷未携带的条目）
        let mut old_models = Table::new();
        old_models.insert("old-a", Item::Table(model_to_table(&sample_model())));
        provider.insert("models", Item::Table(old_models));

        replace_models_table(
            &mut provider,
            &[
                ("renamed-b".to_string(), sample_model()),
                ("c".to_string(), sample_model()),
            ],
        );

        let models = provider.get("models").unwrap().as_table().unwrap();
        assert!(models.get("old-a").is_none(), "载荷未携带的 id 随替换消失");
        assert!(
            models.contains_key("renamed-b"),
            "载荷 id 即落盘键（id 可变更）"
        );
        assert!(models.contains_key("c"));
    }

    #[test]
    fn replace_models_table_with_empty_payload_removes_key() {
        let mut provider = Table::new();
        let mut old_models = Table::new();
        old_models.insert("m", Item::Table(model_to_table(&sample_model())));
        provider.insert("models", Item::Table(old_models));

        replace_models_table(&mut provider, &[]);

        assert!(provider.get("models").is_none(), "空载荷整体移除 models 键");
    }

    #[test]
    fn replace_models_table_renders_quoted_dotted_id() {
        let mut provider = Table::new();
        replace_models_table(
            &mut provider,
            &[("qwen3.6-plus".to_string(), sample_model())],
        );

        // 渲染为合法 TOML 且点号 id 用引号键，可被重新解析
        let mut doc = toml_edit::DocumentMut::new();
        doc.insert(
            "providers",
            Item::Table({
                let mut providers = Table::new();
                providers.insert("p", Item::Table(provider));
                providers
            }),
        );
        let rendered = doc.to_string();
        assert!(
            rendered.contains("\"qwen3.6-plus\""),
            "点号 id 应引号键：{rendered}"
        );
        rendered.parse::<toml_edit::DocumentMut>().unwrap();
    }

    // ===== 供应商段整体序列化 =====

    #[test]
    fn provider_to_table_contains_no_api_key_plaintext() {
        let data = ProviderSpecData {
            name: "P".to_string(),
            base_url: None,
            api_key_env_var: Some("K".to_string()),
            models: Vec::new(),
        };
        let table = provider_to_table(&data);
        // 密钥隔离红线：toml 段内只有指针键，无明文字段
        assert!(table.get("api_key").is_none());
        assert!(table.get("api_key_env_vars").is_some());
    }
}
