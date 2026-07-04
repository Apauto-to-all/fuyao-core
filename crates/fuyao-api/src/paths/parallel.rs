//! 工具并行策略
//!
//! 判断工具调用批次是否可以并行执行：
//! - never_parallel_tools 强制串行
//! - path_scoped_tools 检查路径重叠
//! - 其余工具必须在 parallel_safe_tools 中
//!
//! 包含路径规范化工具函数。

use crate::ToolRunnerConfig;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// 工具调用信息（轻量级，用于并行判断）
pub struct ToolCallInfo {
    pub name: String,
    pub arguments: String,
}

/// 判断工具调用批次是否可以并行执行
///
/// 判断顺序：
/// 1. <= 1 个调用 → 串行
/// 2. 含 never_parallel_tools → 串行
/// 3. path_scoped_tools 检查路径重叠 → 重叠则串行
/// 4. 其余工具不在 parallel_safe_tools → 串行
pub fn should_parallelize(tool_calls: &[ToolCallInfo], config: &ToolRunnerConfig) -> bool {
    if tool_calls.len() <= 1 {
        return false;
    }

    let mut reserved_paths: Vec<PathBuf> = Vec::new();

    for tc in tool_calls {
        if config.never_parallel_tools.contains(&tc.name) {
            return false;
        }

        let args: Value = serde_json::from_str(&tc.arguments).unwrap_or(Value::Null);

        if config.path_scoped_tools.contains(&tc.name) {
            let scoped_path = extract_path_from_args(&args);
            let Some(scoped_path) = scoped_path else {
                return false;
            };
            if reserved_paths
                .iter()
                .any(|existing| paths_overlap(&scoped_path, existing))
            {
                return false;
            }
            reserved_paths.push(scoped_path);
            continue;
        }

        if !config.parallel_safe_tools.contains(&tc.name) {
            return false;
        }
    }

    true
}

/// 从工具参数中提取路径
pub fn extract_path_from_args(args: &Value) -> Option<PathBuf> {
    let raw_path = args.get("path").and_then(|v| v.as_str())?;
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return None;
    }

    let expanded = if trimmed.starts_with('~') {
        dirs::home_dir()
            .map(|home| {
                let rest = &trimmed[1..];
                let rest = rest.strip_prefix(std::path::MAIN_SEPARATOR).unwrap_or(rest);
                home.join(rest)
            })
            .unwrap_or_else(|| PathBuf::from(trimmed))
    } else {
        PathBuf::from(trimmed)
    };

    if expanded.is_absolute() {
        Some(canonicalize_path(&expanded))
    } else {
        Some(canonicalize_path(
            &std::env::current_dir().unwrap_or_default().join(expanded),
        ))
    }
}

/// 规范化路径（不要求文件存在，仅处理 `.`/`..` 和分隔符）
pub fn canonicalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if components
                    .last()
                    .is_some_and(|c| *c != std::path::Component::ParentDir)
                {
                    components.pop();
                } else {
                    components.push(component);
                }
            }
            _ => components.push(component),
        }
    }
    components.iter().collect()
}

