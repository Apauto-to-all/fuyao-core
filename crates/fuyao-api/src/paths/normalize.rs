//! 工作目录路径归一化
//!
//! 把工作目录路径统一为正斜杠 `/` 形态，用于持久化存储与跨平台比对。
//! 入参为 `Option<PathBuf>`（core 内部数据流持有该形态），
//! 所有路径归一化的真相源集中在此模块。

use std::path::PathBuf;

/// 把工作目录路径归一化为持久化形态：统一分隔符为正斜杠 `/`
///
/// Windows 下 `current_dir()` 返回反斜杠路径（如 `C:\a\b`），直接 `to_string_lossy`
/// 存入 DB 会引入反斜杠（escape 字符，跨平台展示 / 日志归一化时处理麻烦）。
/// 统一换成正斜杠后，同一项目无论在 Windows 还是 Unix 下，workspace 字符串形态一致，
/// 按项目过滤（`workspace = ?`）的匹配结果稳定。
///
/// 不做 `canonicalize`（解析符号链接 / 要求路径存在）——那会改变用户对路径的预期、
/// 且路径不存在时会失败，对「展示 + 按项目过滤」不友好。
pub fn normalize_workspace(workspace: &Option<PathBuf>) -> Option<String> {
    workspace
        .as_ref()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_版_workspace_为_none_时返_none() {
        assert_eq!(normalize_workspace(&None), None);
    }

    #[test]
    fn option_版_反斜杠统一为正斜杠() {
        // Windows 反斜杠路径 → 统一为正斜杠（跨平台形态一致）
        let ws = Some(PathBuf::from(r"C:\Users\TF\proj"));
        assert_eq!(
            normalize_workspace(&ws).as_deref(),
            Some("C:/Users/TF/proj")
        );
    }

    #[test]
    fn option_版_正斜杠原样保留() {
        let ws = Some(PathBuf::from("/home/u/proj"));
        assert_eq!(normalize_workspace(&ws).as_deref(), Some("/home/u/proj"));
    }

    #[test]
    fn option_版混用分隔符统一为正斜杠() {
        let ws = Some(PathBuf::from(r"C:\a/b\c"));
        assert_eq!(normalize_workspace(&ws).as_deref(), Some("C:/a/b/c"));
    }
}
