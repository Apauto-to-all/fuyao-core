//! 分层路径结构
//!
//! 用于表示三层目录架构中的路径集合。

use std::path::{Path, PathBuf};

/// 分层路径结构（带标签区分来源）
///
/// 用于返回多层路径，每个路径都有明确的标签标识其来源层级。
#[derive(Debug, Clone, Default)]
pub struct LayeredPaths {
    /// 全局层路径（~/.fuyao/ 下）
    pub global_: Option<PathBuf>,
    /// Agent 目录层路径（~/.fuyao/fuyao-agents/{agent-id}/ 下）
    pub agent: Option<PathBuf>,
    /// 工作目录层路径（{workspace}/.fuyao/ 下）
    pub workspace: Option<PathBuf>,
}

impl LayeredPaths {
    /// 返回所有路径（按优先级排序，用于遍历）
    ///
    /// 优先级：工作目录 → Agent 目录 → 全局
    pub fn all(&self) -> Vec<&Path> {
        let mut paths = Vec::with_capacity(3);
        if let Some(ref p) = self.workspace {
            paths.push(p.as_path());
        }
        if let Some(ref p) = self.agent {
            paths.push(p.as_path());
        }
        if let Some(ref p) = self.global_ {
            paths.push(p.as_path());
        }
        paths
    }

    /// 返回第一个存在的路径（用于查找文件）
    ///
    /// 按优先级顺序查找，返回第一个实际存在的路径。
    pub fn first_exists(&self) -> Option<PathBuf> {
        self.all()
            .into_iter()
            .find(|p| p.exists())
            .map(Path::to_path_buf)
    }

    /// 返回所有存在的路径（用于叠加加载）
    ///
    /// 返回所有实际存在的路径，按优先级排序。
    /// 适用于需要合并多层资源的场景（如 Skills）。
    pub fn merge_exists(&self) -> Vec<PathBuf> {
        self.all()
            .into_iter()
            .filter(|p| p.exists())
            .map(Path::to_path_buf)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layered_paths_all_returns_priority_order() {
        let paths = LayeredPaths {
            global_: Some(PathBuf::from("/global")),
            agent: Some(PathBuf::from("/agent")),
            workspace: Some(PathBuf::from("/workspace")),
        };
        let all = paths.all();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], Path::new("/workspace"));
        assert_eq!(all[1], Path::new("/agent"));
        assert_eq!(all[2], Path::new("/global"));
    }

    #[test]
    fn layered_paths_all_skips_none() {
        let paths = LayeredPaths {
            global_: Some(PathBuf::from("/global")),
            agent: None,
            workspace: None,
        };
        let all = paths.all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], Path::new("/global"));
    }

    #[test]
    fn layered_paths_all_empty() {
        let paths = LayeredPaths::default();
        assert!(paths.all().is_empty());
    }

    #[test]
    fn layered_paths_first_exists_returns_existing() {
        let temp = std::env::temp_dir().join("fuyao_test_layered_first");
        std::fs::create_dir_all(&temp).unwrap();

        let paths = LayeredPaths {
            global_: Some(temp.clone()),
            agent: Some(PathBuf::from("/nonexistent_agent")),
            workspace: Some(PathBuf::from("/nonexistent_ws")),
        };
        assert_eq!(paths.first_exists(), Some(temp.clone()));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn layered_paths_merge_exists_returns_all_existing() {
        let temp = std::env::temp_dir().join("fuyao_test_layered_merge");
        let sub1 = temp.join("sub1");
        let sub2 = temp.join("sub2");
        std::fs::create_dir_all(&sub1).unwrap();
        std::fs::create_dir_all(&sub2).unwrap();

        let paths = LayeredPaths {
            global_: Some(sub1.clone()),
            agent: Some(PathBuf::from("/nonexistent")),
            workspace: Some(sub2.clone()),
        };
        let existing = paths.merge_exists();
        assert_eq!(existing.len(), 2);
        assert!(existing.contains(&sub1));
        assert!(existing.contains(&sub2));

        std::fs::remove_dir_all(&temp).ok();
    }
}
