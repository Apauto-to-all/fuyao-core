//! fuyao.toml 段级变更原语：insert / patch / remove
//!
//! 用 toml_edit 做段级增量 patch——只修改目标 `[providers.<id>]` 子树内的键，
//! 子树之外的注释、未知字段、手写格式逐字保留。段导航（providers 表定位、
//! 点键 / 非表结构的拒写判定）与 options 子段的手写形态兼容（表头 / 内联）
//! 是本模块的内部实现。

use fuyao_api::Model;
use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

use super::error::ProviderAdminError;
use super::serialize::{provider_to_table, replace_models_table};
use super::spec::ProviderSpec;
use super::spec::ProviderSpecData;

/// 创建：目标 id 不存在时插入完整 `[providers.<id>]` 段（含全量模型）
///
/// 已存在返回 [`ProviderAdminError::AlreadyExists`]（id 不可改名，换 id 走
/// 「建新 + 删旧」）。
pub fn insert_provider(
    doc: &mut DocumentMut,
    provider_id: &str,
    data: &ProviderSpecData,
) -> Result<(), ProviderAdminError> {
    let providers = providers_table_mut(doc)?;
    if providers.contains_key(provider_id) {
        return Err(ProviderAdminError::AlreadyExists(format!(
            "providers.{provider_id} 已存在（id 不可改名，换 id 走「建新 + 删旧」）"
        )));
    }
    providers.insert(provider_id, Item::Table(provider_to_table(data)));
    Ok(())
}

/// 更新：目标段管理字段 patch + `models` 子表整表替换
///
/// name / `api_protocol` 覆盖为载荷值（协议必有字段，恒写完整值）；base_url
/// Some 覆盖 / None 清除；`api_key_env_vars` 指针所见即所得（Some 落单值 /
/// None 移除键）；`spec.api_key` 有值时连带移除段内残留的 `options.api_key`
/// 明文（明文密钥不进 toml）；models 按载荷整表替换（未携带的模型消失，
/// 载荷 id 即落盘键）。段内其他键（用户手写的未知字段）不动。目标不存在返回
/// [`ProviderAdminError::NotFound`]。
pub fn patch_provider(
    doc: &mut DocumentMut,
    provider_id: &str,
    spec: &ProviderSpec,
) -> Result<(), ProviderAdminError> {
    let providers = providers_table_mut(doc)?;
    let table = provider_table_mut(providers, provider_id)?
        .ok_or_else(|| ProviderAdminError::NotFound(format!("providers.{provider_id} 不存在")))?;

    // name：覆盖为载荷值
    table.insert("name", value(spec.name.clone()));

    // api_protocol：覆盖为载荷值（必填无缺省，完整期望状态恒携带）
    table.insert("api_protocol", value(spec.api_protocol.as_config_str()));

    // base_url：Some 覆盖 / None 清除（options 兼容表头与内联两种手写形态，
    // 空则整体移除）
    match &spec.base_url {
        Some(base_url) => options_set_base_url(table, provider_id, base_url)?,
        None => options_remove_key(table, "base_url"),
    }

    // 指针所见即所得：Some 落单值 / None 移除键
    match &spec.api_key_env_var {
        Some(env_var) => {
            let mut vars = Array::new();
            vars.push(Value::from(env_var.clone()));
            table.insert("api_key_env_vars", toml_edit::value(vars));
        }
        None => {
            table.remove("api_key_env_vars");
        }
    }

    // 明文密钥不进 toml：写 .env 时连带移除段内残留明文（含内联形态）
    if spec.api_key.is_some() {
        options_remove_key(table, "api_key");
    }

    let model_pairs: Vec<(String, Model)> = spec
        .models
        .iter()
        .map(|entry| (entry.id.clone(), entry.model.clone()))
        .collect();
    replace_models_table(table, &model_pairs);
    Ok(())
}

