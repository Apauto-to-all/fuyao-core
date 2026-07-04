//! 全局层路径
//!
//! 全局层目录架构：
//! - Fuyao home: `~/.fuyao/`（可通过 `FUYAO_HOME` 环境变量覆盖）
//! - Agents 目录: `~/.fuyao/fuyao-agents/`

use std::path::PathBuf;

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

/// 返回 fuyao-agents 目录路径
///
/// 路径: `~/.fuyao/fuyao-agents/`
pub fn get_fuyao_agents_dir() -> PathBuf {
    get_fuyao_home().join("fuyao-agents")
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
}
