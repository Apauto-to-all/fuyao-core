//! 重定向处理
//!
//! 判断重定向是否允许（同域名规则），从响应中提取重定向 URL。

use reqwest::Response;
use url::Url;

/// 判断是否为同域名重定向
///
/// 允许规则：
/// - example.com → www.example.com（添加 www）
/// - www.example.com → example.com（移除 www）
/// - example.com/a → example.com/b（同域名改路径）
///
/// 不允许：
/// - 跨域名重定向（example.com → other.com）
/// - 协议变化（http → https 除外，reqwest 会自动升级）
/// - 包含用户名密码
pub fn is_same_domain_redirect(original_url: &str, redirect_url: &str) -> bool {
    let parsed_orig = match Url::parse(original_url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    let parsed_redir = match Url::parse(redirect_url) {
        Ok(u) => u,
        Err(_) => return false,
    };

    // 协议必须相同（reqwest 已自动升级 http → https，此处检查最终协议）
    if parsed_orig.scheme() != parsed_redir.scheme() {
        return false;
    }

    // 端口必须相同
    if parsed_orig.port() != parsed_redir.port() {
        return false;
    }

    // 不能包含用户名密码
    if parsed_redir.username() != "" || parsed_redir.password().is_some() {
        return false;
    }

    // 比较主域名（忽略 www 前缀）
    let orig_host = parsed_orig.host_str().unwrap_or("").to_lowercase();
    let redir_host = parsed_redir.host_str().unwrap_or("").to_lowercase();

    let orig_base = orig_host.strip_prefix("www.").unwrap_or(&orig_host);
    let redir_base = redir_host.strip_prefix("www.").unwrap_or(&redir_host);

    orig_base == redir_base
}

/// 从 reqwest 响应中获取重定向 URL
pub fn get_redirect_url(response: &Response) -> Option<String> {
    let location = response.headers().get("location")?;
    let location_str = location.to_str().ok()?;

    // 处理相对 URL
    if location_str.starts_with('/') {
        let base_url = response.url().to_string();
        if let Ok(parsed) = Url::parse(&base_url) {
            return Some(format!(
                "{}://{}{}",
                parsed.scheme(),
                parsed.host_str().unwrap_or(""),
                location_str
            ));
        }
    }

    Some(location_str.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_domain_redirect_basic() {
        assert!(is_same_domain_redirect(
            "https://example.com/a",
            "https://example.com/b"
        ));
    }

    #[test]
    fn same_domain_redirect_with_www() {
        assert!(is_same_domain_redirect(
            "https://example.com/a",
            "https://www.example.com/b"
        ));
        assert!(is_same_domain_redirect(
            "https://www.example.com/a",
            "https://example.com/b"
        ));
    }

    #[test]
    fn cross_domain_redirect_blocked() {
        assert!(!is_same_domain_redirect(
            "https://example.com/a",
            "https://other.com/b"
        ));
    }

    #[test]
    fn different_scheme_blocked() {
        assert!(!is_same_domain_redirect(
            "https://example.com/a",
            "http://example.com/b"
        ));
    }

    #[test]
    fn credentials_in_redirect_blocked() {
        assert!(!is_same_domain_redirect(
            "https://example.com/a",
            "https://user:pass@example.com/b"
        ));
    }

    #[test]
    fn different_port_blocked() {
        assert!(!is_same_domain_redirect(
            "https://example.com/a",
            "https://example.com:8443/b"
        ));
    }

    #[test]
    fn invalid_url_returns_false() {
        assert!(!is_same_domain_redirect("not-a-url", "also-not"));
    }
}
