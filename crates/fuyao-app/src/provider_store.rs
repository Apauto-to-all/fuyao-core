//! 供应商管理写回落存储：global 层两文件的增量修改原语
//!
//! 两个文件都落在 global 层（`~/.fuyao/`）：
//! - `fuyao.toml`：用 toml_edit 做段级增量 patch——只修改目标 `[providers.<id>]`
//!   子树内的键，子树之外的注释、未知字段、手写格式逐字保留（toml_edit 的
//!   文档对象承载原文样式信息，未触碰的键渲染时原样输出）。
//! - `.env`：api_key 明文的隔离存放地，做单行级读-改-写——只动目标变量那
//!   一行，用户手写的其他变量与注释不动。
//!
//! 本模块只提供存储原语（文件读写 + 行/段级变更 + 领域对象到表的序列化），
//! 管理策略（校验、冲突信号、指针一致性）由上层 `provider_manager` 决定。

use std::path::{Path, PathBuf};

use fuyao_api::{InputModality, Model, ModelModalities, OutputModality, Provider};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value, value};

use crate::provider_manager::ProviderAdminError;

// ==================== 文件定位与读写 ====================

/// global 层写回落存储句柄：定位 fuyao.toml 与 .env 两个落点
pub(crate) struct GlobalStore {
    /// global 层配置文件路径（`{fuyao_home}/fuyao.toml`）
    toml_path: PathBuf,
    /// global 层环境变量文件路径（`{fuyao_home}/.env`）
    env_path: PathBuf,
}

impl GlobalStore {
    /// 以 fuyao_home 为基准构造存储句柄
    pub(crate) fn new(fuyao_home: &Path) -> Self {
        Self {
            toml_path: fuyao_home.join("fuyao.toml"),
            env_path: fuyao_home.join(".env"),
        }
    }

    /// 读 global 层 fuyao.toml 为可编辑文档
    ///
    /// 文件不存在返回空文档（首次写回时由 [`GlobalStore::write_toml`] 新建）；
    /// 内容非法 TOML 返回 [`ProviderAdminError::TomlParse`]（带解析错误信息）。
    pub(crate) fn read_toml(&self) -> Result<DocumentMut, ProviderAdminError> {
        match std::fs::read_to_string(&self.toml_path) {
            Ok(content) => content
                .parse::<DocumentMut>()
                .map_err(|e| ProviderAdminError::TomlParse(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
            Err(e) => Err(ProviderAdminError::Io(e.to_string())),
        }
    }

    /// 把文档落盘（父目录不存在则创建；toml_edit 渲染保留未触碰部分的原文样式）
    pub(crate) fn write_toml(&self, doc: &DocumentMut) -> Result<(), ProviderAdminError> {
        write_file(&self.toml_path, &doc.to_string())
    }

    /// 读 global 层 .env 全文（文件不存在返回空串）
    pub(crate) fn read_env(&self) -> Result<String, ProviderAdminError> {
        match std::fs::read_to_string(&self.env_path) {
            Ok(content) => Ok(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(ProviderAdminError::Io(e.to_string())),
        }
    }

    /// 把 .env 内容落盘（父目录不存在则创建）
    pub(crate) fn write_env(&self, content: &str) -> Result<(), ProviderAdminError> {
        write_file(&self.env_path, content)
    }
}

/// 写文件：父目录不存在则逐级创建，避免首次写回因目录缺失失败
fn write_file(path: &Path, content: &str) -> Result<(), ProviderAdminError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ProviderAdminError::Io(e.to_string()))?;
    }
    std::fs::write(path, content).map_err(|e| ProviderAdminError::Io(e.to_string()))
}

// ==================== fuyao.toml 段级导航 ====================