/// 删除：移除 `[providers.<id>]` 段（级联其全部模型）
///
/// providers 段删空后连带移除空表头，避免残留空 `[providers]`。目标不存在
/// 返回 [`ProviderAdminError::NotFound`]。
pub fn remove_provider(doc: &mut DocumentMut, provider_id: &str) -> Result<(), ProviderAdminError> {
    let providers = providers_table_mut(doc)?;
    if providers.remove(provider_id).is_none() {
        return Err(ProviderAdminError::NotFound(format!(
            "providers.{provider_id} 不存在"
        )));
    }
    if providers.is_empty() {
        doc.as_table_mut().remove("providers");
    }
    Ok(())
}

// ── 段导航（内部实现）────────────────────────────────────────

/// 取（不存在则创建）顶层 providers 表
///
/// 新建的表标记为隐式（`set_implicit(true)`）：渲染为 `[providers.<id>]`
/// 层级式表头，不产生多余的空 `[providers]` 表头。段存在但值不是 table
/// （标量 / 数组 / 数组表）返回 [`ProviderAdminError::InvalidSection`]。
fn providers_table_mut(doc: &mut DocumentMut) -> Result<&mut Table, ProviderAdminError> {
    if doc.as_table().get("providers").is_none() {
        let mut table = Table::new();
        table.set_implicit(true);
        doc.as_table_mut().insert("providers", Item::Table(table));
    }
    doc.as_table_mut()
        .get_mut("providers")
        .and_then(|item| item.as_table_mut())
        .ok_or_else(|| {
            ProviderAdminError::InvalidSection(
                "providers 段不是 table：必须以 [providers.<id>] 表形式声明供应商".to_string(),
            )
        })
}

/// 取已存在的 `[providers.<id>]` 表（不创建）
///
/// - `Ok(Some(table))`：目标供应商段存在（且为表头形式）
/// - `Ok(None)`：目标供应商段不存在
/// - `Err(InvalidSection)`：段存在但不是 table，或是 `a.b = ...` 点键写法——
///   点键表上无法安全插入子表（models），要求改写为表头形式
fn provider_table_mut<'a>(
    providers: &'a mut Table,
    provider_id: &str,
) -> Result<Option<&'a mut Table>, ProviderAdminError> {
    let Some(item) = providers.get_mut(provider_id) else {
        return Ok(None);
    };
    let table = item.as_table_mut().ok_or_else(|| {
        ProviderAdminError::InvalidSection(format!(
            "providers.{provider_id} 段不是 table：必须以 [providers.{provider_id}] 表形式声明"
        ))
    })?;
    if table.is_dotted() {
        return Err(ProviderAdminError::InvalidSection(format!(
            "providers.{provider_id} 以点键（a.b = ...）形式声明：\
             请改写为 [providers.{provider_id}] 表头形式后再由管理 API 修改"
        )));
    }
    Ok(Some(table))
}

/// 把 base_url 写入供应商段的 options 子段
///
/// options 兼容两种手写形态：表头（`[providers.<id>.options]`）与内联
/// （`options = { ... }`），就地 patch 现有形态；不存在时新建表头形态
/// （渲染为 `[providers.<id>.options]`）。options 段是其他形态（标量等）时
/// 拒绝写回。
fn options_set_base_url(
    table: &mut Table,
    provider_id: &str,
    base_url: &str,
) -> Result<(), ProviderAdminError> {
    match table.get_mut("options") {
        Some(Item::Table(options)) => {
            options.insert("base_url", value(base_url));
            Ok(())
        }
        Some(Item::Value(v)) => {
            let Some(inline) = v.as_inline_table_mut() else {
                return Err(ProviderAdminError::InvalidSection(format!(
                    "providers.{provider_id}.options 段不是 table，无法写入 base_url"
                )));
            };
            inline.insert("base_url", base_url.into());
            Ok(())
        }
        _ => {
            let mut options = Table::new();
            options.insert("base_url", value(base_url));
            table.insert("options", Item::Table(options));
            Ok(())
        }
    }
}

