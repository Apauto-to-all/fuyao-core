//! MCP 安全辅助函数
//!
//! 环境变量过滤、错误信息脱敏、标识符归一化。

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

/// 错误脱敏与名称归一化用到的正则
///
/// 两条模式均为编译期常量字面量，聚合为进程级静态量只编译一次，
/// 避免每次 `sanitize_error` / `sanitize_mcp_name_component` 调用都重复编译。
struct SanitizeRegexes {
    cred: Regex,
    ident: Regex,
}

static RES: LazyLock<SanitizeRegexes> = LazyLock::new(|| SanitizeRegexes {
    cred: Regex::new(concat!(
        r"(?i)(?:ghp_[A-Za-z0-9_]{1,255}",
        r"|sk-[A-Za-z0-9_]{1,255}",
        r"|Bearer\s+\S+",
        r"|token=[^\s&,;]{1,255}",
        r"|key=[^\s&,;]{1,255}",
        r"|API_KEY=[^\s&,;]{1,255}",
        r"|password=[^\s&,;]{1,255}",
        r"|secret=[^\s&,;]{1,255})",
    ))
    .expect("凭证脱敏正则编译失败"),
    ident: Regex::new(r"[^A-Za-z0-9_]").expect("标识符正则编译失败"),
});

/// 安全的环境变量 key 集合
const SAFE_ENV_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "TERM",
    "SHELL",
    "TMPDIR",
    "SYSTEMROOT",
    "COMSPEC",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "APPDATA",
    "LOCALAPPDATA",
    "HOMEDRIVE",
    "HOMEPATH",
];

/// 构建安全的环境变量字典
///
/// 只传递安全的基础变量 + 用户在配置中明确指定的变量。
/// 防止意外泄露 API Key 等敏感信息给 MCP 子进程。
pub fn build_safe_env(user_env: Option<&HashMap<String, String>>) -> HashMap<String, String> {
    let mut env = HashMap::new();

    for (key, value) in std::env::vars() {
        let key_upper = key.to_uppercase();
        if SAFE_ENV_KEYS.iter().any(|k| k.to_uppercase() == key_upper)
            || key_upper.starts_with("XDG_")
        {
            env.insert(key, value);
        }
    }

    if let Some(user) = user_env {
        env.extend(user.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    env
}

/// 脱敏错误信息中的凭证模式
///
/// 替换常见的 token、key、password 等敏感信息为 `[REDACTED]`。
pub fn sanitize_error(text: &str) -> String {
    RES.cred.replace_all(text, "[REDACTED]").into_owned()
}

/// 将 MCP 名称组件转为安全的标识符
///
/// 连字符、点号等非字母数字字符替换为下划线。
pub fn sanitize_mcp_name_component(value: &str) -> String {
    RES.ident.replace_all(value, "_").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_safe_env_includes_safe_keys() {
        unsafe { std::env::set_var("PATH", "/usr/bin") };
        let env = build_safe_env(None);
        // Windows 上 key 为 "Path"，需要大小写不敏感检查
        let has_path = env.keys().any(|k| k.eq_ignore_ascii_case("PATH"));
        assert!(has_path);
    }

    #[test]
    fn build_safe_env_excludes_sensitive_keys() {
        unsafe { std::env::set_var("API_KEY", "secret123") };
        let env = build_safe_env(None);
        assert!(!env.contains_key("API_KEY"));
        unsafe { std::env::remove_var("API_KEY") };
    }

    #[test]
    fn build_safe_env_includes_user_env() {
        let mut user = HashMap::new();
        user.insert("MY_TOOL_KEY".to_string(), "value".to_string());
        let env = build_safe_env(Some(&user));
        assert!(env.contains_key("MY_TOOL_KEY"));
    }

    #[test]
    fn build_safe_env_user_env_overrides() {
        unsafe { std::env::set_var("PATH", "/original") };
        let mut user = HashMap::new();
        user.insert("PATH".to_string(), "/override".to_string());
        let env = build_safe_env(Some(&user));
        assert_eq!(env.get("PATH"), Some(&"/override".to_string()));
    }

    #[test]
    fn sanitize_error_redacts_github_token() {
        let result = sanitize_error("error with ghp_abc123def456");
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("ghp_abc123def456"));
    }

    #[test]
    fn sanitize_error_redacts_sk_token() {
        let result = sanitize_error("key=sk-proj-abc123");
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_error_redacts_bearer() {
        let result = sanitize_error("Bearer abc123token");
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_error_preserves_normal_text() {
        let result = sanitize_error("connection refused");
        assert_eq!(result, "connection refused");
    }

    #[test]
    fn sanitize_mcp_name_component_replaces_hyphens() {
        assert_eq!(sanitize_mcp_name_component("my-server"), "my_server");
    }

    #[test]
    fn sanitize_mcp_name_component_replaces_dots() {
        assert_eq!(sanitize_mcp_name_component("server.v1"), "server_v1");
    }

    #[test]
    fn sanitize_mcp_name_component_preserves_alphanumeric() {
        assert_eq!(sanitize_mcp_name_component("server123"), "server123");
    }
}