/// 取（不存在则创建）顶层 providers 表
///
/// 新建的表标记为隐式（`set_implicit(true)`）：渲染为 `[providers.<id>]`
/// 层级式表头，不产生多余的空 `[providers]` 表头。段存在但值不是 table
/// （标量 / 数组 / 数组表）返回 [`ProviderAdminError::InvalidSection`]。
pub(crate) fn providers_table_mut(doc: &mut DocumentMut) -> Result<&mut Table, ProviderAdminError> {
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
pub(crate) fn provider_table_mut<'a>(
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

/// 取（不存在则创建）`[providers.<id>.models]` 子表
///
/// 新建为隐式表：渲染为 `[providers.<id>.models.<mid>]` 层级式表头。
/// 段存在但不是 table 返回 [`ProviderAdminError::InvalidSection`]。
pub(crate) fn models_table_mut<'a>(
    provider_table: &'a mut Table,
    provider_id: &str,
) -> Result<&'a mut Table, ProviderAdminError> {
    if provider_table.get("models").is_none() {
        let mut table = Table::new();
        table.set_implicit(true);
        provider_table.insert("models", Item::Table(table));
    }
    provider_table
        .get_mut("models")
        .and_then(|item| item.as_table_mut())
        .ok_or_else(|| {
            ProviderAdminError::InvalidSection(format!(
                "providers.{provider_id}.models 段不是 table：\
                 必须以 [providers.{provider_id}.models.<mid>] 表形式声明模型"
            ))
        })
}

// ==================== 领域对象 → TOML 表 ====================

/// 供应商段写入：新建完整的 `[providers.<id>]` 表体
///
/// api_key 明文不进 toml（密钥隔离红线）：段内只写 `api_key_env_vars` 指针，
/// 指向 .env 中按约定生成的变量（由调用方负责写入 .env）。
pub(crate) fn provider_to_table(spec_provider: &ProviderSpecData, env_var: &str) -> Table {
    let mut table = Table::new();
    table.insert("name", value(spec_provider.name.clone()));
    if let Some(base_url) = &spec_provider.base_url {
        let mut options = Table::new();
        options.insert("base_url", value(base_url.clone()));
        table.insert("options", Item::Table(options));
    }
    let mut vars = Array::new();
    vars.push(Value::from(env_var));
    table.insert("api_key_env_vars", toml_edit::value(vars));
    table
}

/// 供应商写回载荷的字段集（存储层视角的纯数据，见 `provider_manager::ProviderSpec`）
pub(crate) struct ProviderSpecData {
    /// 供应商显示名
    pub(crate) name: String,
    /// 自定义 base URL
    pub(crate) base_url: Option<String>,
}

/// 模型段写入：构造 `[providers.<id>.models.<mid>]` 表体
///
/// 渲染形态取手写配置的常见写法：`limit` / 仅标量价格的 `cost` / 非默认 `modalities`
/// 用内联表，含梯度的 `cost` 用表头 + `[[...cost.tiers]]`（内联表无法跨行容纳
/// 数组表）。缺省字段（无价格的 cost、默认 text 模态、空档位列表）不写——
/// 读回路径按同一默认值补齐，写盘前后语义一致。
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

