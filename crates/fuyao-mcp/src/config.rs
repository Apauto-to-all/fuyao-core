//! MCP 配置加载
//!
//! 从 FuyaoConfig 中读取 mcp_servers 配置，
//! 支持 ${ENV_VAR} 环境变量插值。

use std::collections::HashMap;

use fuyao_api::MCPServerConfig;
use regex::Regex;

/// 递归解析 ${VAR} 占位符
///
/// 从环境变量中替换配置中的环境变量引用。
/// 未找到的环境变量保留原样。
fn interpolate_env_vars_string(value: &str) -> String {
    let re = Regex::new(r"\$\{([^}]+)\}").expect("正则表达式编译失败");
    re.replace_all(value, |caps: &regex::Captures| {
        std::env::var(&caps[1]).unwrap_or_else(|_| caps[0].to_string())
    })
    .into_owned()
}

/// 递归解析 JSON Value 中的 ${VAR} 占位符
fn interpolate_env_vars_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(interpolate_env_vars_string(s)),
        serde_json::Value::Object(map) => {
            let new_map: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), interpolate_env_vars_value(v)))
                .collect();
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(interpolate_env_vars_value).collect())
        }
        other => other.clone(),
    }
}

/// 解析 mcp_servers 配置段
///
/// 从 fuyao.toml 中 mcp_servers 键下的原始字典解析为 MCPServerConfig 映射。
/// 自动进行环境变量插值。
pub fn parse_mcp_servers(raw: Option<&serde_json::Value>) -> HashMap<String, MCPServerConfig> {
    let Some(raw) = raw else {
        return HashMap::new();
    };
    let Some(map) = raw.as_object() else {
        return HashMap::new();
    };

    let mut result = HashMap::new();
    for (name, cfg) in map {
        let Some(cfg_obj) = cfg.as_object() else {
            continue;
        };

        // 将 serde_json::Map 转换为 serde_json::Value 再做插值
        let cfg_value = serde_json::Value::Object(cfg_obj.clone());
        let resolved = interpolate_env_vars_value(&cfg_value);

        // 通过 serde_json 反序列化为 MCPServerConfig
        if let Ok(config) = serde_json::from_value::<MCPServerConfig>(resolved) {
            result.insert(name.clone(), config);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_env_vars_string_no_placeholder() {
        assert_eq!(interpolate_env_vars_string("hello"), "hello");
    }

    #[test]
    fn interpolate_env_vars_string_with_env() {
        unsafe { std::env::set_var("FUYAO_TEST_MCP_HOST", "example.com") };
        let result = interpolate_env_vars_string("https://${FUYAO_TEST_MCP_HOST}/mcp");
        assert_eq!(result, "https://example.com/mcp");
        unsafe { std::env::remove_var("FUYAO_TEST_MCP_HOST") };
    }

    #[test]
    fn interpolate_env_vars_string_missing_env_keeps_original() {
        let result = interpolate_env_vars_string("${FUYAO_NONEXISTENT_VAR_12345}");
        assert_eq!(result, "${FUYAO_NONEXISTENT_VAR_12345}");
    }

    #[test]
    fn interpolate_env_vars_value_string() {
        unsafe { std::env::set_var("FUYAO_TEST_CMD", "node") };
        let value = serde_json::json!("${FUYAO_TEST_CMD}");
        let result = interpolate_env_vars_value(&value);
        assert_eq!(result, "node");
        unsafe { std::env::remove_var("FUYAO_TEST_CMD") };
    }

    #[test]
    fn interpolate_env_vars_value_object() {
        unsafe { std::env::set_var("FUYAO_TEST_URL", "http://localhost:8080") };
        let value = serde_json::json!({
            "url": "${FUYAO_TEST_URL}",
            "timeout": 60
        });
        let result = interpolate_env_vars_value(&value);
        assert_eq!(result["url"], "http://localhost:8080");
        assert_eq!(result["timeout"], 60);
        unsafe { std::env::remove_var("FUYAO_TEST_URL") };
    }

    #[test]
    fn interpolate_env_vars_value_array() {
        unsafe { std::env::set_var("FUYAO_TEST_ARG", "value") };
        let value = serde_json::json!(["${FUYAO_TEST_ARG}", "static"]);
        let result = interpolate_env_vars_value(&value);
        assert_eq!(result[0], "value");
        assert_eq!(result[1], "static");
        unsafe { std::env::remove_var("FUYAO_TEST_ARG") };
    }

    #[test]
    fn parse_mcp_servers_none_input() {
        let result = parse_mcp_servers(None);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_mcp_servers_non_object_input() {
        let result = parse_mcp_servers(Some(&serde_json::json!("string")));
        assert!(result.is_empty());
    }

    #[test]
    fn parse_mcp_servers_valid_config() {
        let input = serde_json::json!({
            "my-server": {
                "command": "npx",
                "args": ["-y", "some-mcp-server"],
                "timeout": 60
            }
        });
        let result = parse_mcp_servers(Some(&input));
        assert_eq!(result.len(), 1);
        let config = result.get("my-server").expect("应存在 my-server");
        assert_eq!(config.command, Some("npx".to_string()));
        assert_eq!(config.timeout, 60);
    }

    #[test]
    fn parse_mcp_servers_http_config() {
        let input = serde_json::json!({
            "remote": {
                "url": "http://localhost:8080/mcp",
                "headers": {"Authorization": "Bearer token"}
            }
        });
        let result = parse_mcp_servers(Some(&input));
        assert_eq!(result.len(), 1);
        let config = result.get("remote").expect("应存在 remote");
        assert!(config.is_http());
    }

    #[test]
    fn parse_mcp_servers_skips_invalid_entry() {
        let input = serde_json::json!({
            "valid": {
                "command": "node"
            },
            "invalid": "not an object"
        });
        let result = parse_mcp_servers(Some(&input));
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("valid"));
    }
}
