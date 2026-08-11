//! MCP 输入 schema 标准化
//!
//! 不同 MCP Server 返回的 JSON-Schema 形态各异（definitions/$ref、nullable union、
//! 缺失 type 等），需在转为 OpenAI function schema 前归一化。本模块集中处理这些
//! provider 兼容性修补，与安全脱敏职责分离。

/// 完整版 schema 标准化
///
/// 处理：
/// 1. definitions → $defs（Kimi/Moonshot 兼容）
/// 2. nullable union collapse（Anthropic 兼容）
/// 3. 缺失 type: object 修复
/// 4. required 数组裁剪（只保留 properties 中存在的字段）
pub fn normalize_mcp_input_schema(schema: &serde_json::Value) -> serde_json::Value {
    if schema.is_null() || !schema.is_object() {
        return serde_json::json!({"type": "object", "properties": {}});
    }

    let mut normalized = rewrite_local_refs(schema.clone());
    normalized = strip_nullable_union(normalized);
    normalized = repair_object_shape(normalized);

    // 最终兜底：type=object 但无 properties
    if let Some(obj) = normalized.as_object()
        && obj.get("type").and_then(|v| v.as_str()) == Some("object")
        && !obj.contains_key("properties")
        && let Some(obj) = normalized.as_object_mut()
    {
        obj.insert("properties".to_string(), serde_json::json!({}));
    }

    normalized
}

/// 重写 definitions → $defs，修复 $ref 路径
fn rewrite_local_refs(node: serde_json::Value) -> serde_json::Value {
    match node {
        serde_json::Value::Object(map) => {
            let mut normalized = serde_json::Map::new();
            for (key, value) in map {
                let out_key = if key == "definitions" {
                    "$defs".to_string()
                } else {
                    key
                };
                normalized.insert(out_key, rewrite_local_refs(value));
            }
            // 修复 $ref 路径
            if let Some(ref_val) = normalized.get("$ref").and_then(|v| v.as_str())
                && let Some(rest) = ref_val.strip_prefix("#/definitions/")
            {
                normalized.insert(
                    "$ref".to_string(),
                    serde_json::Value::String(format!("#/$defs/{rest}")),
                );
            }
            serde_json::Value::Object(normalized)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(rewrite_local_refs).collect())
        }
        other => other,
    }
}

/// 折叠 nullable union（anyOf: [type, null] → type + nullable: true）
fn strip_nullable_union(node: serde_json::Value) -> serde_json::Value {
    match node {
        serde_json::Value::Object(map) => {
            // 先递归处理所有子节点
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                result.insert(k, strip_nullable_union(v));
            }

            // 检查 anyOf 是否为 [value_branch, null_branch]
            if let Some(any_of) = result.get("anyOf").and_then(|v| v.as_array())
                && any_of.len() == 2
            {
                let mut null_branch = None;
                let mut value_branch = None;

                for branch in any_of {
                    if let Some(obj) = branch.as_object() {
                        if obj.get("type").and_then(|v| v.as_str()) == Some("null") {
                            null_branch = Some(true);
                        } else {
                            value_branch = Some(branch.clone());
                        }
                    }
                }

                if null_branch.is_some() && value_branch.is_some() {
                    // 合并 value_branch 的字段到 result
                    if let Some(value_obj) = value_branch.as_ref().and_then(|v| v.as_object()) {
                        for (k, v) in value_obj {
                            if k != "anyOf" && !result.contains_key(k.as_str()) {
                                result.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    result.remove("anyOf");
                    result.insert("nullable".to_string(), serde_json::json!(true));
                }
            }

            serde_json::Value::Object(result)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(strip_nullable_union).collect())
        }
        other => other,
    }
}

/// 修复 object 类型缺失 type/properties，裁剪 required
fn repair_object_shape(node: serde_json::Value) -> serde_json::Value {
    match node {
        serde_json::Value::Object(map) => {
            // 先递归
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                result.insert(k, repair_object_shape(v));
            }

            // 有 properties 或 required 但无 type → 补 type: object
            if !result.contains_key("type")
                && (result.contains_key("properties") || result.contains_key("required"))
            {
                result.insert("type".to_string(), serde_json::json!("object"));
            }

            // type=object 时修复 properties 和 required
            if result.get("type").and_then(|v| v.as_str()) == Some("object") {
                // 确保 properties 存在且为 object
                if !result.contains_key("properties")
                    || !result.get("properties").is_some_and(|v| v.is_object())
                {
                    result.insert("properties".to_string(), serde_json::json!({}));
                }

                // 裁剪 required：只保留 properties 中存在的字段
                if let Some(required) = result.get("required").and_then(|v| v.as_array()) {
                    let props = result
                        .get("properties")
                        .and_then(|v| v.as_object())
                        .map(|m| {
                            m.keys()
                                .cloned()
                                .collect::<std::collections::HashSet<String>>()
                        });

                    if let Some(prop_keys) = props {
                        let valid: Vec<serde_json::Value> = required
                            .iter()
                            .filter(|v| v.as_str().map(|s| prop_keys.contains(s)).unwrap_or(false))
                            .cloned()
                            .collect();
                        if valid.is_empty() {
                            result.remove("required");
                        } else {
                            result.insert("required".to_string(), serde_json::Value::Array(valid));
                        }
                    }
                }
            }

            serde_json::Value::Object(result)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(repair_object_shape).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_mcp_input_schema_null_input() {
        let result = normalize_mcp_input_schema(&serde_json::Value::Null);
        assert_eq!(result["type"], "object");
        assert!(result["properties"].is_object());
    }

    #[test]
    fn normalize_mcp_input_schema_rewrites_definitions() {
        let input = serde_json::json!({
            "definitions": {"Foo": {"type": "string"}},
            "$ref": "#/definitions/Foo"
        });
        let result = normalize_mcp_input_schema(&input);
        assert!(result.get("$defs").is_some());
        assert_eq!(result["$ref"], "#/$defs/Foo");
    }

    #[test]
    fn normalize_mcp_input_schema_strips_nullable_union() {
        let input = serde_json::json!({
            "anyOf": [
                {"type": "string"},
                {"type": "null"}
            ]
        });
        let result = normalize_mcp_input_schema(&input);
        assert_eq!(result["type"], "string");
        assert_eq!(result["nullable"], true);
        assert!(result.get("anyOf").is_none());
    }

    #[test]
    fn normalize_mcp_input_schema_repairs_missing_type() {
        let input = serde_json::json!({
            "properties": {"name": {"type": "string"}},
            "required": ["name"]
        });
        let result = normalize_mcp_input_schema(&input);
        assert_eq!(result["type"], "object");
    }

    #[test]
    fn normalize_mcp_input_schema_trims_required() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name", "nonexistent"]
        });
        let result = normalize_mcp_input_schema(&input);
        let required = result["required"].as_array().expect("required 应为数组");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "name");
    }

    #[test]
    fn normalize_mcp_input_schema_adds_empty_properties() {
        let input = serde_json::json!({"type": "object"});
        let result = normalize_mcp_input_schema(&input);
        assert!(result["properties"].is_object());
    }
}
