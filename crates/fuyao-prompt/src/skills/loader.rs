//! Skills 加载器
//!
//! 按需加载 Skill 完整内容（Tier 2）和关联文件（Tier 3）。

use super::error::SkillsError;
use super::finder::find_skill_md_by_name;
use super::helpers::{
    find_first_non_heading, parse_skill_frontmatter, scan_linked_files, truncate_skill_fields,
};
use fuyao_api::AgentPaths;
use fuyao_api::SkillDefinition;

/// 按名称加载 Skill 完整定义（Tier 2：完整内容）
///
/// 搜索顺序：workspace → agent → global → extra（高优先级优先）。
pub fn load_skill(name: &str, ctx: &AgentPaths) -> Result<SkillDefinition, SkillsError> {
    let (skill_dir, skill_md_path) = find_skill_md_by_name(name, ctx);
    let skill_md_path = skill_md_path.ok_or_else(|| SkillsError::NotFound(name.to_string()))?;

    if !skill_md_path.exists() {
        return Err(SkillsError::NotFound(name.to_string()));
    }

    let content = std::fs::read_to_string(&skill_md_path)?;
    let parsed = parse_skill_frontmatter(&content);

    // name fallback：frontmatter → 目录名
    let skill_name = if parsed.name.is_empty() {
        skill_dir
            .as_ref()
            .and_then(|d| d.file_name().map(|f| f.to_string_lossy().to_string()))
            .unwrap_or_else(|| {
                skill_md_path
                    .parent()
                    .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
                    .unwrap_or_default()
            })
    } else {
        parsed.name
    };

    // description fallback：frontmatter → 正文首行非标题
    let description = if parsed.description.is_empty() {
        find_first_non_heading(&parsed.body)
    } else {
        parsed.description
    };

    // 最终验证：name/description 必须有值
    if skill_name.is_empty() || description.is_empty() {
        return Err(SkillsError::NotFound(name.to_string()));
    }

    // 关联文件
    let linked_files = skill_dir
        .as_ref()
        .map(|d| scan_linked_files(d))
        .unwrap_or_default();

    // 字段截断
    let (name, description, compatibility) =
        truncate_skill_fields(&skill_name, &description, parsed.compatibility.as_deref());

    Ok(SkillDefinition {
        name,
        description,
        license: parsed.license,
        compatibility,
        metadata: parsed.metadata,
        body: parsed.body,
        skill_dir: skill_dir.map(|d| d.to_string_lossy().to_string()),
        linked_files,
    })
}

/// 加载 Skill 内的关联文件（Tier 3：关联文件）
///
/// 安全：防止路径遍历（`..`），确保解析后仍在 Skill 目录内。
pub fn load_skill_file(
    name: &str,
    file_path: &str,
    ctx: &AgentPaths,
) -> Result<String, SkillsError> {
    let (skill_dir, _) = find_skill_md_by_name(name, ctx);
    let skill_dir = skill_dir.ok_or_else(|| SkillsError::NotFound(name.to_string()))?;

    // 检查空文件路径
    if file_path.trim().is_empty() {
        return Err(SkillsError::PathTraversal(file_path.to_string()));
    }

    // 安全：防止路径遍历
    if file_path.contains("..") {
        return Err(SkillsError::PathTraversal(file_path.to_string()));
    }

    let target = skill_dir.join(file_path);

    // 安全：确保解析后仍在 Skill 目录内
    let target_resolved = target.canonicalize()?;
    let skill_dir_resolved = skill_dir.canonicalize()?;
    if !target_resolved.starts_with(&skill_dir_resolved) {
        return Err(SkillsError::PathTraversal(file_path.to_string()));
    }

    // 检查是否为文件（不是目录）
    if !target_resolved.is_file() {
        return Err(SkillsError::NotFound(file_path.to_string()));
    }

    match std::fs::read_to_string(&target_resolved) {
        Ok(content) => Ok(content),
        Err(_) => {
            // 二进制文件
            let size = std::fs::metadata(&target_resolved)
                .map(|m| m.len())
                .unwrap_or(0);
            let name = target_resolved
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            Ok(format!("[二进制文件: {name}, 大小: {size} 字节]"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn create_test_skill_ctx(skill_name: &str) -> (PathBuf, AgentPaths) {
        let dir = std::env::temp_dir().join(format!("fuyao_test_loader_{}", skill_name));
        let skills_dir = dir.join(".fuyao").join("skills").join(skill_name);
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("SKILL.md"),
            format!("---\nname: {}\n---\nBody", skill_name),
        )
        .unwrap();
        let ctx = AgentPaths {
            agent_id: None,
            workspace: Some(dir.clone()),
            ..Default::default()
        };
        (dir, ctx)
    }

    #[test]
    fn load_skill_not_found() {
        let ctx = AgentPaths::default();
        let result = load_skill("nonexistent_skill_12345", &ctx);
        assert!(result.is_err());
    }

    #[test]
    fn load_skill_file_not_found() {
        let ctx = AgentPaths::default();
        let result = load_skill_file("nonexistent_skill_12345", "test.txt", &ctx);
        assert!(result.is_err());
    }

    #[test]
    fn load_skill_file_rejects_path_traversal() {
        let (dir, ctx) = create_test_skill_ctx("traversal_test");
        let result = load_skill_file("traversal_test", "../../../etc/passwd", &ctx);
        assert!(matches!(result, Err(SkillsError::PathTraversal(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_skill_file_rejects_empty_path() {
        let (dir, ctx) = create_test_skill_ctx("empty_test");
        let result = load_skill_file("empty_test", "", &ctx);
        assert!(matches!(result, Err(SkillsError::PathTraversal(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_skill_file_rejects_whitespace_only_path() {
        let (dir, ctx) = create_test_skill_ctx("whitespace_test");
        let result = load_skill_file("whitespace_test", "   ", &ctx);
        assert!(matches!(result, Err(SkillsError::PathTraversal(_))));
        std::fs::remove_dir_all(&dir).ok();
    }
}
