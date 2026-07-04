//! URL 安全检查
//!
//! 防止 SSRF（Server-Side Request Forgery），阻止请求私有网络地址和云元数据端点。
//! DNS 解析失败时阻断（fail closed）。

use super::types::URLSafetyResult;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use url::Url;

/// 云元数据 hostname，永远阻断
const BLOCKED_HOSTNAMES: &[&str] = &["metadata.google.internal", "metadata.goog"];

/// 云元数据 IP 地址，永远阻断
const ALWAYS_BLOCKED_IPS: &[&str] = &[
    "169.254.169.254", // AWS/GCP/Azure/DO/Oracle
    "169.254.170.2",   // AWS ECS task metadata
    "169.254.169.253", // Azure IMDS
    "fd00:ec2::254",   // AWS metadata (IPv6)
    "100.100.100.200", // Alibaba Cloud
];

/// CGNAT 网段前缀（100.64.0.0/10）
/// 前 10 位 = 0x6440，即 100.64.0.0 ~ 100.127.255.255
const CGNAT_PREFIX_U32: u32 = 0x6440_0000;
const CGNAT_MASK_U32: u32 = 0xFFC0_0000;

/// 检查 IPv4 是否为私有地址（10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16）
fn is_ipv4_private(ip: &Ipv4Addr) -> bool {
    let octets = ip.octets();
    // 10.0.0.0/8
    if octets[0] == 10 {
        return true;
    }
    // 172.16.0.0/12
    if octets[0] == 172 && (16..=31).contains(&octets[1]) {
        return true;
    }
    // 192.168.0.0/16
    if octets[0] == 192 && octets[1] == 168 {
        return true;
    }
    false
}

/// 检查 IPv4 是否为回环地址（127.0.0.0/8）
fn is_ipv4_loopback(ip: &Ipv4Addr) -> bool {
    ip.octets()[0] == 127
}

/// 检查 IPv4 是否为 link-local（169.254.0.0/16）
fn is_ipv4_link_local(ip: &Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 169 && octets[1] == 254
}

/// 检查 IPv4 是否为保留地址
fn is_ipv4_reserved(ip: &Ipv4Addr) -> bool {
    let octets = ip.octets();
    // 0.0.0.0/8
    if octets[0] == 0 {
        return true;
    }
    // 100.64.0.0/10 (CGNAT) — 单独检查
    // 128.0.0.0/16
    if octets[0] == 128 && octets[1] == 0 {
        return true;
    }
    // 191.255.0.0/16
    if octets[0] == 191 && octets[1] == 255 {
        return true;
    }
    // 192.0.0.0/24
    if octets[0] == 192 && octets[1] == 0 && octets[2] == 0 {
        return true;
    }
    // 192.0.2.0/24
    if octets[0] == 192 && octets[1] == 0 && octets[2] == 2 {
        return true;
    }
    // 198.18.0.0/15
    if octets[0] == 198 && (octets[1] == 18 || octets[1] == 19) {
        return true;
    }
    // 198.51.100.0/24
    if octets[0] == 198 && octets[1] == 51 && octets[2] == 100 {
        return true;
    }
    // 203.0.113.0/24
    if octets[0] == 203 && octets[1] == 0 && octets[2] == 113 {
        return true;
    }
    // 224.0.0.0/4 (组播)
    if octets[0] >= 224 {
        return true;
    }
    false
}

/// 检查 IPv4 是否为组播地址（224.0.0.0/4）
fn is_ipv4_multicast(ip: &Ipv4Addr) -> bool {
    ip.octets()[0] >= 224
}

/// 检查 IPv6 是否为回环地址（::1）
fn is_ipv6_loopback(ip: &Ipv6Addr) -> bool {
    *ip == Ipv6Addr::LOCALHOST
}

/// 检查 IPv6 是否为组播地址（ff00::/8）
fn is_ipv6_multicast(ip: &Ipv6Addr) -> bool {
    ip.segments()[0] >= 0xff00
}

/// 检查 IPv6 是否为未指定地址（::）
fn is_ipv6_unspecified(ip: &Ipv6Addr) -> bool {
    *ip == Ipv6Addr::UNSPECIFIED
}

/// 检查 IP 是否应被阻断
///
/// 包括：私有地址、回环地址、link-local、保留地址、CGNAT、组播、未指定地址
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            if is_ipv4_private(v4) || is_ipv4_loopback(v4) || is_ipv4_link_local(v4) {
                return true;
            }
            if is_ipv4_reserved(v4) || is_ipv4_multicast(v4) {
                return true;
            }
            // CGNAT: 100.64.0.0/10
            let ip_u32 = u32::from_be_bytes(v4.octets());
            if (ip_u32 & CGNAT_MASK_U32) == CGNAT_PREFIX_U32 {
                return true;
            }
            false
        }
        IpAddr::V6(v6) => {
            // 检查 IPv4-mapped IPv6 地址（::ffff:x.x.x.x）
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_ip(&IpAddr::V4(v4));
            }
            is_ipv6_loopback(v6) || is_ipv6_multicast(v6) || is_ipv6_unspecified(v6)
        }
    }
}

/// 检查 IP 是否为回环地址
fn is_loopback_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_ipv4_loopback(v4),
        IpAddr::V6(v6) => is_ipv6_loopback(v6),
    }
}

