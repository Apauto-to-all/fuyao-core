//! 环境变量加载模块
//!
//! 从三层路径加载 .env 文件到进程环境变量。
//!
//! 加载顺序（低优先级 → 高优先级）：
//! 1. 全局路径：~/.fuyao/.env
//! 2. Agent 路径：{agent_root}/.env
//! 3. 工作区路径：{workspace}/.env
//!
//! 高优先级覆盖低优先级同名变量。

use fuyao_api::AgentPaths;

/// 从三层路径加载 .env 文件
///
/// 对应原 env_loader 插件逻辑，作为配置初始化的一部分调用。
/// 使用 dotenvy 解析 .env 文件，支持变量替换、引号等标准格式。
pub fn load_env(agent_paths: &AgentPaths) {
    let paths = agent_paths.env_paths();
    // all() 返回 [workspace, agent, global]，反转后从 global 开始（低优先级→高优先级）
    for path in paths.all().into_iter().rev() {
        // dotenvy::from_path_override 会解析 .env 并设置到 std::env，
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
        load_env(&paths);
    }
}
