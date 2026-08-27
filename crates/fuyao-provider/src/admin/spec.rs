//! 供应商写回载荷类型与入参校验
//!
//! 校验是 fail-loud 口径：写入前拦下一切会让落盘结果不可读回 / 语义不一致的
//! 载荷，此时文件未动。

use std::collections::HashSet;
use std::fmt;

use fuyao_api::{ApiProtocol, Model};

use super::error::ProviderAdminError;

/// 供应商写回载荷的单个模型条目（模型 id + 全量字段）
pub struct ProviderModelSpec {
    /// 模型 id（`[providers.<id>.models.<mid>]` 的键）
    pub id: String,
    /// 模型全量字段
    pub model: Model,
}

/// 供应商写回载荷（create / update 共用，完整期望状态）
///
/// 模型内嵌为全量列表：create 一次落齐全部模型；update 时 `models` 子表整表
/// 替换为载荷内容（未携带的模型消失，模型 id 可随替换变更）。
///
/// `api_key_env_var` 是 .env 变量名指针，用户自设、与供应商 id 解耦——toml
/// 落单值 `api_key_env_vars = [变量名]`（所见即所得），`None` 则移除指针键；
/// id 变更或删旧建新时指针不变、.env 行不动，密钥天然保持。
///
/// `api_key` 为 `Some` 时把明文 upsert 进 .env 的该变量（同名覆盖、异名新
/// 加）；`None` 不动 .env。.env 只增改不删除，变量与值均归用户持有。
///
/// `base_url` / `name` 为完整期望状态：update 时 `base_url = None` 表示清除
/// 该项（调用方提交表单的完整状态，而非增量）。
///
/// `api_protocol` 为必有字段（非 Option）：协议必填无缺省，清除即配置非法，
/// create / update 载荷恒携带完整值。
pub struct ProviderSpec {
    /// 供应商显示名（必填）
    pub name: String,
    /// API 协议（wire 方言三选一，必有字段，恒随载荷落盘）
    pub api_protocol: ApiProtocol,
    /// 自定义 base URL（None = 不配置 / 清除）
    pub base_url: Option<String>,
    /// API Key 环境变量名（None = 不配置指针；明文提供时必填）
    pub api_key_env_var: Option<String>,
    /// API Key 明文（None = 不动 .env；Some = upsert 进上述变量）
    pub api_key: Option<String>,
    /// 全量模型列表（create 落盘 / update 整表替换）
    pub models: Vec<ProviderModelSpec>,
}

impl ProviderSpec {
    /// 转存储层数据形态（api_key 明文不随行——.env 写入由管理器另行编排）
    pub fn into_data(self) -> ProviderSpecData {
        ProviderSpecData {
            name: self.name,
            api_protocol: self.api_protocol,
            base_url: self.base_url,
            api_key_env_var: self.api_key_env_var,
            models: self
                .models
                .into_iter()
                .map(|entry| (entry.id, entry.model))
                .collect(),
        }
    }
}

impl fmt::Debug for ProviderSpec {
    /// api_key 不出现在 Debug 输出中（敏感信息红线），以占位符标注有无
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderSpec")
            .field("name", &self.name)
            .field("api_protocol", &self.api_protocol)
            .field("base_url", &self.base_url)
            .field("api_key_env_var", &self.api_key_env_var)
            .field(
                "api_key",
                &if self.api_key.is_some() {
                    "<已隐藏>"
                } else {
                    "<未提供>"
                },
            )
            .field("models", &self.models.len())
            .finish()
    }
}

/// 供应商写回载荷的字段集（存储层视角的纯数据）
pub struct ProviderSpecData {
    /// 供应商显示名
    pub name: String,
    /// API 协议（wire 方言三选一，落盘必写）
    pub api_protocol: ApiProtocol,
    /// 自定义 base URL
    pub base_url: Option<String>,
    /// API Key 环境变量名（None = 不落指针键）
    pub api_key_env_var: Option<String>,
    /// 全量模型列表（模型 id + 模型字段的二元组）
    pub models: Vec<(String, Model)>,
}

// ── 入参校验（fail-loud）──────────────────────────────────────

/// 供应商 id 校验：非空，字符集限 `[A-Za-z0-9_-]`
///
/// 该字符集同时满足三个消费者：TOML 裸键（无需引号转义）、环境变量名惯例
/// （大写后作 `.env` 变量名主体）、注册缓存的小写化 key。不满足即拒绝，
/// 不做自动改写（id 是身份锚点，静默变形会造成写盘键与调用方预期不一致）。
pub fn validate_provider_id(provider_id: &str) -> Result<(), ProviderAdminError> {
    if provider_id.is_empty() {
        return Err(ProviderAdminError::Invalid(
            "供应商 id 不能为空".to_string(),
        ));
    }
    if !provider_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(ProviderAdminError::Invalid(format!(
            "供应商 id 含非法字符（{provider_id}）：仅允许字母、数字、下划线、连字符"
        )));
    }
    Ok(())
}

