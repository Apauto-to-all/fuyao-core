//! Skills 集成测试共享 fixture
//!
//! 构造可注入 fuyao_home / workspace / extra_dirs 的 AgentPaths，
//! 以及在指定 skills 目录下创建 SKILL.md + 关联文件的辅助函数。

use std::path::{Path, PathBuf};

use fuyao_api::AgentPaths;

/// 构造可注入路径的 AgentPaths，绕开环境变量实现 per-test 隔离。
///
/// `fuyao_home` 与 `workspace` 均可注入，覆盖三层（global/agent/workspace）+ extra 组合。
#[allow(dead_code)]
pub fn make_paths(
    fuyao_home: PathBuf,
    workspace: Option<PathBuf>,
    extra_dirs: Vec<PathBuf>,
) -> AgentPaths {
    AgentPaths {
        agent_id: None,
        workspace,
        extra_dirs,
        fuyao_home,
    }
}

/// 在 `skills_root` 下创建一个带 frontmatter 的 SKILL.md，返回 skill 目录路径。
///
/// `body` 为正文（frontmatter 之后的 markdown）；若为 None 则只写 frontmatter。
#[allow(dead_code)]
pub fn write_skill_md(
    skills_root: &Path,
    name: &str,
    frontmatter: &str,
    body: Option<&str>,
) -> PathBuf {
    let skill_dir = skills_root.join(name);
    std::fs::create_dir_all(&skill_dir).unwrap_or_else(|e| panic!("创建 skill 目录失败：{e}"));
    let content = match body {
        Some(b) => format!("---\n{frontmatter}\n---\n{b}"),
        None => format!("---\n{frontmatter}\n---\n"),
    };
    let skill_md = skill_dir.join("SKILL.md");
    std::fs::write(&skill_md, content).unwrap_or_else(|e| panic!("写 SKILL.md 失败：{e}"));
    skill_dir
}

/// 在 skill 目录下创建关联子目录文件（scripts/references/assets）。
/// `subdir` 必须是 LINKED_SUBDIRS 之一。
#[allow(dead_code)]
pub fn write_linked_file(skill_dir: &Path, subdir: &str, file_name: &str, content: &str) {
    let dir = skill_dir.join(subdir);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("创建 {subdir} 失败：{e}"));
    std::fs::write(dir.join(file_name), content).unwrap_or_else(|e| panic!("写文件失败：{e}"));
}