/// 读回现有 Provider 配置（`api_key_env_vars` / `options`），供写回方保持指针一致
///
/// 从 toml_edit 表直接读取（不经过整份配置加载），仅提取写回方关心的两个键；
/// options 兼容表头（`Item::Table`）与内联（`Item::Value` 的 InlineTable）两种
/// 手写形态。
pub(crate) fn read_provider_pointers(table: &Table) -> Provider {
    let mut provider = Provider::default();
    if let Some(arr) = table.get("api_key_env_vars").and_then(|i| i.as_array()) {
        provider.api_key_env_vars = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    match table.get("options") {
        Some(Item::Table(options)) => {
            provider.options.base_url = options
                .get("base_url")
                .and_then(|v| v.as_str())
                .map(String::from);
            provider.options.api_key = options
                .get("api_key")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        Some(Item::Value(value)) => {
            if let Some(inline) = value.as_inline_table() {
                provider.options.base_url = inline
                    .get("base_url")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                provider.options.api_key = inline
                    .get("api_key")
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }
        }
        _ => {}
    }
    provider
}

// ==================== .env 单行级增量 ====================

/// .env 行级写入失败形态
#[derive(Debug)]
pub(crate) enum EnvWriteError {
    /// 目标变量已有不同的值且未确认覆盖（携带变量名；不携带已有值——密钥红线）
    Conflict(String),
    /// 目标行是跨行引号值（多行值），单行级改写会破坏文件结构
    MultiLine(String),
}

/// 按约定生成供应商的 api_key 环境变量名：`{供应商 id 大写}_API_KEY`
pub(crate) fn generated_env_var_name(provider_id: &str) -> String {
    format!("{}_API_KEY", provider_id.to_ascii_uppercase())
}

/// 把 api_key 值格式化为 `.env` 单行 `VAR=值`
///
/// dotenvy 语义下的安全格式：含空白 / `#` / `$` 的值用单引号包裹（单引号内
/// dotenvy 不做变量替换，保字面值）；普通值裸写。空值写成 `VAR=""`。
/// 值含引号或控制字符无法用单行安全表达，返回 [`ProviderAdminError::Invalid`]。
pub(crate) fn format_env_line(var: &str, value: &str) -> Result<String, ProviderAdminError> {
    if value
        .chars()
        .any(|c| c == '\'' || c == '"' || c.is_control())
    {
        return Err(ProviderAdminError::Invalid(
            "API Key 含引号或控制字符，无法写入 .env（请检查输入）".to_string(),
        ));
    }
    if value.is_empty() {
        return Ok(format!("{var}=\"\""));
    }
    let needs_quote = value
        .chars()
        .any(|c| c.is_whitespace() || c == '#' || c == '$');
    if needs_quote {
        Ok(format!("{var}='{value}'"))
    } else {
        Ok(format!("{var}={value}"))
    }
}

/// 解析一行 .env 的变量名（不含值）
///
/// 兼容 dotenvy 语法：行首空白、`export ` 前缀、`=` 两侧空白；注释行与
/// 无 `=` 的行返回 `None`。
fn env_line_var(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let after_export = match trimmed.strip_prefix("export") {
        Some(rest) if rest.starts_with([' ', '\t']) => rest.trim_start(),
        _ => trimmed,
    };
    let eq = after_export.find('=')?;
    let name = after_export[..eq].trim_end();
    if name.is_empty() || name.contains([' ', '\t']) {
        return None;
    }
    Some(name)
}

/// 判断一行的值是否为未闭合的跨行引号值
///
/// dotenvy 会把未闭合引号后的行接续为值的一部分；单行级改写此类行会破坏
/// 文件结构，调用方须拒绝。
fn is_multiline_value(line: &str) -> bool {
    let Some(eq) = line.find('=') else {
        return false;
    };
    let value = line[eq + 1..].trim();
    let Some(first) = value.chars().next() else {
        return false;
    };
    if first == '\'' || first == '"' {
        // 引号开头且行内无第二个同类引号 → 未闭合，值跨行
        value[1..].find(first).is_none()
    } else {
        false
    }
}

/// 单行级写入：目标变量行存在则替换整行，不存在则追加到文件末尾
///
/// - 已有行与目标行相同（忽略行尾 `\r` 的 CRLF 差异）→ 幂等直返原内容
/// - 已有行不同且未确认覆盖 → [`EnvWriteError::Conflict`]
/// - 已有行是跨行引号值 → [`EnvWriteError::MultiLine`]
/// - 追加时保证与既有内容之间恰好一个换行分隔
pub(crate) fn upsert_env_line(
    content: &str,
    var: &str,
    new_line: &str,
    overwrite: bool,
) -> Result<String, EnvWriteError> {
    let lines: Vec<&str> = content.split('\n').collect();
    let target = lines.iter().position(|l| env_line_var(l) == Some(var));
    match target {
        Some(idx) => {
            let existing = lines[idx].trim_end_matches('\r');
            if existing == new_line {
                // 幂等：目标行已是期望内容，原样返回（不改写文件，也不动行尾风格）
                return Ok(content.to_string());
            }
            if !overwrite {
                return Err(EnvWriteError::Conflict(var.to_string()));
            }
            if is_multiline_value(lines[idx]) {
                return Err(EnvWriteError::MultiLine(var.to_string()));
            }
            let mut lines: Vec<String> = lines.into_iter().map(String::from).collect();
            lines[idx] = new_line.to_string();
            Ok(lines.join("\n"))
        }
        None => {
            let mut out = content.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(new_line);
            out.push('\n');
            Ok(out)
        }
    }
}

/// 单行级删除：移除目标变量那一行；变量不存在时原样返回
///
/// 目标行是跨行引号值时返回 [`EnvWriteError::MultiLine`]（删除首行会留下
/// 残续行，破坏文件结构）。
pub(crate) fn remove_env_line(content: &str, var: &str) -> Result<String, EnvWriteError> {
    let lines: Vec<&str> = content.split('\n').collect();
    let target = lines.iter().position(|l| env_line_var(l) == Some(var));
    let Some(idx) = target else {
        return Ok(content.to_string());
    };
    if is_multiline_value(lines[idx]) {
        return Err(EnvWriteError::MultiLine(var.to_string()));
    }
    let mut lines: Vec<String> = lines.into_iter().map(String::from).collect();
    lines.remove(idx);
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::{ModelCost, ModelLimit};

    // ===== 变量名生成 =====

    #[test]
    fn generated_env_var_name_uppercases_provider_id() {
        assert_eq!(generated_env_var_name("deepseek"), "DEEPSEEK_API_KEY");
        assert_eq!(generated_env_var_name("My-Vendor_9"), "MY-VENDOR_9_API_KEY");
    }

    // ===== 行格式化 =====

    #[test]
    fn format_env_line_plain_value_unquoted() {
        assert_eq!(
            format_env_line("DEEPSEEK_API_KEY", "sk-abc123").unwrap(),
            "DEEPSEEK_API_KEY=sk-abc123"
        );
    }

    #[test]
    fn format_env_line_special_value_single_quoted() {
        // 含 #、$、空白的值用单引号包裹（dotenvy 单引号内不做替换）
        assert_eq!(format_env_line("K", "a b#c$d").unwrap(), "K='a b#c$d'");
    }

    #[test]
    fn format_env_line_empty_value_writes_empty_quotes() {
        assert_eq!(format_env_line("K", "").unwrap(), "K=\"\"");
    }

    #[test]
    fn format_env_line_rejects_quotes_and_controls() {
        assert!(format_env_line("K", "a'b").is_err());
        assert!(format_env_line("K", "a\"b").is_err());
        assert!(format_env_line("K", "a\nb").is_err());
    }

    // ===== 行变量名解析 =====

    #[test]
    fn env_line_var_parses_plain_export_and_spaces() {
        assert_eq!(env_line_var("K=v"), Some("K"));
        assert_eq!(env_line_var("  K = v"), Some("K"));
        assert_eq!(env_line_var("export K=v"), Some("K"));
        assert_eq!(env_line_var("\texport  K = v"), Some("K"));
    }

    #[test]
    fn env_line_var_ignores_comments_and_bare_lines() {
        assert_eq!(env_line_var("# K=v"), None);
        assert_eq!(env_line_var("not a pair"), None);
        assert_eq!(env_line_var(""), None);
    }

    #[test]
    fn env_line_var_matches_exact_name_only() {
        // 前缀相似的变量名不得误命中（DEEPSEEK_API_KEY_EXTRA ≠ DEEPSEEK_API_KEY）
        assert_eq!(
            env_line_var("DEEPSEEK_API_KEY_EXTRA=v"),
            Some("DEEPSEEK_API_KEY_EXTRA")
        );
    }

    // ===== 跨行值识别 =====

    #[test]
    fn multiline_value_detected_for_unclosed_quotes() {
        assert!(is_multiline_value("K='abc"));
        assert!(is_multiline_value("K=\"abc"));
    }

    #[test]
    fn single_line_quoted_values_not_flagged() {
        assert!(!is_multiline_value("K='abc'"));
        assert!(!is_multiline_value("K=\"a b\""));
        assert!(!is_multiline_value("K=abc"));
    }

    // ===== 单行写入 =====

    #[test]
    fn upsert_appends_when_var_absent() {
        let content = "# 手写注释\nOTHER_VAR=keep\n";
        let out = upsert_env_line(content, "K", "K=v1", false).unwrap();
        assert_eq!(out, "# 手写注释\nOTHER_VAR=keep\nK=v1\n");
    }

    #[test]
    fn upsert_append_adds_separator_when_missing_trailing_newline() {
        let out = upsert_env_line("A=1", "K", "K=v", false).unwrap();
        assert_eq!(out, "A=1\nK=v\n");
    }

    #[test]
    fn upsert_replace_keeps_other_lines_verbatim() {
        let content = "# 注释\nA=1\nK=old\nB=2\n";
        let out = upsert_env_line(content, "K", "K=new", true).unwrap();
        assert_eq!(out, "# 注释\nA=1\nK=new\nB=2\n");
    }

    #[test]
    fn upsert_conflict_when_different_and_no_overwrite() {
        let err = upsert_env_line("K=handwritten\n", "K", "K=new", false).unwrap_err();
        assert!(matches!(err, EnvWriteError::Conflict(var) if var == "K"));
    }

    #[test]
    fn upsert_idempotent_when_line_identical() {
        let content = "K=v\n";
        let out = upsert_env_line(content, "K", "K=v", false).unwrap();
        assert_eq!(out, content);
    }

    #[test]
    fn upsert_treats_crlf_existing_line_as_same_content() {
        // CRLF 文件里已有行带 \r：与 LF 新行语义相同，幂等不改写（保住 CRLF 风格）
        let content = "K=v\r\n";
        let out = upsert_env_line(content, "K", "K=v", false).unwrap();
        assert_eq!(out, content);
    }

    #[test]
    fn upsert_rejects_multiline_existing_value() {
        let err = upsert_env_line("K='start\ncontinues'\n", "K", "K=v", true).unwrap_err();
        assert!(matches!(err, EnvWriteError::MultiLine(_)));
    }

    // ===== 单行删除 =====

    #[test]
    fn remove_deletes_target_line_only() {
        let content = "# 注释\nA=1\nK=drop\nB=2\n";
        let out = remove_env_line(content, "K").unwrap();
        assert_eq!(out, "# 注释\nA=1\nB=2\n");
    }

    #[test]
    fn remove_absent_var_returns_unchanged() {
        let content = "A=1\n";
        let out = remove_env_line(content, "K").unwrap();
        assert_eq!(out, content);
    }

    // ===== providers 表导航 =====

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

    // ===== Model → 表序列化 =====

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
        let mut doc = DocumentMut::new();
        let mut providers = Table::new();
        let mut provider = Table::new();
        let mut models = Table::new();
        models.insert("m", Item::Table(model_to_table(&model)));
        provider.insert("models", Item::Table(models));
        providers.insert("p", Item::Table(provider));
        doc.insert("providers", Item::Table(providers));
        let rendered = doc.to_string();
        assert!(rendered.contains("[[providers.p.models.m.cost.tiers]]"));
        rendered.parse::<DocumentMut>().unwrap();
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

    // ===== 指针读取 =====

    #[test]
    fn read_provider_pointers_extracts_env_vars_and_options() {
        let mut table = Table::new();
        let mut vars = Array::new();
        vars.push(Value::from("A_API_KEY"));
        vars.push(Value::from("B_API_KEY"));
        table.insert("api_key_env_vars", toml_edit::value(vars));
        let mut options = Table::new();
        options.insert("base_url", value("https://example.com"));
        options.insert("api_key", value("plaintext"));
        table.insert("options", Item::Table(options));

        let provider = read_provider_pointers(&table);
        assert_eq!(provider.api_key_env_vars, vec!["A_API_KEY", "B_API_KEY"]);
        assert_eq!(
            provider.options.base_url.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(provider.options.api_key.as_deref(), Some("plaintext"));
    }
}
