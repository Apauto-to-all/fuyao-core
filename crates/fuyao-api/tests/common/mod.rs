//! 集成测试共享工具
//!
//! 跨测试二进制复用的 fixture 构造与文件写入辅助。
//! 仅放真实被多处使用的工具；不为「可能复用」提前抽象。

use std::path::{Path, PathBuf};

use fuyao_api::AgentPaths;

/// 构造可注入 `fuyao_home` 的 `AgentPaths`，绕开环境变量，实现 per-test 隔离。
///
/// `agent_id` 与 `workspace` 均可选，覆盖典型组合（裸名 / global/{名} / workspace/{名}）。
/// `extra_dirs` 留空，需要时由调用方在返回值上追加。
#[allow(dead_code)]
pub fn make_agent_paths(
    fuyao_home: PathBuf,
    agent_id: Option<&str>,
    workspace: Option<PathBuf>,
) -> AgentPaths {
    AgentPaths {
        agent_id: agent_id.map(str::to_string),
        workspace,
        extra_dirs: Vec::new(),
        fuyao_home,
    }
}

/// 在 `dir` 下写入名为 `fuyao.toml` 的配置文件，返回其路径。
#[allow(dead_code)]
pub fn write_config_file(dir: &Path, content: &str) -> PathBuf {
    write_file(dir, "fuyao.toml", content)
}

/// 在 `dir` 下写入名为 `.env` 的环境变量文件，返回其路径。
#[allow(dead_code)]
pub fn write_env_file(dir: &Path, content: &str) -> PathBuf {
    write_file(dir, ".env", content)
}

/// 在 `dir` 下写入任意文件，返回其完整路径。
fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
    use std::io::Write;

    let path = dir.join(name);
    let mut file = std::fs::File::create(&path)
        .unwrap_or_else(|e| panic!("创建 {name} 失败：{e}（dir={dir:?}）"));
    file.write_all(content.as_bytes())
        .unwrap_or_else(|e| panic!("写入 {name} 失败：{e}"));
    path
}
