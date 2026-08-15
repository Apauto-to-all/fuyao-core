//! 敏感信息脱敏模块
//!
//! 使用正则表达式匹配并脱敏 API keys、tokens、credentials 等敏感信息。
//! 短 token（< 18 字符）完全遮蔽，长 token 保留前 6 和后 4 字符用于调试。
//!
//! ## 覆盖的敏感信息类型
//!
//! - **API Key 前缀**: sk-, ghp_, AIza, AKIA, hf_, pypi-, npm_, gsk_, pplx- 等
//! - **环境变量赋值**: API_KEY=xxx, TOKEN=xxx, SECRET=xxx 等
//! - **JSON 字段值**: "api_key": "xxx", "token": "xxx" 等
//! - **Authorization 头**: Bearer token
//! - **PEM 私钥**: -----BEGIN PRIVATE KEY----- 块
//! - **数据库连接串**: postgres://user:pass@host, mongodb+srv:// 等
//! - **JWT 令牌**: eyJ... 格式的三段式 token

use fancy_regex::Regex;
use std::sync::LazyLock;

struct RedactPatterns {
    prefix: Regex,
    env_assign: Regex,
    json_field: Regex,
    auth_header: Regex,
    private_key: Regex,
    db_connstr: Regex,
    jwt: Regex,
}

static PATTERNS: LazyLock<RedactPatterns> = LazyLock::new(|| {
    let prefix_re = Regex::new(
        r"(?<![A-Za-z0-9_-])(sk-[A-Za-z0-9_-]{10,}|sk_live_[A-Za-z0-9]{10,}|sk_test_[A-Za-z0-9]{10,}|ghp_[A-Za-z0-9]{10,}|github_pat_[A-Za-z0-9_]{10,}|AIza[A-Za-z0-9_-]{30,}|AKIA[A-Z0-9]{16}|xox[baprs]-[A-Za-z0-9-]{10,}|hf_[A-Za-z0-9]{10,}|pypi-[A-Za-z0-9_-]{10,}|npm_[A-Za-z0-9]{10,}|gsk_[A-Za-z0-9]{10,}|pplx-[A-Za-z0-9]{10,})(?![A-Za-z0-9_-])"
    ).unwrap();

    let env_assign_re = Regex::new(
        r#"([A-Z0-9_]{0,50}(?:API_?KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|AUTH)[A-Z0-9_]{0,50})\s*=\s*(['"]?)(\S+)\2"#
    ).unwrap();

    let json_field_re = Regex::new(
        r#"("(?:api_?[Kk]ey|token|secret|password|access_token|refresh_token|auth_token|bearer)")\s*:\s*"([^"]+)""#
    ).unwrap();

    let auth_header_re = Regex::new(r"(?i)(Authorization:\s*Bearer\s+)(\S+)").unwrap();

    let private_key_re =
        Regex::new(r"-----BEGIN[A-Z ]*PRIVATE KEY-----[\s\S]*?-----END[A-Z ]*PRIVATE KEY-----")
            .unwrap();

    let db_connstr_re = Regex::new(
        r"(?i)((?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis|amqp)://[^:]+:)([^@]+)(@)",
    )
    .unwrap();

    let jwt_re = Regex::new(r"eyJ[A-Za-z0-9_-]{10,}(?:\.[A-Za-z0-9_=-]{4,}){0,2}").unwrap();

    RedactPatterns {
        prefix: prefix_re,
        env_assign: env_assign_re,
        json_field: json_field_re,
        auth_header: auth_header_re,
        private_key: private_key_re,
        db_connstr: db_connstr_re,
        jwt: jwt_re,
    }
});

fn mask_token(token: &str) -> String {
    if token.len() < 18 {
        "***".to_string()
    } else {
        format!("{}...{}", &token[..6], &token[token.len() - 4..])
    }
}

/// 各脱敏模式的必备字面量（ASCII 大小写不敏感预筛）
///
/// 每条正则命中时必然包含对应片段之一；全部缺席时正则不可能命中，直接跳过执行。
/// 预筛防的是回溯引擎的失效扫描：超长无命中文本上，逐起点的失败尝试会累积耗尽
/// 回溯预算导致执行报错。片段按 ASCII 大小写不敏感口径给出——预筛误放行只会
/// 多跑一次正则（无害方向）；Unicode 折叠等极端形态不在预筛口径内
const PREFIX_NEEDLES: &[&str] = &[
    "sk-",
    "ghp_",
    "github_pat_",
    "aiza",
    "akia",
    "xox",
    "hf_",
    "pypi-",
    "npm_",
    "gsk_",
    "pplx-",
];
const ENV_ASSIGN_NEEDLES: &[&str] = &[
    "api_key",
    "apikey",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "auth",
];
const JSON_FIELD_NEEDLES: &[&str] = &["api_key", "apikey", "token", "secret", "password", "bearer"];
const AUTH_HEADER_NEEDLES: &[&str] = &["authorization"];
const PRIVATE_KEY_NEEDLES: &[&str] = &["-----begin"];
const DB_CONNSTR_NEEDLES: &[&str] = &["postgres", "mysql", "mongodb", "redis", "amqp"];
const JWT_NEEDLES: &[&str] = &["eyj"];