/// 判断两个路径是否重叠（同一目录或子目录）
pub fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left_parts: Vec<_> = left.components().collect();
    let right_parts: Vec<_> = right.components().collect();

    if left_parts.is_empty() || right_parts.is_empty() {
        return left_parts.is_empty() == right_parts.is_empty() && !left_parts.is_empty();
    }

    let common_len = left_parts.len().min(right_parts.len());
    left_parts[..common_len] == right_parts[..common_len]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolRunnerConfig;

    fn make_tool_call(_id: &str, name: &str, args: &str) -> ToolCallInfo {
        ToolCallInfo {
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    #[test]
    fn single_call_is_not_parallel() {
        let config = ToolRunnerConfig::default();
        let calls = vec![make_tool_call("1", "read", r#"{"path":"/a"}"#)];
        assert!(!should_parallelize(&calls, &config));
    }

    #[test]
    fn never_parallel_tool_forces_sequential() {
        let config = ToolRunnerConfig::default();
        let calls = vec![
            make_tool_call("1", "bash", r#"{"command":"ls"}"#),
            make_tool_call("2", "read", r#"{"path":"/a"}"#),
        ];
        assert!(!should_parallelize(&calls, &config));
    }

    #[test]
    fn path_scoped_tools_with_overlapping_paths_are_sequential() {
        let config = ToolRunnerConfig::default();
        let calls = vec![
            make_tool_call("1", "read", r#"{"path":"/a/b"}"#),
            make_tool_call("2", "write", r#"{"path":"/a/b/c.txt"}"#),
        ];
        assert!(!should_parallelize(&calls, &config));
    }

    #[test]
    fn path_scoped_tools_with_non_overlapping_paths_are_parallel() {
        let config = ToolRunnerConfig::default();
        let calls = vec![
            make_tool_call("1", "read", r#"{"path":"/a/b.txt"}"#),
            make_tool_call("2", "write", r#"{"path":"/x/y.txt"}"#),
        ];
        assert!(should_parallelize(&calls, &config));
    }

    #[test]
    fn path_scoped_tool_with_missing_path_is_sequential() {
        let config = ToolRunnerConfig::default();
        let calls = vec![
            make_tool_call("1", "read", r#"{}"#),
            make_tool_call("2", "read", r#"{"path":"/a"}"#),
        ];
        assert!(!should_parallelize(&calls, &config));
    }

    #[test]
    fn unknown_tool_is_sequential() {
        let config = ToolRunnerConfig::default();
        let calls = vec![
            make_tool_call("1", "my_custom_tool", r#"{}"#),
            make_tool_call("2", "glob", r#"{}"#),
        ];
        assert!(!should_parallelize(&calls, &config));
    }

    #[test]
    fn parallel_safe_tools_are_parallel() {
        let config = ToolRunnerConfig::default();
        let calls = vec![
            make_tool_call("1", "glob", r#"{"pattern":"*.rs"}"#),
            make_tool_call("2", "grep", r#"{"pattern":"fn main"}"#),
        ];
        assert!(should_parallelize(&calls, &config));
    }

    #[test]
    fn mixed_safe_and_path_scoped_non_overlapping_are_parallel() {
        let config = ToolRunnerConfig::default();
        let calls = vec![
            make_tool_call("1", "read", r#"{"path":"/a/b.txt"}"#),
            make_tool_call("2", "glob", r#"{"pattern":"*.rs"}"#),
        ];
        assert!(should_parallelize(&calls, &config));
    }

    #[test]
    fn paths_overlap_same_prefix() {
        assert!(paths_overlap(Path::new("/a/b"), Path::new("/a/b/c")));
    }

    #[test]
    fn paths_overlap_identical() {
        assert!(paths_overlap(Path::new("/a/b"), Path::new("/a/b")));
    }

    #[test]
    fn paths_no_overlap_different() {
        assert!(!paths_overlap(Path::new("/a/b"), Path::new("/x/y")));
    }

    #[test]
    fn paths_no_overlap_sibling_dirs() {
        assert!(!paths_overlap(Path::new("/a/b1"), Path::new("/a/b2")));
    }

    #[test]
    fn extract_path_from_args_with_path() {
        let args = serde_json::json!({"path": "/tmp/test.txt"});
        let result = extract_path_from_args(&args);
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("test.txt"));
    }

    #[test]
    fn extract_path_from_args_empty_path() {
        let args = serde_json::json!({"path": ""});
        assert!(extract_path_from_args(&args).is_none());
    }

    #[test]
    fn extract_path_from_args_no_path_key() {
        let args = serde_json::json!({"command": "ls"});
        assert!(extract_path_from_args(&args).is_none());
    }

    #[test]
    fn extract_path_from_args_path_not_string() {
        let args = serde_json::json!({"path": 42});
        assert!(extract_path_from_args(&args).is_none());
    }

    #[test]
    fn canonicalize_removes_dot() {
        let result = canonicalize_path(Path::new("/a/./b"));
        assert_eq!(result, PathBuf::from("/a/b"));
    }

    #[test]
    fn canonicalize_resolves_dotdot() {
        let result = canonicalize_path(Path::new("/a/b/../c"));
        assert_eq!(result, PathBuf::from("/a/c"));
    }
}
