//! 工具公共函数
//!
//! 提供所有工具共享的辅助函数，如路径解析、结果格式化。
//!
//! ## 路径解析规则
//!
//! - 无 path 且有 workspace → 返回 workspace
//! - 绝对路径 → 直接返回
//! - 相对路径 → 基于 workspace 解析（无 workspace 则基于 cwd）
//! - `~` 前缀 → 展开为用户主目录

use std::path::{Path, PathBuf};

/// 解析路径
///
/// 规则：
/// - 无 path 且有 workspace → 返回 workspace
/// - 绝对路径 → 直接返回
/// - 相对路径 → 基于 workspace 解析（无 workspace 则基于 cwd）
pub fn resolve_path(path: &str, workspace: Option<&Path>) -> PathBuf {
    if path.is_empty() {
        if let Some(ws) = workspace {
            return ws.to_path_buf();
        }
        return std::env::current_dir().unwrap_or_default();
    }

    let expanded = expand_tilde(path);

    if expanded.is_absolute() {
        return expanded;
    }

    if let Some(ws) = workspace {
        ws.join(&expanded)
    } else {
        std::env::current_dir().unwrap_or_default().join(&expanded)
    }
}

/// 展开 ~ 为用户主目录
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
        && let Some(home) = dirs_home()
    {
        return if rest.is_empty() {
            home
        } else {
            let rest_trimmed = rest.trim_start_matches(['/', '\\']);
            home.join(rest_trimmed)
        };
    }
    PathBuf::from(path)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_empty_path_returns_workspace() {
        let ws = PathBuf::from("/tmp/project");
        let result = resolve_path("", Some(&ws));
        assert_eq!(result, ws);
    }

    #[test]
    fn resolve_absolute_path_returns_as_is() {
        let abs_path = if cfg!(windows) {
            "C:\\usr\\local\\bin"
        } else {
            "/usr/local/bin"
        };
        let result = resolve_path(abs_path, None);
        assert_eq!(result, PathBuf::from(abs_path));
    }

    #[test]
    fn resolve_relative_path_with_workspace() {
        let ws = PathBuf::from("/tmp/project");
        let result = resolve_path("src/main.rs", Some(&ws));
        assert_eq!(result, PathBuf::from("/tmp/project/src/main.rs"));
    }

    #[test]
    fn resolve_relative_path_without_workspace() {
        let result = resolve_path("src/main.rs", None);
        let expected = std::env::current_dir()
            .unwrap_or_default()
            .join("src/main.rs");
        assert_eq!(result, expected);
    }

    #[test]
    fn expand_tilde_basic() {
        let result = expand_tilde("~/test");
        assert!(result.is_absolute());
        assert!(result.to_string_lossy().ends_with("test"));
    }
}