/// 检查 IP 是否为永远阻断的云元数据地址
fn is_always_blocked_ip(ip: &IpAddr) -> bool {
    for blocked in ALWAYS_BLOCKED_IPS {
        if let Ok(blocked_ip) = blocked.parse::<IpAddr>()
            && ip == &blocked_ip
        {
            return true;
        }
    }
    // link-local 网段 169.254.0.0/16
    if let IpAddr::V4(v4) = ip
        && is_ipv4_link_local(v4)
    {
        return true;
    }
    false
}

/// 检查 URL 安全性（异步，需要 DNS 解析）
///
/// 阻断私有网络地址和云元数据端点，防止 SSRF 攻击。
/// DNS 解析失败时阻断（fail closed）。
pub async fn check_url_safety(url: &str) -> URLSafetyResult {
    let parsed = match Url::parse(url) {
        Ok(u) => u,
        Err(_) => {
            return URLSafetyResult {
                safe: false,
                hostname: None,
                resolved_ip: None,
                message: Some("URL 解析失败：格式不正确".to_string()),
                suggestion: Some("请检查 URL 格式是否正确".to_string()),
            };
        }
    };

    let hostname = match parsed.host_str() {
        Some(h) => h.trim().to_lowercase(),
        None => {
            return URLSafetyResult {
                safe: false,
                hostname: None,
                resolved_ip: None,
                message: Some("URL 解析失败：缺少 hostname".to_string()),
                suggestion: Some("请检查 URL 格式是否正确".to_string()),
            };
        }
    };

    if hostname.is_empty() {
        return URLSafetyResult {
            safe: false,
            hostname: None,
            resolved_ip: None,
            message: Some("URL 解析失败：缺少 hostname".to_string()),
            suggestion: Some("请检查 URL 格式是否正确".to_string()),
        };
    }

    // 阻断已知危险 hostname
    if BLOCKED_HOSTNAMES.contains(&hostname.as_str()) {
        return URLSafetyResult {
            safe: false,
            hostname: Some(hostname),
            resolved_ip: None,
            message: Some("阻断云元数据 hostname".to_string()),
            suggestion: Some(
                "云元数据端点（metadata.google.internal 等）禁止访问，请使用公开 URL".to_string(),
            ),
        };
    }

    // 异步 DNS 解析
    let addrs = match tokio::net::lookup_host((&*hostname, 0)).await {
        Ok(addrs) => addrs,
        Err(_) => {
            return URLSafetyResult {
                safe: false,
                hostname: Some(hostname.clone()),
                resolved_ip: None,
                message: Some(format!("阻断 URL — DNS 解析失败: {hostname}")),
                suggestion: Some("域名无法解析，请检查 URL 是否正确或域名是否存在".to_string()),
            };
        }
    };

    for addr in addrs {
        let ip = addr.ip();
        let ip_str = ip.to_string();

        // 永远阻断云元数据 IP 和 link-local
        if is_always_blocked_ip(&ip) {
            return URLSafetyResult {
                safe: false,
                hostname: Some(hostname.clone()),
                resolved_ip: Some(ip_str.clone()),
                message: Some(format!("阻断云元数据地址: {hostname} -> {ip_str}")),
                suggestion: Some("云元数据 IP（169.254.x.x）禁止访问，请使用公开 URL".to_string()),
            };
        }

        // 阻断私有/内网地址
        if is_blocked_ip(&ip) {
            let suggestion = if is_loopback_ip(&ip) {
                "回环地址（localhost/127.x.x.x）禁止访问，请使用公开 URL".to_string()
            } else {
                "私有网络地址（10.x.x.x、192.168.x.x 等）禁止访问，请使用公开可访问的 URL"
                    .to_string()
            };

            return URLSafetyResult {
                safe: false,
                hostname: Some(hostname.clone()),
                resolved_ip: Some(ip_str.clone()),
                message: Some(format!("阻断私有/内网地址: {hostname} -> {ip_str}")),
                suggestion: Some(suggestion),
            };
        }
    }

    URLSafetyResult {
        safe: true,
        hostname: Some(hostname),
        resolved_ip: None,
        message: None,
        suggestion: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_blocked_ip_private() {
        assert!(is_blocked_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_loopback() {
        assert!(is_blocked_ip(&"127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_cgnat() {
        assert!(is_blocked_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"100.127.255.255".parse().unwrap()));
        assert!(!is_blocked_ip(&"100.63.255.255".parse().unwrap()));
        assert!(!is_blocked_ip(&"100.128.0.1".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_public() {
        assert!(!is_blocked_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_blocked_ip(&"1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn is_always_blocked_ip_cloud_metadata() {
        assert!(is_always_blocked_ip(&"169.254.169.254".parse().unwrap()));
        assert!(is_always_blocked_ip(&"169.254.170.2".parse().unwrap()));
        assert!(is_always_blocked_ip(&"169.254.1.1".parse().unwrap()));
    }

    #[test]
    fn is_blocked_ip_ipv4_mapped_ipv6() {
        let ip: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        assert!(is_blocked_ip(&ip));
        let ip: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        assert!(is_blocked_ip(&ip));
    }

    #[tokio::test]
    async fn check_url_safety_invalid_url() {
        let result = check_url_safety("not-a-url").await;
        assert!(!result.safe);
    }

    #[tokio::test]
    async fn check_url_safety_no_hostname() {
        let result = check_url_safety("file:///tmp/test").await;
        assert!(!result.safe);
    }

    #[tokio::test]
    async fn check_url_safety_blocked_hostname() {
        let result = check_url_safety("http://metadata.google.internal/computeMetadata/v1/").await;
        assert!(!result.safe);
        assert!(result.message.unwrap().contains("云元数据"));
    }
}
