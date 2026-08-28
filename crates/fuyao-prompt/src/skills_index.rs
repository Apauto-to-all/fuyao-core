//! Skills 读取门面：文件系统四层优先 + 内置兜底
//!
//! 技能读取的统一出口，三个函数与 `fuyao_skills` 对应函数签名完全一致，
//! 消费方只需换 import 即获得内置技能兜底：
//! - [`list_skills`]：文件系统发现结果 + 内置技能条目追加（同名去重，内置最低优先级）
//! - [`load_skill`]：文件系统优先，NotFound 时查内置表
//! - [`load_skill_file`]：文件系统优先，NotFound 时查内置表
//!
//! 错误透传约定：仅 [`SkillsError::NotFound`] 触发内置兜底；其他错误（IO 等）
//! 原样透传，不吞成内置兜底——文件系统层的真实故障不被内置版本掩盖。

use crate::builtin::{BuiltinKind, builtin_entry, builtin_names, builtin_skill_file};
use fuyao_api::{AgentPaths, LINKED_SUBDIRS, SkillDefinition, SkillMeta};
use fuyao_skills::SkillsError;
use fuyao_skills::{find_first_non_heading, parse_skill_frontmatter, truncate_skill_fields};
use std::collections::{HashMap, HashSet};

/// 列举全部可用技能（Tier 1：元数据）
///
/// 文件系统四层（workspace > agent > global > extra）发现结果在前，
/// 内置技能条目追加在后（同名去重，内置最低优先级）。
/// 内置 Tier 1 元数据由嵌入 SKILL.md 的 frontmatter 解析而来，
/// name / description 必须带全——缺失即视为资产损坏，跳过并记 WARN，
/// 不 panic、不拖垮整个列举。
pub fn list_skills(ctx: &AgentPaths) -> Result<Vec<SkillMeta>, SkillsError> {
    let mut skills = fuyao_skills::find_all_skills(ctx)?;
    // 去重键持有所有权（String），避免与 skills 的后续 push 构成借用冲突
    let fs_names: HashSet<String> = skills.iter().map(|s| s.name.clone()).collect();

    for name in builtin_names(BuiltinKind::Skills) {
        let Some(md) = builtin_skill_file(name, "SKILL.md") else {
            tracing::warn!(
                skill = name,
                cause = "缺 SKILL.md",
                "内置技能资产损坏，已从列举跳过"
            );
            continue;
        };
        let parsed = parse_skill_frontmatter(md);
        if parsed.name.is_empty() || parsed.description.is_empty() {
            tracing::warn!(
                skill = name,
                cause = "frontmatter 缺 name/description",
                "内置技能资产损坏，已从列举跳过"
            );
            continue;
        }
        // 同名去重：文件系统版本胜，内置版本不追加
        if fs_names.contains(&parsed.name) {
            continue;
        }
        skills.push(SkillMeta::new(parsed.name, parsed.description));
    }

    // 追加的内置条目可能打乱名称序，整体重排保证输出稳定
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

/// 按名称加载技能完整定义（Tier 2：完整内容）
///
/// 先查文件系统（四层优先级）；返回 [`SkillsError::NotFound`] 时查内置表，
/// 内置也未命中则原样返回 NotFound。
/// 内置定义的 name / description fallback 与文件系统语义一致
/// （frontmatter → 条目名 / 正文首行非标题），`skill_dir` 恒为 `None`
/// （编译期嵌入，无磁盘目录）。
pub fn load_skill(name: &str, ctx: &AgentPaths) -> Result<SkillDefinition, SkillsError> {
    match fuyao_skills::load_skill(name, ctx) {
        Ok(def) => Ok(def),
        // 仅 NotFound 触发内置兜底
        Err(err @ SkillsError::NotFound(_)) => builtin_skill_definition(name).ok_or(err),
        // 其他错误（IO 等）原样透传
        Err(err) => Err(err),
    }
}

/// 加载技能内的关联文件（Tier 3：关联文件）
///
/// 先查文件系统；返回 [`SkillsError::NotFound`] 时查内置表。
/// 内置兜底沿用同口径防护：file_path 空白或含 `..` 一律拒绝为
/// [`SkillsError::PathTraversal`]；内置表中也不存在该文件时返回 NotFound。
pub fn load_skill_file(
    name: &str,
    file_path: &str,
    ctx: &AgentPaths,
) -> Result<String, SkillsError> {
    match fuyao_skills::load_skill_file(name, file_path, ctx) {
        Ok(content) => Ok(content),
        Err(err @ SkillsError::NotFound(_)) => {
            if file_path.trim().is_empty() || file_path.contains("..") {
                return Err(SkillsError::PathTraversal(file_path.to_string()));
            }
            builtin_skill_file(name, file_path)
                .map(str::to_string)
                .ok_or(err)
        }
        // 其他错误（含文件系统层判定的 PathTraversal）原样透传
        Err(err) => Err(err),
    }
}

/// 从内置表解析技能完整定义
///
/// 仅精确匹配内置技能名（空串不匹配），未命中或资产不完整返回 `None`，
/// 由调用方落回 NotFound 语义。
fn builtin_skill_definition(name: &str) -> Option<SkillDefinition> {
    let entry = builtin_entry(BuiltinKind::Skills, name)?;
    let md = entry.file("SKILL.md")?;
    let parsed = parse_skill_frontmatter(md);

    // name fallback：frontmatter → 条目名（条目名即运行时的目录名语义）
    let skill_name = if parsed.name.is_empty() {
        entry.name().to_string()
    } else {
        parsed.name
    };
    // description fallback：frontmatter → 正文首行非标题
    let description = if parsed.description.is_empty() {
        find_first_non_heading(&parsed.body)
    } else {
        parsed.description
    };

    // 最终验证（同文件系统口径）：name/description 必须有值
    if skill_name.is_empty() || description.is_empty() {
        return None;
    }

    let (name, description, compatibility) =
        truncate_skill_fields(&skill_name, &description, parsed.compatibility.as_deref());

    Some(SkillDefinition {
        name,
        description,
        license: parsed.license,
        compatibility,
        metadata: parsed.metadata,
        body: parsed.body,
        skill_dir: None,
        linked_files: builtin_linked_files(entry.files()),
    })
}

/// 从静态文件集推导内置技能的关联文件映射
///
/// 与文件系统扫描同语义：仅统计位于关联子目录（scripts / references / assets，
/// 见 [`LINKED_SUBDIRS`]）下的文件，键为子目录名，值为组内排序的目录内相对路径
/// 列表；SKILL.md 与根级散放文件不属于关联文件。
fn builtin_linked_files(
    files: &'static [(&'static str, &'static str)],
) -> HashMap<String, Vec<String>> {
    let mut linked: HashMap<String, Vec<String>> = HashMap::new();
    for (rel, _) in files {
        if *rel == "SKILL.md" {
            continue;
        }
        let Some((subdir, _)) = rel.split_once('/') else {
            continue;
        };
        if !LINKED_SUBDIRS.contains(&subdir) {
            continue;
        }
        linked
            .entry(subdir.to_string())
            .or_default()
            .push(rel.to_string());
    }
    for values in linked.values_mut() {
        values.sort();
    }
    linked
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 fuyao_home 指向隔离 tempdir 的路径上下文（全局层为空目录）
    fn isolated_ctx() -> (tempfile::TempDir, AgentPaths) {
        let home = tempfile::tempdir().unwrap();
        let ctx = AgentPaths {
            fuyao_home: home.path().to_path_buf(),
            ..AgentPaths::default()
        };
        (home, ctx)
    }

    /// 纯内置环境：list_skills 应含内置技能 fuyao-config
    #[test]
    fn list_skills_includes_builtin() {
        let (_home, ctx) = isolated_ctx();
        let skills = list_skills(&ctx).unwrap();
        let meta = skills
            .iter()
            .find(|s| s.name == "fuyao-config")
            .expect("内置技能 fuyao-config 应在列");
        assert!(!meta.description.is_empty());
    }

    /// workspace 层放同名技能后：去重为 1 条且用户版胜出（列举与加载一致）
    #[test]
    fn workspace_skill_overrides_builtin() {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let user_skill_dir = ws.join(".fuyao").join("skills").join("fuyao-config");
        std::fs::create_dir_all(&user_skill_dir).unwrap();
        std::fs::write(
            user_skill_dir.join("SKILL.md"),
            "---\nname: fuyao-config\ndescription: 用户自定义版本\n---\n自定义内容",
        )
        .unwrap();

        let ctx = AgentPaths {
            fuyao_home: home.path().to_path_buf(),
            workspace: Some(ws),
            ..AgentPaths::default()
        };

        let skills = list_skills(&ctx).unwrap();
        let matches: Vec<_> = skills.iter().filter(|s| s.name == "fuyao-config").collect();
        assert_eq!(matches.len(), 1, "同名技能应去重为 1 条");
        assert_eq!(matches[0].description, "用户自定义版本");

        let def = load_skill("fuyao-config", &ctx).unwrap();
        assert_eq!(def.description, "用户自定义版本");
        assert_eq!(def.body, "自定义内容");
    }

    /// 纯内置加载：返回完整定义，skill_dir 为 None，name 为条目名
    #[test]
    fn load_skill_pure_builtin_returns_definition() {
        let (_home, ctx) = isolated_ctx();
        let def = load_skill("fuyao-config", &ctx).unwrap();
        assert_eq!(def.name, "fuyao-config");
        assert!(!def.description.is_empty());
        assert!(def.skill_dir.is_none(), "内置技能无磁盘目录");
        assert!(!def.body.is_empty());
        // fuyao-config 资产仅含 SKILL.md，无关联文件
        assert!(def.linked_files.is_empty());
    }

    /// 文件系统与内置均未命中 → NotFound
    #[test]
    fn load_skill_unknown_name_returns_not_found() {
        let (_home, ctx) = isolated_ctx();
        let err = load_skill("nonexistent-skill", &ctx).unwrap_err();
        assert!(matches!(err, SkillsError::NotFound(_)));
    }

    /// 内置技能的关联文件读取：SKILL.md 命中并返回嵌入内容
    #[test]
    fn load_skill_file_builtin_hits() {
        let (_home, ctx) = isolated_ctx();
        let content = load_skill_file("fuyao-config", "SKILL.md", &ctx).unwrap();
        assert!(content.contains("name: fuyao-config"));
    }

    /// 内置兜底沿用同口径防护：路径含 ".." 或空白 → PathTraversal
    #[test]
    fn load_skill_file_builtin_rejects_traversal_and_blank() {
        let (_home, ctx) = isolated_ctx();
        let err = load_skill_file("fuyao-config", "../secret.txt", &ctx).unwrap_err();
        assert!(matches!(err, SkillsError::PathTraversal(_)));

        let err = load_skill_file("fuyao-config", "   ", &ctx).unwrap_err();
        assert!(matches!(err, SkillsError::PathTraversal(_)));
    }

    /// 内置技能存在但文件不在静态表 → NotFound
    #[test]
    fn load_skill_file_builtin_unknown_file_not_found() {
        let (_home, ctx) = isolated_ctx();
        let err = load_skill_file("fuyao-config", "no-such-file.md", &ctx).unwrap_err();
        assert!(matches!(err, SkillsError::NotFound(_)));
    }

    /// 关联文件推导：只有关联子目录下的文件计入分组，SKILL.md 与根级文件不计入
    #[test]
    fn builtin_linked_files_groups_by_linked_subdirs() {
        let entry = crate::builtin::builtin_entry(BuiltinKind::Skills, "fuyao-config").unwrap();
        assert!(builtin_linked_files(entry.files()).is_empty());

        // 构造带各类文件形态的静态文件集，直接验证推导函数的分组口径
        let files: &'static [(&'static str, &'static str)] = &[
            ("SKILL.md", "skill"),
            ("scripts/b.sh", "b"),
            ("scripts/a.sh", "a"),
            ("references/api.md", "api"),
            ("loose.md", "loose"),
            ("other/x.md", "x"),
        ];
        let linked = builtin_linked_files(files);
        assert_eq!(
            linked.get("scripts"),
            Some(&vec![
                "scripts/a.sh".to_string(),
                "scripts/b.sh".to_string()
            ])
        );
        assert_eq!(
            linked.get("references"),
            Some(&vec!["references/api.md".to_string()])
        );
        assert!(!linked.contains_key("assets"));
        // 根级与关联子目录之外的文件不计入
        assert!(!linked.values().flatten().any(|f| f == "loose.md"));
        assert!(!linked.values().flatten().any(|f| f == "other/x.md"));
        assert!(!linked.values().flatten().any(|f| f == "SKILL.md"));
    }
}