/// 模型 id 校验：非空，禁 `/`（破坏 `provider/model` 复合 id 拆分）、引号、
/// 控制字符与 TOML 键上下文的保留符号
pub fn validate_model_id(model_id: &str) -> Result<(), ProviderAdminError> {
    if model_id.trim().is_empty() {
        return Err(ProviderAdminError::Invalid("模型 id 不能为空".to_string()));
    }
    let bad: &[char] = &['/', '"', '\'', '#', '=', '[', ']'];
    if model_id.chars().any(|c| c.is_control() || bad.contains(&c)) {
        return Err(ProviderAdminError::Invalid(format!(
            "模型 id 含非法字符（{model_id}）：禁止 / 引号 # = [ ] 与控制字符\
             （点号等其余字符可用，落盘时自动加引号键）"
        )));
    }
    if model_id != model_id.trim() {
        return Err(ProviderAdminError::Invalid(format!(
            "模型 id 首尾含空白（{model_id}）：请去除后重试"
        )));
    }
    Ok(())
}

/// 供应商显示名校验：非空（name 必填，空值拒写）
pub fn validate_provider_name(name: &str) -> Result<(), ProviderAdminError> {
    if name.trim().is_empty() {
        return Err(ProviderAdminError::Invalid(
            "供应商 name 不能为空（显示名，如 name = \"DeepSeek\"）".to_string(),
        ));
    }
    Ok(())
}

/// API Key 环境变量名校验：非空，字符集 `[A-Za-z0-9_]` 且不以数字开头
///
/// 用户自设的变量名与供应商 id 解耦（id 变更不影响变量名）。.env / dotenvy
/// 惯例形态之外的名字无法被 `api_key_env_vars` 指针解析链可靠命中，写入前
/// 拦下。
pub fn validate_env_var_name(env_var: &str) -> Result<(), ProviderAdminError> {
    if env_var.is_empty() {
        return Err(ProviderAdminError::Invalid(
            "API Key 环境变量名不能为空（如 MY_DEEPSEEK_KEY）".to_string(),
        ));
    }
    let invalid = || {
        ProviderAdminError::Invalid(format!(
            "API Key 环境变量名非法（{env_var}）：仅允许字母、数字、下划线，且不能以数字开头"
        ))
    };
    let mut chars = env_var.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return Err(invalid()),
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(invalid());
    }
    Ok(())
}

