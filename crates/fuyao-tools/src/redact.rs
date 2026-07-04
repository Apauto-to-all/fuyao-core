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

/// 脱敏文本中的敏感信息
pub fn redact_sensitive_text(text: &str) -> String {
    if text.is_empty() || !crate::config::REDACT_SECRETS {
        return text.to_string();
    }

    let text = PATTERNS
        .prefix
        .replace_all(text, |caps: &fancy_regex::Captures| mask_token(&caps[1]))
        .to_string();

    let text = PATTERNS
        .env_assign
        .replace_all(&text, |caps: &fancy_regex::Captures| {
            format!("{}={}{}", &caps[1], &caps[2], mask_token(&caps[3]))
        })
        .to_string();

    let text = PATTERNS
        .json_field
        .replace_all(&text, |caps: &fancy_regex::Captures| {
            format!(r#"{}: "{}""#, &caps[1], mask_token(&caps[2]))
        })
        .to_string();

    let text = PATTERNS
        .auth_header
        .replace_all(&text, |caps: &fancy_regex::Captures| {
            format!("{}{}", &caps[1], mask_token(&caps[2]))
        })
        .to_string();

    let text = PATTERNS
        .private_key
        .replace_all(&text, "[REDACTED PRIVATE KEY]")
        .to_string();

    let text = PATTERNS
        .db_connstr
        .replace_all(&text, |caps: &fancy_regex::Captures| {
            format!("{}***{}", &caps[1], &caps[3])
        })
        .to_string();

    PATTERNS
        .jwt
        .replace_all(&text, |caps: &fancy_regex::Captures| mask_token(&caps[0]))
        .to_string()
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
}
