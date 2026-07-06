//! 环境变量加载
//!
//! 从三层路径加载 `.env` 文件到进程环境变量。与 TOML 配置的三层合并并行，
//! 同属配置加载范畴，故收入 config 模块作为单一加载入口的一部分。
//!
//! 加载顺序（低优先级 → 高优先级）：
//! 1. 全局路径：`~/.fuyao/.env`
//! 2. Agent 路径：`{agent_root}/.env`
//! 3. 工作区路径：`{workspace}/.env`
//!
//! 高优先级覆盖低优先级同名变量。

use crate::AgentPaths;

/// 从三层路径加载 `.env` 文件
///
/// 使用 `dotenvy` 解析 `.env` 文件，支持变量替换、引号等标准格式。
/// 任一层文件不存在时静默跳过，不报错。
pub fn load_env(agent_paths: &AgentPaths) {
    let paths = agent_paths.env_paths();
    // all() 返回 [workspace, agent, global]，反转后从 global 开始（低优先级→高优先级）
    for path in paths.all().into_iter().rev() {
        // dotenvy::from_path_override 解析 .env 并设置到 std::env，
        // 已存在的变量会被覆盖（高优先级覆盖低优先级）
        let _ = dotenvy::from_path_override(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_env_nonexistent_paths_no_panic() {
        let paths = AgentPaths {
            agent_id: Some("nonexistent_test".to_string()),
            workspace: None,
        };
        // 全部不存在的路径不应 panic
        load_env(&paths);
    }
}
