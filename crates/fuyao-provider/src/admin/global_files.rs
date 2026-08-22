//! global 层两落点（fuyao.toml / .env）的文件读写函数
//!
//! 落点经 [`AgentPaths`] 的分层路径方法解析（`config_paths` / `env_paths` 的
//! global 层），不自行拼接路径。
//!
//! - `fuyao.toml`：读为可编辑文档（toml_edit，承载原文样式信息）
//! - `.env`：读为全文文本（单行级改写由 [`super::env_file`] 承接）
//!
//! 文件不存在时读出空文档 / 空串（首次写回时由写函数新建）；写入前逐级
//! 创建父目录，避免首次写回因目录缺失失败。

use std::path::{Path, PathBuf};

use fuyao_api::AgentPaths;
use toml_edit::DocumentMut;

use super::error::ProviderAdminError;

/// global 层 fuyao.toml 路径（[`AgentPaths::config_paths`] 的 global 层）
fn toml_path(agent_paths: &AgentPaths) -> PathBuf {
    agent_paths
        .config_paths()
        .global_
        .expect("config_paths 的 global 层恒存在")
}

/// global 层 .env 路径（[`AgentPaths::env_paths`] 的 global 层）
fn env_path(agent_paths: &AgentPaths) -> PathBuf {
    agent_paths
        .env_paths()
        .global_
        .expect("env_paths 的 global 层恒存在")
}

/// 读 global 层 fuyao.toml 为可编辑文档
///
/// 文件不存在返回空文档（首次写回时由 [`write_global_toml`] 新建）；
/// 内容非法 TOML 返回 [`ProviderAdminError::TomlParse`]（带解析错误信息）。
pub fn read_global_toml(agent_paths: &AgentPaths) -> Result<DocumentMut, ProviderAdminError> {
    match std::fs::read_to_string(toml_path(agent_paths)) {
        Ok(content) => content
            .parse::<DocumentMut>()
            .map_err(|e| ProviderAdminError::TomlParse(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(ProviderAdminError::Io(e.to_string())),
    }
}

/// 把文档落盘 global 层 fuyao.toml（父目录不存在则创建；toml_edit 渲染保留
/// 未触碰部分的原文样式）
pub fn write_global_toml(
    agent_paths: &AgentPaths,
    doc: &DocumentMut,
) -> Result<(), ProviderAdminError> {
    write_file(&toml_path(agent_paths), &doc.to_string())
}

/// 读 global 层 .env 全文（文件不存在返回空串）
pub fn read_global_env(agent_paths: &AgentPaths) -> Result<String, ProviderAdminError> {
    match std::fs::read_to_string(env_path(agent_paths)) {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(ProviderAdminError::Io(e.to_string())),
    }
}

/// 把 .env 内容落盘 global 层（父目录不存在则创建）
pub fn write_global_env(agent_paths: &AgentPaths, content: &str) -> Result<(), ProviderAdminError> {
    write_file(&env_path(agent_paths), content)
}

/// 写文件：父目录不存在则逐级创建，避免首次写回因目录缺失失败
fn write_file(path: &Path, content: &str) -> Result<(), ProviderAdminError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ProviderAdminError::Io(e.to_string()))?;
    }
    std::fs::write(path, content).map_err(|e| ProviderAdminError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造仅 global 层的 AgentPaths（fuyao_home 注入临时目录）
    fn global_paths(home: &Path) -> AgentPaths {
        AgentPaths {
            fuyao_home: home.to_path_buf(),
            ..AgentPaths::default()
        }
    }

    /// toml 读写往返：不存在时读出空文档，写入后读回一致
    #[test]
    fn read_write_toml_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let paths = global_paths(temp.path());

        // 不存在 → 空文档
        assert!(read_global_toml(&paths).unwrap().is_empty());

        let mut doc: DocumentMut = "[providers.demo]\nname = \"x\"\n".parse().unwrap();
        doc.as_table_mut()
            .get_mut("providers")
            .unwrap()
            .as_table_mut()
            .unwrap()
            .insert("extra", toml_edit::value("y"));
        write_global_toml(&paths, &doc).unwrap();

        let back = read_global_toml(&paths).unwrap().to_string();
        assert!(back.contains("[providers.demo]"), "写盘后可读回：{back}");
        assert!(back.contains("extra"), "新增键落盘：{back}");
    }

    /// .env 读写：不存在时读出空串，写入后读回一致
    #[test]
    fn read_write_env_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let paths = global_paths(temp.path());

        assert_eq!(read_global_env(&paths).unwrap(), "");
        write_global_env(&paths, "K=v\n").unwrap();
        assert_eq!(read_global_env(&paths).unwrap(), "K=v\n");
    }

    /// 落点与 AgentPaths 分层路径体系一致（global 层 fuyao.toml / .env）
    #[test]
    fn paths_match_layered_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let paths = global_paths(temp.path());

        assert_eq!(
            toml_path(&paths),
            temp.path().join("fuyao.toml"),
            "toml 落点 = config_paths 的 global 层"
        );
        assert_eq!(
            env_path(&paths),
            temp.path().join(".env"),
            "env 落点 = env_paths 的 global 层"
        );
    }
}
