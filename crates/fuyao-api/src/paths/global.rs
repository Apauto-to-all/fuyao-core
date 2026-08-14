//! 全局层路径
//!
//! 全局层目录架构：
//! - Fuyao home: `~/.fuyao/`（可通过 `FUYAO_HOME` 环境变量覆盖）
//! - Agents 目录: `~/.fuyao/fuyao-agents/`

use std::path::{Path, PathBuf};

/// 返回 Fuyao home 目录
///
/// 默认: `~/.fuyao`
/// 可通过 `FUYAO_HOME` 环境变量覆盖
pub fn get_fuyao_home() -> PathBuf {
    if let Ok(home) = std::env::var("FUYAO_HOME") {
        let p = PathBuf::from(&home);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    dirs_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".fuyao")
}

/// 返回全局层 fuyao-agents 目录路径
///
/// 路径: `{fuyao_home}/fuyao-agents/`（agent_id 为 global 来源时的数据根）。
/// 全局层 agent 布局的单一事实来源：所有需要该目录的消费点（agent_root 定位、
/// agent_id 列举扫描）都必须经本函数拼路径，禁止散写 `"fuyao-agents"` 字面量，
/// 布局变更只改此处。`fuyao_home` 由调用方注入（读 `AgentPaths.fuyao_home` 字段），
/// 路径解析为纯函数、零全局状态。
pub fn get_fuyao_agents_dir(fuyao_home: &Path) -> PathBuf {
    fuyao_home.join("fuyao-agents")
}

/// 返回用户 home 目录
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
    fn get_fuyao_home_default() {
        unsafe { std::env::remove_var("FUYAO_HOME") };
        let home = get_fuyao_home();
        assert!(home.to_string_lossy().contains(".fuyao"));
    }

    #[test]
    fn get_fuyao_home_with_env_override() {
        let temp = std::env::temp_dir().join("fuyao_test_home_override");
        unsafe { std::env::set_var("FUYAO_HOME", &temp) };
        let home = get_fuyao_home();
        assert_eq!(home, temp);
        unsafe { std::env::remove_var("FUYAO_HOME") };
    }

    /// 全局层 agents 目录 = 注入 home 下的 fuyao-agents 子目录（纯函数）
    #[test]
    fn get_fuyao_agents_dir_joins_injected_home() {
        let dir = get_fuyao_agents_dir(Path::new("/tmp/home"));
        assert_eq!(dir, PathBuf::from("/tmp/home/fuyao-agents"));
    }
}