/// ASCII 大小写不敏感的子串探测（任一命中即真，无分配）
///
/// 针串均为 ASCII 字面量：多字节 UTF-8 字节经 ASCII 折叠后不会与针串字节相等，
/// 按字节窗口比较即安全
fn contains_any_ascii_ci(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| {
        let n = needle.as_bytes();
        if n.is_empty() {
            return true;
        }
        text.as_bytes()
            .windows(n.len())
            .any(|w| w.iter().zip(n).all(|(a, b)| a.eq_ignore_ascii_case(b)))
    })
}

/// 执行单条脱敏：预筛未命中直接跳过；正则执行出错（如回溯超限）降级保留原文并告警
///
/// 降级取向：脱敏是尽力而为的输出净化，引擎级故障不应放大为整个工具调用失败；
/// 保留原文存在泄密风险，故必须 WARN 留痕供事后审计
fn try_redact(
    re: &Regex,
    text: &str,
    needles: &[&str],
    rep: impl Fn(&fancy_regex::Captures) -> String,
) -> String {
    if !contains_any_ascii_ci(text, needles) {
        return text.to_string();
    }
    match re.try_replacen(text, 0, rep) {
        Ok(replaced) => replaced.into_owned(),
        Err(e) => {
            tracing::warn!(cause = %e, text_len = text.len(), "脱敏正则执行失败，本轮保留原文");
            text.to_string()
        }
    }
}

/// 脱敏文本中的敏感信息
pub fn redact_sensitive_text(text: &str) -> String {
    if text.is_empty() || !crate::config::REDACT_SECRETS {
        return text.to_string();
    }

    let text = try_redact(&PATTERNS.prefix, text, PREFIX_NEEDLES, |caps| {
        mask_token(&caps[1])
    });
    let text = try_redact(&PATTERNS.env_assign, &text, ENV_ASSIGN_NEEDLES, |caps| {
        format!("{}={}{}", &caps[1], &caps[2], mask_token(&caps[3]))
    });
    let text = try_redact(&PATTERNS.json_field, &text, JSON_FIELD_NEEDLES, |caps| {
        format!(r#"{}: "{}""#, &caps[1], mask_token(&caps[2]))
    });
    let text = try_redact(&PATTERNS.auth_header, &text, AUTH_HEADER_NEEDLES, |caps| {
        format!("{}{}", &caps[1], mask_token(&caps[2]))
    });
    let text = try_redact(&PATTERNS.private_key, &text, PRIVATE_KEY_NEEDLES, |_| {
        "[REDACTED PRIVATE KEY]".to_string()
    });
    let text = try_redact(&PATTERNS.db_connstr, &text, DB_CONNSTR_NEEDLES, |caps| {
        format!("{}***{}", &caps[1], &caps[3])
    });
    try_redact(&PATTERNS.jwt, &text, JWT_NEEDLES, |caps| {
        mask_token(&caps[0])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_sk_token() {
        let text = "key=sk-1234567890abcdefghijklmnop";
        let result = redact_sensitive_text(text);
        assert!(!result.contains("sk-1234567890abcdefghijklmnop"));
    }

    #[test]
    fn redact_env_var() {
        let text = "API_KEY=mysecretkey123";
        let result = redact_sensitive_text(text);
        assert!(!result.contains("mysecretkey123"));
    }

    #[test]
    fn redact_json_field() {
        let text = r#"{"api_key": "sk-1234567890abcdefghijklmnop"}"#;
        let result = redact_sensitive_text(text);
        assert!(!result.contains("sk-1234567890abcdefghijklmnop"));
    }

    #[test]
    fn redact_auth_header() {
        let text = "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0";
        let result = redact_sensitive_text(text);
        assert!(!result.contains("eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn redact_db_connection_string() {
        let text = "postgres://user:secretpassword@localhost:5432/db";
        let result = redact_sensitive_text(text);
        assert!(!result.contains("secretpassword"));
        assert!(result.contains("postgres://user:"));
        assert!(result.contains("@localhost"));
    }

    #[test]
    fn no_redact_normal_text() {
        let text = "normal text without secrets";
        let result = redact_sensitive_text(text);
        assert_eq!(result, text);
    }

    #[test]
    fn redact_empty_string() {
        let result = redact_sensitive_text("");
        assert_eq!(result, "");
    }

    /// 超长重复文本（含大量数字 run）不触发回溯超限 panic：
    /// 预筛缺席直接跳过正则，即使误入也会降级保留原文
    #[test]
    fn redact_huge_repetitive_text_no_panic() {
        let text: String = (1..=3000)
            .map(|i| format!("data row {i:06} alpha beta gamma delta\n"))
            .collect();
        assert!(text.len() > 100_000, "测试前提：文本规模达到回溯危险区");
        let result = redact_sensitive_text(&text);
        assert!(result.contains("data row 000001"));
        assert_eq!(result.len(), text.len(), "无敏感内容时应原样保留");
    }

    /// 预筛命中但正则无匹配：内容原样返回
    #[test]
    fn redact_prefilter_hit_but_no_match() {
        let text = "an author wrote tokens of gratitude"; // 含 "auth"/"token" 字面量但无赋值形态
        let result = redact_sensitive_text(text);
        assert_eq!(result, text);
    }
}
