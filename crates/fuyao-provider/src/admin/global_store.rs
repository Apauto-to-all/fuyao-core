//! global 层写回落存储句柄：两落点的文件定位与读写
//!
//! - `fuyao.toml`：读为可编辑文档（toml_edit，承载原文样式信息）
//! - `.env`：读为全文文本（单行级改写由 [`super::env_file`] 承接）
//!
//! 文件不存在时读出空文档 / 空串（首次写回时由写方法新建）；写入前逐级
//! 创建父目录，避免首次写回因目录缺失失败。

use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;

use super::error::ProviderAdminError;

/// global 层写回落存储句柄：定位 fuyao.toml 与 .env 两个落点
pub struct GlobalStore {
    /// global 层配置文件路径（`{fuyao_home}/fuyao.toml`）
    toml_path: PathBuf,
    /// global 层环境变量文件路径（`{fuyao_home}/.env`）
    env_path: PathBuf,
}

impl GlobalStore {
    /// 以 fuyao_home 为基准构造存储句柄
    pub fn new(fuyao_home: &Path) -> Self {
        Self {
            toml_path: fuyao_home.join("fuyao.toml"),
            env_path: fuyao_home.join(".env"),
        }
    }

    /// 读 global 层 fuyao.toml 为可编辑文档
    ///
    /// 文件不存在返回空文档（首次写回时由 [`GlobalStore::write_toml`] 新建）；
    /// 内容非法 TOML 返回 [`ProviderAdminError::TomlParse`]（带解析错误信息）。
    pub fn read_toml(&self) -> Result<DocumentMut, ProviderAdminError> {
        match std::fs::read_to_string(&self.toml_path) {
            Ok(content) => content
                .parse::<DocumentMut>()
                .map_err(|e| ProviderAdminError::TomlParse(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
            Err(e) => Err(ProviderAdminError::Io(e.to_string())),
        }
    }

    /// 把文档落盘（父目录不存在则创建；toml_edit 渲染保留未触碰部分的原文样式）
    pub fn write_toml(&self, doc: &DocumentMut) -> Result<(), ProviderAdminError> {
        write_file(&self.toml_path, &doc.to_string())
    }

    /// 读 global 层 .env 全文（文件不存在返回空串）
    pub fn read_env(&self) -> Result<String, ProviderAdminError> {
        match std::fs::read_to_string(&self.env_path) {
            Ok(content) => Ok(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(ProviderAdminError::Io(e.to_string())),
        }
    }

    /// 把 .env 内容落盘（父目录不存在则创建）
    pub fn write_env(&self, content: &str) -> Result<(), ProviderAdminError> {
        write_file(&self.env_path, content)
    }
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

    /// toml 读写往返：不存在时读出空文档，写入后读回一致
    #[test]
    fn read_write_toml_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let store = GlobalStore::new(temp.path());

        // 不存在 → 空文档
        assert!(store.read_toml().unwrap().is_empty());

        let mut doc: DocumentMut = "[providers.demo]\nname = \"x\"\n".parse().unwrap();
        doc.as_table_mut()
            .get_mut("providers")
            .unwrap()
            .as_table_mut()
            .unwrap()
            .insert("extra", toml_edit::value("y"));
        store.write_toml(&doc).unwrap();

        let back = store.read_toml().unwrap().to_string();
        assert!(back.contains("[providers.demo]"), "写盘后可读回：{back}");
        assert!(back.contains("extra"), "新增键落盘：{back}");
    }

    /// .env 读写：不存在时读出空串，父目录缺失时写入自动创建
    #[test]
    fn read_write_env_creates_parent_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let store = GlobalStore::new(temp.path());

        assert_eq!(store.read_env().unwrap(), "");
        store.write_env("K=v\n").unwrap();
        assert_eq!(store.read_env().unwrap(), "K=v\n");
    }
}
