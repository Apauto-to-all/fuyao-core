//! Skills 查找
//!
//! 扫描分层目录发现 Skill（含插件 extra），返回元数据列表（Tier 1）。
//! 使用 ignore crate 进行高性能遍历，排除特定目录。

use crate::error::SkillsError;
use crate::helpers::{find_first_non_heading, parse_skill_frontmatter};
use fuyao_api::AgentPaths;
use fuyao_api::SkillMeta;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// 排除的目录（扫描 Skills 时跳过）
const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".github",
    ".venv",
    "venv",
    ".env",
    "node_modules",
    "__pycache__",
    "dist",
    "build",
];

/// 在分层目录中按名称查找 SKILL.md
///
/// 搜索顺序：workspace → agent → global → extra（高优先级优先）。
///
/// 返回 `(skill_dir, skill_md_path)` 元组，未找到均为 `None`。
pub fn find_skill_md_by_name(name: &str, ctx: &AgentPaths) -> (Option<PathBuf>, Option<PathBuf>) {
    let skills_dirs = ctx.skills_paths().merge_exists();
    if skills_dirs.is_empty() {
        return (None, None);
    }

    for search_dir in &skills_dirs {
        // 直接匹配目录名
        let direct_path = search_dir.join(name);
        let skill_md = direct_path.join("SKILL.md");
        if direct_path.is_dir() && skill_md.exists() {
            return (Some(direct_path), Some(skill_md));
        }

        // 递归搜索所有 SKILL.md
        for found_md in walk_skill_mds(search_dir) {
            if found_md
                .parent()
                .map(|p| {
                    p.file_name()
                        .map(|f| f.to_string_lossy().as_ref() == name)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
            {
                let skill_dir = found_md.parent().unwrap().to_path_buf();
                return (Some(skill_dir), Some(found_md));
            }
        }
    }

    (None, None)
}

/// 扫描分层目录（含插件 extra），发现所有可用 Skill（Tier 1：元数据）
pub fn find_all_skills(ctx: &AgentPaths) -> Result<Vec<SkillMeta>, SkillsError> {
    let skills_dirs = ctx.skills_paths().merge_exists();
    if skills_dirs.is_empty() {
        return Ok(Vec::new());
    }

    let mut skills = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    for skills_dir in &skills_dirs {
        for skill_md in walk_skill_mds(skills_dir) {
            let skill_dir = match skill_md.parent() {
                Some(p) => p.to_path_buf(),
                None => continue,
            };

            // 读取前 4000 字节解析 frontmatter
            //
            // 字节切片必须落在 UTF-8 字符边界上，否则 panic。中文等字符占 3 字节，
            // 直接 `c[..4000]` 在 4000 落到多字节字符中间时 panic。用 floor_char_boundary
            // 把 4000 回退到最近的字符起始边界，取一个不超 4000 字节的安全前缀。
            let content = match std::fs::read_to_string(&skill_md) {
                Ok(c) => {
                    if c.len() > 4000 {
                        let end = c.floor_char_boundary(4000);
                        c[..end].to_string()
                    } else {
                        c
                    }
                }
                Err(_) => continue,
            };

            let parsed = parse_skill_frontmatter(&content);

            // name fallback：frontmatter → 目录名
            let name = if parsed.name.is_empty() {
                skill_dir
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default()
            } else {
                parsed.name
            };

            if name.is_empty() || seen_names.contains(&name) {
                continue;
            }

            // description fallback：frontmatter → 正文首行非标题
            let description = if parsed.description.is_empty() {
                find_first_non_heading(&parsed.body)
            } else {
                parsed.description
            };

            seen_names.insert(name.clone());
            skills.push(SkillMeta::new(name, description));
        }
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

/// 递归搜索目录下的所有 SKILL.md 文件
///
/// 使用 ignore crate 进行高性能遍历，排除隐藏目录（如 .git），
/// 同时排除 EXCLUDED_DIRS 中定义的目录，不遵循 .gitignore 规则。
fn walk_skill_mds(base: &Path) -> Vec<PathBuf> {
    WalkBuilder::new(base)
        .hidden(true)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| {
            // 排除 EXCLUDED_DIRS 中的目录
            if let Some(parent) = e.path().parent() {
                let dir_name = parent.file_name().unwrap_or_default().to_string_lossy();
                if EXCLUDED_DIRS.contains(&dir_name.as_ref()) {
                    return false;
                }
            }
            true
        })
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| e.file_name() == "SKILL.md")
        .map(|e| e.path().to_path_buf())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_skill_mds_finds_skill_files() {
        let dir = std::env::temp_dir().join("fuyao_test_walk_skill_mds");
        std::fs::create_dir_all(&dir).unwrap();

        // 创建一个 SKILL.md 文件
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: test\ndescription: test skill\n---\nBody",
        )
        .unwrap();

        let result = walk_skill_mds(&dir);
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("SKILL.md"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walk_skill_mds_excludes_hidden_dirs() {
        let dir = std::env::temp_dir().join("fuyao_test_walk_hidden");
        std::fs::create_dir_all(&dir).unwrap();

        // 创建隐藏目录中的 SKILL.md
        let hidden_dir = dir.join(".hidden");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        std::fs::write(hidden_dir.join("SKILL.md"), "test").unwrap();

        // 创建正常目录中的 SKILL.md
        let normal_dir = dir.join("normal");
        std::fs::create_dir_all(&normal_dir).unwrap();
        std::fs::write(normal_dir.join("SKILL.md"), "test").unwrap();

        let result = walk_skill_mds(&dir);
        assert_eq!(result.len(), 1);
        assert!(result[0].to_string_lossy().contains("normal"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walk_skill_mds_excludes_excluded_dirs() {
        let dir = std::env::temp_dir().join("fuyao_test_walk_excluded");
        std::fs::create_dir_all(&dir).unwrap();

        // 创建 node_modules 目录中的 SKILL.md
        let node_modules = dir.join("node_modules");
        std::fs::create_dir_all(&node_modules).unwrap();
        std::fs::write(node_modules.join("SKILL.md"), "test").unwrap();

        // 创建 __pycache__ 目录中的 SKILL.md
        let pycache = dir.join("__pycache__");
        std::fs::create_dir_all(&pycache).unwrap();
        std::fs::write(pycache.join("SKILL.md"), "test").unwrap();

        // 创建正常目录中的 SKILL.md
        let normal_dir = dir.join("normal");
        std::fs::create_dir_all(&normal_dir).unwrap();
        std::fs::write(normal_dir.join("SKILL.md"), "test").unwrap();

        let result = walk_skill_mds(&dir);
        assert_eq!(result.len(), 1);
        assert!(result[0].to_string_lossy().contains("normal"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walk_skill_mds_empty_dir() {
        let dir = std::env::temp_dir().join("fuyao_test_walk_empty");
        std::fs::create_dir_all(&dir).unwrap();

        let result = walk_skill_mds(&dir);
        assert!(result.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walk_skill_mds_nonexistent_dir() {
        let dir = std::env::temp_dir().join("fuyao_test_walk_nonexistent_12345");
        let result = walk_skill_mds(&dir);
        assert!(result.is_empty());
    }

    #[test]
    fn find_skill_md_by_name_with_default_ctx() {
        let ctx = AgentPaths::default();
        let result = find_skill_md_by_name("test", &ctx);
        assert_eq!(result, (None, None));
    }

    #[test]
    fn find_all_skills_with_default_ctx() {
        let ctx = AgentPaths::default();
        let result = find_all_skills(&ctx).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn excluded_dirs_contains_expected() {
        assert!(EXCLUDED_DIRS.contains(&".git"));
        assert!(EXCLUDED_DIRS.contains(&".github"));
        assert!(EXCLUDED_DIRS.contains(&".venv"));
        assert!(EXCLUDED_DIRS.contains(&"node_modules"));
        assert!(EXCLUDED_DIRS.contains(&"__pycache__"));
        assert!(EXCLUDED_DIRS.contains(&"dist"));
        assert!(EXCLUDED_DIRS.contains(&"build"));
    }

    /// 字节截断必须落在 UTF-8 字符边界上，否则 panic
    ///
    /// 回归保护：中文等字符占 3 字节，`c[..4000]` 在 4000 落到多字节字符中间时
    /// 会 panic（曾发现在含中文 frontmatter 的 SKILL.md 上崩溃）。
    /// 用 floor_char_boundary 回退到最近的字符起始边界即可。
    #[test]
    fn truncate_at_byte_boundary_on_multibyte_content() {
        // 构造长度 > 4000 字节、且 4000 落在中文字符中间的内容（中文 3 字节/字）
        let content = "题".repeat(2000); // 6000 字节，全中文
        assert!(content.len() > 4000);

        // 旧写法会 panic：let _ = &content[..4000];
        // 新写法：floor_char_boundary 回退到字符边界
        let end = content.floor_char_boundary(4000);
        let truncated = &content[..end];

        // 截断点严格不超 4000，且是字符边界（UTF-8 合法，可重转 String）
        assert!(end <= 4000);
        assert!(end % 3 == 0, "全中文内容，字符边界应是 3 的倍数：{end}");
        let _ = truncated.to_string(); // 不 panic 即合法 UTF-8
    }
}
