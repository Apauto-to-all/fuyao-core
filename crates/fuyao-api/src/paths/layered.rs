//! 分层路径结构
//!
//! 用于表示三层目录架构中的路径集合。

use std::path::{Path, PathBuf};

/// 分层路径结构（带标签区分来源）
///
/// 用于返回多层路径，每个路径都有明确的标签标识其来源层级。
///
/// 是 [`crate::AgentPaths`] 八个公开方法的返回类型（`config_paths` / `skills_paths` /
/// `agents_def_paths` 等）——四层解析的公开原语，消费方持有方法返回值调
/// [`all`](Self::all) / [`merge_exists`](Self::merge_exists) 遍历，不按名导入本类型
/// （全仓按名引用为零是预期形态）。类型必须保持公开，不可降为 crate 私有。
#[derive(Debug, Clone, Default)]
pub struct LayeredPaths {
    /// 全局层路径（~/.fuyao/ 下）
    pub global_: Option<PathBuf>,
    /// Agent 目录层路径（~/.fuyao/fuyao-agents/{agent-id}/ 下）
    pub agent: Option<PathBuf>,
    /// 工作目录层路径（{workspace}/.fuyao/ 下）
    pub workspace: Option<PathBuf>,
    /// 额外目录（插件等），最低优先级
    pub extra: Vec<PathBuf>,
}

impl LayeredPaths {
    /// 返回所有路径（按优先级排序，用于遍历）
    ///
    /// 优先级：工作目录 → Agent 目录 → 全局 → 额外目录（extra，最低）
    pub fn all(&self) -> Vec<&Path> {
        let mut paths = Vec::with_capacity(3 + self.extra.len());
        if let Some(ref p) = self.workspace {
            paths.push(p.as_path());
        }
        if let Some(ref p) = self.agent {
            paths.push(p.as_path());
        }
        if let Some(ref p) = self.global_ {
            paths.push(p.as_path());
        }
        // 额外目录（插件等）最低优先级，放在最后
        for p in &self.extra {
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        };
        let existing = paths.merge_exists();
        assert_eq!(existing.len(), 2);
        assert!(existing.contains(&sub1));
        assert!(existing.contains(&sub2));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn layered_paths_all_includes_extra_lowest_priority() {
        let paths = LayeredPaths {
            global_: Some(PathBuf::from("/global")),
            agent: Some(PathBuf::from("/agent")),
            workspace: Some(PathBuf::from("/workspace")),
            extra: vec![PathBuf::from("/plugin1"), PathBuf::from("/plugin2")],
        };
        let all = paths.all();
        assert_eq!(all.len(), 5);
        // 优先级：workspace > agent > global > extra（最低）
        assert_eq!(all[0], Path::new("/workspace"));
        assert_eq!(all[1], Path::new("/agent"));
        assert_eq!(all[2], Path::new("/global"));
        assert_eq!(all[3], Path::new("/plugin1"));
        assert_eq!(all[4], Path::new("/plugin2"));
    }

    #[test]
    fn layered_paths_merge_exists_includes_extra() {
        let temp = std::env::temp_dir().join("fuyao_test_layered_extra_merge");
        let extra_dir = temp.join("plugin_skills");
        std::fs::create_dir_all(&extra_dir).unwrap();

        let paths = LayeredPaths {
            global_: Some(PathBuf::from("/nonexistent_global")),
            agent: None,
            workspace: None,
            extra: vec![extra_dir.clone()],
        };
        let existing = paths.merge_exists();
        // 三层均不存在，只有 extra 存在
        assert_eq!(existing.len(), 1);
        assert!(existing.contains(&extra_dir));

        std::fs::remove_dir_all(&temp).ok();
    }
}
