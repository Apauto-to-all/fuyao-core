//! fuyao-prompt 集成测试共享 fixture
//!
//! 所有 fixture 基于 tempfile::TempDir 字段注入 AgentPaths / AgentRegistry，
//! 零环境变量依赖，实现 per-test 隔离（规避 FUYAO_HOME 串扰）。

// 跨测试二进制共享：不同 tests/*.rs 只用本模块的子集，未用部分不报 dead_code
#![allow(dead_code)]

use std::path::PathBuf;
use tempfile::TempDir;

/// 构造可注入 fuyao_home 的 AgentPaths（零环境变量依赖）
///
/// `home` 作为全局基准；`workspace` 为工作目录（None 时仅全局层）；
/// `extra_dirs` 用于注入插件根目录（agents/、instructions/、skills/）。
pub fn make_agent_paths(
    home: PathBuf,
    workspace: Option<PathBuf>,
    extra_dirs: Vec<PathBuf>,
) -> fuyao_api::AgentPaths {
    fuyao_api::AgentPaths {
        agent_id: None,
        workspace,
        extra_dirs,
        fuyao_home: home,
    }
}

/// 创建临时 fuyao_home 目录（含 TempDir 句柄，退出自动清理）
pub fn temp_home() -> TempDir {
    tempfile::tempdir().expect("创建临时 fuyao_home 失败")
}

/// 在指定 base 下创建 agents/{name}.md 定义文件
///
/// frontmatter 由调用方提供完整内容（含 --- 分隔），body 为正文。
pub fn write_agent_def(base: &std::path::Path, name: &str, md: &str) {
    let agents_dir = base.join("agents");
    std::fs::create_dir_all(&agents_dir).expect("创建 agents/ 目录失败");
    std::fs::write(agents_dir.join(format!("{name}.md")), md).expect("写入 agent 定义失败");
}

/// 在 fuyao_home 下创建 agents/default.md
pub fn write_default_agent(home: &std::path::Path, md: &str) {
    write_agent_def(home, "default", md);
}

/// 在 base 目录下创建 instructions/{file} 补充指令文件
pub fn write_instruction(base: &std::path::Path, file: &str, content: &str) {
    let instr_dir = base.join("instructions");
    std::fs::create_dir_all(&instr_dir).expect("创建 instructions/ 目录失败");
    std::fs::write(instr_dir.join(file), content).expect("写入补充指令失败");
}

/// 在 base 目录下创建 AGENTS.md 项目上下文文件
pub fn write_agents_md(base: &std::path::Path, content: &str) {
    std::fs::write(base.join("AGENTS.md"), content).expect("写入 AGENTS.md 失败");
}
