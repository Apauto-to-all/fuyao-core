//! Agent 注册表（agent_id 列举器）
//!
//! 纯文件夹扫描：扫描全局层与项目层的 `fuyao-agents/` 目录，按文件夹存在性列举
//! 所有 agent_id。不读取任何内容文件，不注入合成 default。
//! 列举元素 [`AgentIdOption`] / [`Source`] 定义于 fuyao-api 的 `selection` 模块。

use fuyao_api::{AgentIdOption, Source, get_workspace_agents_dir};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Agent 注册表
///
/// 扫描全局层和项目层的 `fuyao-agents/` 目录，合并同名 Agent（项目层优先），
/// 列举所有可用的 agent_id。仅做文件夹存在性扫描，不读内容文件、不做 CRUD。
pub struct AgentRegistry {
    /// 工作目录（用于扫描项目层 Agent）
    workspace: Option<PathBuf>,
    /// 全局基准路径（~/.fuyao），构造时注入
    fuyao_home: PathBuf,
}

impl AgentRegistry {
    /// 创建 AgentRegistry
    ///
    /// `workspace` 为 None 时只扫描全局层。
    /// `fuyao_home` 为全局基准路径，扫描 `{fuyao_home}/fuyao-agents/`。
    pub fn new(workspace: Option<PathBuf>, fuyao_home: PathBuf) -> Self {
        Self {
            workspace,
            fuyao_home,
        }
    }

    /// 列举所有 agent_id（扫描 `fuyao-agents/` 文件夹）
    ///
    /// 扫描全局层 `{fuyao_home}/fuyao-agents/` 和项目层 `{ws}/.fuyao/fuyao-agents/`，
    /// 项目层同名覆盖全局层。仅按文件夹存在性列举，不读任何内容文件。
    /// [`AgentIdOption::id`] 为纯文件夹名（不带 `global/` / `workspace/` 前缀），
    /// 来源由 `source` 字段承载；调用方按需自行拼成带前缀的 agent_id。
    /// 不注入合成 default（default 是定义名不是 agent_id；agent_id=None 表示无隔离）。
    /// 结果按 id 升序排序。
    pub fn list_all_ids(&self) -> Vec<AgentIdOption> {
        // key = 文件夹名（纯名），用于项目层覆盖同名全局
        let mut by_name: HashMap<String, AgentIdOption> = HashMap::new();

        // 全局层
        scan_layer(
            &self.fuyao_home.join("fuyao-agents"),
            Source::Global,
            &mut by_name,
        );

        // 项目层（覆盖同名全局）
        if let Some(ref ws) = self.workspace {
            scan_layer(
                &get_workspace_agents_dir(ws),
                Source::Workspace,
                &mut by_name,
            );
        }

        let mut ids: Vec<AgentIdOption> = by_name.into_values().collect();
        ids.sort_by(|a, b| a.id.cmp(&b.id));
        ids
    }
}

/// 扫描单个目录，将发现的子文件夹作为 agent_id 加入 `by_name`
///
/// key 为文件夹名（纯名），用于项目层覆盖同名全局；
/// [`AgentIdOption::id`] 为纯文件夹名，`source` 标记来源层。
/// 仅按 `is_dir()` 判定，目录不存在或读取失败 → 静默跳过。
fn scan_layer(dir: &Path, source: Source, by_name: &mut HashMap<String, AgentIdOption>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // 目录不存在 → 跳过
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        by_name.insert(name.clone(), AgentIdOption { id: name, source });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_all_ids_empty_no_panic() {
        // 无 fuyao-agents/ 目录时不 panic，返回空
        let temp = std::env::temp_dir().join("fuyao_test_registry_ids_empty");
        std::fs::remove_dir_all(&temp).ok();
        std::fs::create_dir_all(&temp).unwrap();

        let registry = AgentRegistry::new(None, temp.clone());
        assert!(registry.list_all_ids().is_empty(), "空目录应返回空列表");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn list_all_ids_global_layer_pure_name() {
        // 全局层文件夹 → id 为纯名（无前缀），source = Global；普通文件被忽略
        let temp = std::env::temp_dir().join("fuyao_test_registry_ids_global");
        std::fs::remove_dir_all(&temp).ok();
        std::fs::create_dir_all(temp.join("fuyao-agents").join("coder")).unwrap();
        std::fs::create_dir_all(temp.join("fuyao-agents").join("reviewer")).unwrap();
        // 普通文件不应被当作 agent_id
        std::fs::write(temp.join("fuyao-agents").join("not-a-dir.txt"), "x").unwrap();

        let registry = AgentRegistry::new(None, temp.clone());
        let ids = registry.list_all_ids();

        assert_eq!(ids.len(), 2, "仅 2 个文件夹应被列举");
        let coder = ids.iter().find(|a| a.id == "coder").expect("应找到 coder");
        assert_eq!(coder.source, Source::Global);
        // 按纯名排序：coder 在 reviewer 前
        assert_eq!(ids[0].id, "coder");
        assert_eq!(ids[1].id, "reviewer");
        // 普通文件不在结果中
        assert!(!ids.iter().any(|a| a.id == "not-a-dir"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn list_all_ids_workspace_overrides_global() {
        // 项目层同名文件夹覆盖全局层 → 只剩该名，source = Workspace
        let temp = std::env::temp_dir().join("fuyao_test_registry_ids_override");
        std::fs::remove_dir_all(&temp).ok();
        std::fs::create_dir_all(temp.join("fuyao-agents").join("coder")).unwrap();

        let ws = temp.join("myproject");
        std::fs::create_dir_all(ws.join(".fuyao").join("fuyao-agents").join("coder")).unwrap();

        let registry = AgentRegistry::new(Some(ws), temp.clone());
        let ids = registry.list_all_ids();

        assert_eq!(ids.len(), 1, "同名应被项目层覆盖去重");
        assert_eq!(ids[0].id, "coder");
        assert_eq!(ids[0].source, Source::Workspace);

        std::fs::remove_dir_all(&temp).ok();
    }
}
