//! 工作目录路径归一化
//!
//! 把工作目录路径统一为正斜杠 `/` 形态，用于持久化存储与跨平台比对。
//! 两种入参形态：
//! - `Option<PathBuf>`（core 内部数据流持有该形态）走 [`normalize_workspace`]
//! - `&str`（消费方从 IPC 等渠道收到裸字符串）走 [`normalize_workspace_str`]
//!
//! 两个函数同一归一逻辑，只是适配不同入参类型——所有路径归一化的真相源集中在此模块。

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

/// 把 `&str` 工作目录路径归一为正斜杠形态，返回 `String`
///
/// 与 [`normalize_workspace`] 同一归一逻辑，面向字符串入参的消费方。内部委托
/// [`normalize_workspace`]，保证两个入口永不漂移。
///
/// 消费方是外部二次开发应用（经 path 依赖消费本 crate 的下游终端产品）——它们在
/// IPC / 配置等边界拿到的是裸字符串，经本入口归一。因此本仓库 workspace 内 grep
/// 不到调用点属预期：「仓内零调用」不构成死代码判据，本函数是跨仓库 SDK 公开
/// 表面的一部分。
///
/// 空入参返空串——调用方负责非空校验（写入侧应做边界校验，空路径不应到达此处）。
pub fn normalize_workspace_str(workspace: &str) -> String {
    normalize_workspace(&Some(PathBuf::from(workspace))).unwrap_or_default()
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
        let ws = Some(PathBuf::from(r"C:\Users\alice\proj"));
        assert_eq!(
            normalize_workspace(&ws).as_deref(),
            Some("C:/Users/alice/proj")
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

    #[test]
    fn str_版与_option_版结果一致() {
        // 同一路径，&str 入参与 Option<PathBuf> 入参归一结果必须一致
        let path = r"C:\Users\alice\proj";
        let via_opt = normalize_workspace(&Some(PathBuf::from(path)));
        let via_str = normalize_workspace_str(path);
        assert_eq!(via_str, via_opt.unwrap_or_default());
    }

    #[test]
    fn str_版空入参返空串() {
        assert_eq!(normalize_workspace_str(""), "");
    }

    #[test]
    fn str_版混用分隔符统一为正斜杠() {
        assert_eq!(normalize_workspace_str(r"C:\a/b\c"), "C:/a/b/c");
    }
}
