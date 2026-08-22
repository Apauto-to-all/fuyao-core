//! `.env` 单行级增量原语
//!
//! api_key 明文的隔离存放地（global 层 `.env`），只动目标变量那一行——用户
//! 手写的其他变量与注释不动。dotenvy 语义：含空白 / `#` / `$` 的值用单引号
//! 包裹（单引号内不做变量替换，保字面值）；普通值裸写；空值写成 `VAR=""`。

use super::error::ProviderAdminError;

/// 把 api_key 值格式化为 `.env` 单行 `VAR=值`
///
/// 值含引号或控制字符无法用单行安全表达，返回 [`ProviderAdminError::Invalid`]。
pub fn format_env_line(var: &str, value: &str) -> Result<String, ProviderAdminError> {
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

/// 单行级写入：目标变量行存在则整行覆盖，不存在则追加到文件末尾
///
/// - 已有行与目标行相同（忽略行尾 `\r` 的 CRLF 差异）→ 幂等直返原内容
/// - 已有行不同 → 直接覆盖（.env 变量值由用户持有，覆盖即更新语义）
/// - 已有行是跨行引号值 → [`ProviderAdminError::Invalid`]（单行级改写会破坏
///   文件结构）
/// - 追加时保证与既有内容之间恰好一个换行分隔
pub fn upsert_env_line(
    content: &str,
    var: &str,
    new_line: &str,
) -> Result<String, ProviderAdminError> {
    let lines: Vec<&str> = content.split('\n').collect();
    let target = lines.iter().position(|l| env_line_var(l) == Some(var));
    match target {
        Some(idx) => {
            let existing = lines[idx].trim_end_matches('\r');
            if existing == new_line {
                // 幂等：目标行已是期望内容，原样返回（不改写文件，也不动行尾风格）
                return Ok(content.to_string());
            }
            if is_multiline_value(lines[idx]) {
                return Err(ProviderAdminError::Invalid(format!(
                    "环境变量 {var} 的现有值跨多行，无法单行级改写（请手工整理 .env 后重试）"
                )));
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

/// 备好 .env 单行级 upsert 的新内容（格式化 + 改写一步到位）
///
/// 供两段式写回的「先备好、推迟提交」编排使用：含跨行值拒绝，toml 侧写盘
/// 失败时不会动 .env。
pub fn prepare_env_upsert(
    content: &str,
    env_var: &str,
    api_key: &str,
) -> Result<String, ProviderAdminError> {
    let new_line = format_env_line(env_var, api_key)?;
    upsert_env_line(content, env_var, &new_line)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let out = upsert_env_line(content, "K", "K=v1").unwrap();
        assert_eq!(out, "# 手写注释\nOTHER_VAR=keep\nK=v1\n");
    }

    #[test]
    fn upsert_append_adds_separator_when_missing_trailing_newline() {
        let out = upsert_env_line("A=1", "K", "K=v").unwrap();
        assert_eq!(out, "A=1\nK=v\n");
    }

    #[test]
    fn upsert_overwrites_existing_line_keeping_others_verbatim() {
        let content = "# 注释\nA=1\nK=old\nB=2\n";
        let out = upsert_env_line(content, "K", "K=new").unwrap();
        assert_eq!(out, "# 注释\nA=1\nK=new\nB=2\n");
    }

    #[test]
    fn upsert_idempotent_when_line_identical() {
        let content = "K=v\n";
        let out = upsert_env_line(content, "K", "K=v").unwrap();
        assert_eq!(out, content);
    }

    #[test]
    fn upsert_treats_crlf_existing_line_as_same_content() {
        // CRLF 文件里已有行带 \r：与 LF 新行语义相同，幂等不改写（保住 CRLF 风格）
        let content = "K=v\r\n";
        let out = upsert_env_line(content, "K", "K=v").unwrap();
        assert_eq!(out, content);
    }

    #[test]
    fn upsert_rejects_multiline_existing_value() {
        let err = upsert_env_line("K='start\ncontinues'\n", "K", "K=v").unwrap_err();
        assert!(matches!(err, ProviderAdminError::Invalid(_)));
    }

    // ===== 组合原语 =====

    #[test]
    fn prepare_env_upsert_formats_and_rewrites_in_one_step() {
        let out = prepare_env_upsert("A=1\n", "K", "sk-abc").unwrap();
        assert_eq!(out, "A=1\nK=sk-abc\n");
    }

    #[test]
    fn prepare_env_upsert_propagates_invalid_plaintext() {
        assert!(prepare_env_upsert("", "K", "a'b").is_err());
    }
}