/// 从供应商段的 options 子段（表头 / 内联两形态）移除指定键；移除后 options
/// 为空则整体移除该键
fn options_remove_key(table: &mut Table, key: &str) {
    match table.get_mut("options") {
        Some(Item::Table(options)) => {
            options.remove(key);
            if options.is_empty() {
                table.remove("options");
            }
        }
        Some(Item::Value(v)) => {
            if let Some(inline) = v.as_inline_table_mut() {
                inline.remove(key);
                if inline.is_empty() {
                    table.remove("options");
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::{ModelCost, ModelLimit, ModelModalities};

    /// 构造最小可用模型（limit.context 为正整数）
    fn sample_model() -> Model {
        Model {
            name: "deepseek-v4-flash".to_string(),
            cost: ModelCost::default(),
            limit: ModelLimit {
                context: 128000,
                input: None,
                output: 0,
            },
            reasoning_efforts: Vec::new(),
            modalities: ModelModalities::default(),
        }
    }

    /// 构造最小写回载荷
    fn sample_spec() -> ProviderSpec {
        ProviderSpec {
            name: "DeepSeek".to_string(),
            api_protocol: fuyao_api::ApiProtocol::OpenaiCompletions,
            base_url: Some("https://api.deepseek.com".to_string()),
            api_key_env_var: Some("MY_DEEPSEEK_KEY".to_string()),
            api_key: None,
            models: vec![super::super::spec::ProviderModelSpec {
                id: "deepseek-v4-flash".to_string(),
                model: sample_model(),
            }],
        }
    }

    // ===== insert_provider =====

    #[test]
    fn insert_provider_rejects_duplicate_id() {
        let mut doc: DocumentMut = "[providers.p]\nname = \"old\"\n".parse().unwrap();
        let data = ProviderSpecData {
            name: "P".to_string(),
            api_protocol: fuyao_api::ApiProtocol::OpenaiCompletions,
            base_url: None,
            api_key_env_var: None,
            models: Vec::new(),
        };
        assert!(matches!(
            insert_provider(&mut doc, "p", &data),
            Err(ProviderAdminError::AlreadyExists(_))
        ));
    }

    #[test]
    fn insert_provider_renders_full_section() {
        let mut doc = DocumentMut::new();
        let spec = sample_spec();
        let data = ProviderSpecData {
            name: spec.name.clone(),
            api_protocol: spec.api_protocol,
            base_url: spec.base_url.clone(),
            api_key_env_var: spec.api_key_env_var.clone(),
            models: vec![("deepseek-v4-flash".to_string(), sample_model())],
        };
        insert_provider(&mut doc, "deepseek", &data).unwrap();
        let rendered = doc.to_string();
        assert!(rendered.contains("[providers.deepseek]"), "{rendered}");
        assert!(rendered.contains("MY_DEEPSEEK_KEY"), "{rendered}");
        assert!(rendered.contains("base_url"), "{rendered}");
        assert!(
            rendered.contains("api_protocol = \"openai-completions\""),
            "协议必写：{rendered}"
        );
        rendered.parse::<DocumentMut>().unwrap();
    }

    // ===== patch_provider =====

    #[test]
    fn patch_provider_missing_target_fails_not_found() {
        let mut doc = DocumentMut::new();
        assert!(matches!(
            patch_provider(&mut doc, "nope", &sample_spec()),
            Err(ProviderAdminError::NotFound(_))
        ));
    }

    /// patch 的完整期望状态语义：name / base_url / 指针 / models 整表替换
    #[test]
    fn patch_provider_applies_full_expected_state() {
        let mut doc: DocumentMut = "[providers.p]\nname = \"old\"\nunknown_key = \"keep\"\n\
             [providers.p.models.old-m]\nname = \"old-m\"\nlimit = { context = 8 }\n"
            .parse()
            .unwrap();
        patch_provider(&mut doc, "p", &sample_spec()).unwrap();

        let table = doc
            .as_table()
            .get("providers")
            .unwrap()
            .get("p")
            .unwrap()
            .as_table()
            .unwrap();
        assert_eq!(table.get("name").unwrap().as_str(), Some("DeepSeek"));
        assert_eq!(
            table.get("api_protocol").unwrap().as_str(),
            Some("openai-completions"),
            "协议随载荷覆盖"
        );
        assert_eq!(
            table.get("unknown_key").unwrap().as_str(),
            Some("keep"),
            "段内用户手写的未知字段不动"
        );
        // 指针落单值
        assert_eq!(
            table
                .get("api_key_env_vars")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // models 整表替换：old-m 消失、载荷 id 落盘
        let models = table.get("models").unwrap().as_table().unwrap();
        assert!(models.get("old-m").is_none());
        assert!(models.contains_key("deepseek-v4-flash"));
    }

    /// base_url = None 清除 options；指针 None 移除键；空 models 移除 models 键
    #[test]
    fn patch_provider_none_values_clear_keys() {
        let mut doc: DocumentMut = "[providers.p]\nname = \"old\"\n\
             options = { base_url = \"https://x\" }\napi_key_env_vars = [\"K\"]\n"
            .parse()
            .unwrap();
        let spec = ProviderSpec {
            name: "P".to_string(),
            api_protocol: fuyao_api::ApiProtocol::OpenaiCompletions,
            base_url: None,
            api_key_env_var: None,
            api_key: None,
            models: Vec::new(),
        };
        patch_provider(&mut doc, "p", &spec).unwrap();
        let table = doc
            .as_table()
            .get("providers")
            .unwrap()
            .get("p")
            .unwrap()
            .as_table()
            .unwrap();
        assert!(table.get("options").is_none(), "空 options 整体移除");
        assert!(table.get("api_key_env_vars").is_none());
        assert!(table.get("models").is_none(), "空载荷移除 models 键");
    }

    // ===== remove_provider =====

    #[test]
    fn remove_provider_cascades_and_drops_empty_header() {
        let mut doc: DocumentMut = "[providers.p]\nname = \"x\"\n".parse().unwrap();
        remove_provider(&mut doc, "p").unwrap();
        assert!(!doc.to_string().contains("providers"), "删空后无空表头");
    }

    #[test]
    fn remove_provider_keeps_other_sections_verbatim() {
        let mut doc: DocumentMut =
            "[providers.a]\nname = \"a\"\n# 手写注释\n[providers.b]\nname = \"b\"\n"
                .parse()
                .unwrap();
        remove_provider(&mut doc, "a").unwrap();
        let rendered = doc.to_string();
        // 注释归属 [providers.b] 表头装饰，删除 a 不动 b 及其注释
        assert!(rendered.contains("# 手写注释"), "其他段的手写注释保留");
        assert!(rendered.contains("[providers.b]"), "其他供应商不动");
        assert!(!rendered.contains("[providers.a]"), "目标段已移除");
    }

    #[test]
    fn remove_provider_missing_target_fails_not_found() {
        let mut doc = DocumentMut::new();
        assert!(matches!(
            remove_provider(&mut doc, "nope"),
            Err(ProviderAdminError::NotFound(_))
        ));
    }

    // ===== 段导航 =====

    #[test]
    fn providers_table_mut_creates_implicit_when_absent() {
        let mut doc = DocumentMut::new();
        let table = providers_table_mut(&mut doc).unwrap();
        table.insert("demo", Item::Table(Table::new()));
        // 隐式父表：渲染为 [providers.demo]，无空 [providers] 表头
        assert_eq!(doc.to_string(), "[providers.demo]\n");
    }

    #[test]
    fn providers_table_mut_rejects_non_table_section() {
        let mut doc: DocumentMut = "providers = \"oops\"\n".parse().unwrap();
        assert!(matches!(
            providers_table_mut(&mut doc),
            Err(ProviderAdminError::InvalidSection(_))
        ));
    }

    #[test]
    fn provider_table_mut_returns_none_when_absent() {
        let mut doc: DocumentMut = "[providers.other]\nname = \"x\"\n".parse().unwrap();
        let providers = providers_table_mut(&mut doc).unwrap();
        assert!(provider_table_mut(providers, "missing").unwrap().is_none());
    }

    #[test]
    fn provider_table_mut_rejects_dotted_form() {
        let mut doc: DocumentMut = "providers.dotted.name = \"x\"\n".parse().unwrap();
        let providers = providers_table_mut(&mut doc).unwrap();
        assert!(matches!(
            provider_table_mut(providers, "dotted"),
            Err(ProviderAdminError::InvalidSection(_))
        ));
    }
}