/// 写回载荷整体校验（fail-loud，此时文件未动）
///
/// name 非空；变量名字符集合法；明文必须伴随变量名（明文无 .env 落点即配置
/// 错误）；每个模型 id / 必填字段合法且载荷内 id 无重复（models 整表替换以
/// id 为键，重复条目会静默互相覆盖，写入前拦下）。
pub fn validate_spec(spec: &ProviderSpec) -> Result<(), ProviderAdminError> {
    validate_provider_name(&spec.name)?;
    if let Some(env_var) = &spec.api_key_env_var {
        validate_env_var_name(env_var)?;
    }
    if let Some(api_key) = &spec.api_key {
        if spec.api_key_env_var.is_none() {
            return Err(ProviderAdminError::Invalid(
                "api_key 明文必须伴随 api_key_env_var 变量名（明文需有 .env 落点）".to_string(),
            ));
        }
        validate_api_key(api_key)?;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for entry in &spec.models {
        validate_model_id(&entry.id)?;
        validate_model(&entry.model)?;
        if !seen.insert(entry.id.as_str()) {
            return Err(ProviderAdminError::Invalid(format!(
                "模型 id 重复（{}）：载荷内每个模型 id 只能出现一次",
                entry.id
            )));
        }
    }
    Ok(())
}

/// 模型必填校验：name 非空 + `limit.context` 正整数
///
/// `limit.context` 缺失 / 为 0 会在运行期导致压缩触发公式失效（usable=0、
/// 阈值恒真、每轮必压缩），写入期即拦下，保证写盘结果可被配置加载无损读回。
pub fn validate_model(model: &Model) -> Result<(), ProviderAdminError> {
    if model.name.trim().is_empty() {
        return Err(ProviderAdminError::Invalid(
            "模型 name 不能为空（显示名，如 name = \"deepseek-v4-flash\"）".to_string(),
        ));
    }
    if model.limit.context == 0 {
        return Err(ProviderAdminError::Invalid(
            "模型 limit.context 必须为正整数（上下文窗口 tokens，如 limit = { context = 128000 }）"
                .to_string(),
        ));
    }
    Ok(())
}

/// api_key 明文校验：不含引号 / 控制字符（.env 单行格式的安全边界）
pub fn validate_api_key(api_key: &str) -> Result<(), ProviderAdminError> {
    if api_key
        .chars()
        .any(|c| c == '\'' || c == '"' || c.is_control())
    {
        return Err(ProviderAdminError::Invalid(
            "API Key 含引号或控制字符，无法写入 .env（请检查输入）".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小 Model 配置（元信息取默认值）
    fn test_model(name: &str) -> fuyao_api::Model {
        fuyao_api::Model {
            name: name.to_string(),
            cost: Default::default(),
            limit: Default::default(),
            reasoning_efforts: vec![],
            modalities: Default::default(),
        }
    }

    #[test]
    fn validate_provider_id_accepts_bare_key_charset() {
        assert!(validate_provider_id("deepseek").is_ok());
        assert!(validate_provider_id("My-Vendor_9").is_ok());
    }

    #[test]
    fn validate_provider_id_rejects_empty_and_special_chars() {
        assert!(validate_provider_id("").is_err());
        assert!(validate_provider_id("a.b").is_err());
        assert!(validate_provider_id("a b").is_err());
        assert!(validate_provider_id("中文").is_err());
    }

    #[test]
    fn validate_model_id_allows_dots_but_rejects_slash_and_quotes() {
        assert!(validate_model_id("qwen3.6-plus").is_ok());
        assert!(validate_model_id("deepseek-v4-flash").is_ok());
        assert!(validate_model_id("a/b").is_err());
        assert!(validate_model_id("a\"b").is_err());
        assert!(validate_model_id(" a").is_err());
        assert!(validate_model_id("  ").is_err());
    }

    #[test]
    fn validate_model_enforces_name_and_positive_context() {
        let mut model = test_model("x");
        model.limit.context = 128000;
        assert!(validate_model(&model).is_ok());

        model.name = "  ".to_string();
        assert!(validate_model(&model).is_err());

        model.name = "x".to_string();
        model.limit.context = 0;
        assert!(validate_model(&model).is_err());
    }

    #[test]
    fn validate_env_var_name_accepts_env_convention() {
        assert!(validate_env_var_name("MY_DEEPSEEK_KEY").is_ok());
        assert!(validate_env_var_name("_internal").is_ok());
    }

    #[test]
    fn validate_env_var_name_rejects_empty_leading_digit_and_special_chars() {
        assert!(validate_env_var_name("").is_err());
        assert!(validate_env_var_name("1ABC").is_err());
        assert!(validate_env_var_name("A-B").is_err());
        assert!(validate_env_var_name("A.B").is_err());
        assert!(validate_env_var_name("变量").is_err());
    }

    /// 明文无变量名（明文缺 .env 落点）与载荷内重复模型 id 都在写入前拦下
    #[test]
    fn validate_spec_rejects_homeless_api_key_and_duplicate_model_ids() {
        let base = |api_key_env_var: Option<&str>, id: &str| ProviderSpec {
            name: "DeepSeek".to_string(),
            api_protocol: ApiProtocol::OpenaiCompletions,
            base_url: None,
            api_key_env_var: api_key_env_var.map(str::to_string),
            api_key: Some("sk-plain".to_string()),
            models: vec![ProviderModelSpec {
                id: id.to_string(),
                model: test_model("m"),
            }],
        };

        let homeless = base(None, "m");
        assert!(
            matches!(
                validate_spec(&homeless),
                Err(ProviderAdminError::Invalid(_))
            ),
            "明文必须伴随变量名"
        );

        let duplicated = ProviderSpec {
            api_key: None,
            models: vec![
                ProviderModelSpec {
                    id: "m".to_string(),
                    model: test_model("m"),
                },
                ProviderModelSpec {
                    id: "m".to_string(),
                    model: test_model("m"),
                },
            ],
            ..base(Some("K"), "other")
        };
        assert!(
            matches!(
                validate_spec(&duplicated),
                Err(ProviderAdminError::Invalid(_))
            ),
            "载荷内重复模型 id 拦下"
        );
    }

    // ===== ProviderSpec 的 Debug 屏蔽 =====

    #[test]
    fn provider_spec_debug_masks_api_key() {
        let spec = ProviderSpec {
            name: "DeepSeek".to_string(),
            api_protocol: ApiProtocol::OpenaiCompletions,
            base_url: None,
            api_key_env_var: Some("MY_DEEPSEEK_KEY".to_string()),
            api_key: Some("sk-secret".to_string()),
            models: vec![ProviderModelSpec {
                id: "deepseek-v4-flash".to_string(),
                model: test_model("deepseek-v4-flash"),
            }],
        };
        let debug = format!("{spec:?}");
        assert!(
            !debug.contains("sk-secret"),
            "Debug 输出不得泄露 api_key：{debug}"
        );
        assert!(debug.contains("<已隐藏>"), "应以占位符标注：{debug}");
    }
}
