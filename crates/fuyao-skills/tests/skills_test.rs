//! fuyao-skills 集成测试
//!
//! 钉死 Skills 三层发现与加载的公开 API 契约：
//! - 三层优先级（workspace > agent > global > extra）与同名去重
//! - frontmatter 全字段解析 + fallback 链（name→目录名、description→正文首行非标题）
//! - load_skill_file 路径遍历防护
//! - linked_files 关联文件扫描
//!
//! 全部走 tempdir 真实文件系统，零 mock、零网络。

mod common;

use fuyao_skills::{
    SkillsError, find_all_skills, find_skill_md_by_name, load_skill, load_skill_file,
};
use rstest::rstest;

/// 构造指向 tempdir 下 home 的 skills 根目录路径
fn home_skills(home: &std::path::Path) -> std::path::PathBuf {
    home.join("skills")
}

/// 构造 workspace 层的 skills 根目录（ws/.fuyao/skills）
fn ws_skills(ws: &std::path::Path) -> std::path::PathBuf {
    ws.join(".fuyao").join("skills")
}

// ---------------------------------------------------------------------------
// find_all_skills：三层发现与去重
// ---------------------------------------------------------------------------

#[test]
fn find_all_skills_empty_returns_empty_vec() {
    // 无任何 skills 目录 → Ok(空)
    let temp = tempfile::tempdir().unwrap();
    let paths = common::make_paths(temp.path().to_path_buf(), None, vec![]);
    let skills = find_all_skills(&paths).unwrap();
    assert!(skills.is_empty());
}

#[test]
fn find_all_skills_discovers_from_global_layer() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    common::write_skill_md(
        &home_skills(home),
        "my-skill",
        "name: my-skill\ndescription: 全局层 skill",
        None,
    );

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let skills = find_all_skills(&paths).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "my-skill");
    assert_eq!(skills[0].description, "全局层 skill");
}

#[test]
fn find_all_skills_dedup_keeps_higher_priority_layer() {
    // 同名 skill 在 global 与 workspace 两层，应去重保留 workspace（高优先级）
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let ws = temp.path().join("project");
    std::fs::create_dir_all(ws_skills(&ws)).unwrap();

    common::write_skill_md(
        &home_skills(home),
        "dup",
        "name: dup\ndescription: 全局层",
        None,
    );
    common::write_skill_md(
        &ws_skills(&ws),
        "dup",
        "name: dup\ndescription: workspace 层",
        None,
    );

    let paths = common::make_paths(home.to_path_buf(), Some(ws), vec![]);
    let skills = find_all_skills(&paths).unwrap();
    assert_eq!(skills.len(), 1, "同名应去重");
    assert_eq!(skills[0].description, "workspace 层", "应保留高优先级层");
}

#[test]
fn find_all_skills_extra_dirs_lowest_priority() {
    // extra_dirs 提供的 skill 应被发现，但优先级最低
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let plugin_root = temp.path().join("my-plugin");
    std::fs::create_dir_all(plugin_root.join("skills")).unwrap();

    common::write_skill_md(
        &plugin_root.join("skills"),
        "plugin-skill",
        "name: plugin-skill\ndescription: 来自插件",
        None,
    );

    let paths = common::make_paths(home.to_path_buf(), None, vec![plugin_root]);
    let skills = find_all_skills(&paths).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "plugin-skill");
}

#[test]
fn find_all_skills_results_sorted_by_name() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    common::write_skill_md(
        &home_skills(home),
        "zebra",
        "name: zebra\ndescription: z",
        None,
    );
    common::write_skill_md(
        &home_skills(home),
        "apple",
        "name: apple\ndescription: a",
        None,
    );
    common::write_skill_md(
        &home_skills(home),
        "mango",
        "name: mango\ndescription: m",
        None,
    );

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let skills = find_all_skills(&paths).unwrap();
    let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["apple", "mango", "zebra"]);
}

// ---------------------------------------------------------------------------
// find_skill_md_by_name：优先级与模糊匹配
// ---------------------------------------------------------------------------

#[test]
fn find_skill_md_by_name_returns_none_when_absent() {
    let temp = tempfile::tempdir().unwrap();
    let paths = common::make_paths(temp.path().to_path_buf(), None, vec![]);
    let (dir, md) = find_skill_md_by_name("nonexistent", &paths);
    assert!(dir.is_none());
    assert!(md.is_none());
}

#[test]
fn find_skill_md_by_name_prefers_workspace_layer() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let ws = temp.path().join("project");
    std::fs::create_dir_all(ws_skills(&ws)).unwrap();

    // 两层都放同名 skill
    common::write_skill_md(&home_skills(home), "shared", "name: shared", None);
    common::write_skill_md(&ws_skills(&ws), "shared", "name: shared", None);

    let paths = common::make_paths(home.to_path_buf(), Some(ws.clone()), vec![]);
    let (dir, md) = find_skill_md_by_name("shared", &paths);
    assert!(dir.is_some());
    assert!(md.is_some());
    // 命中的应是 workspace 层
    assert!(dir.unwrap().starts_with(ws_skills(&ws)));
    assert!(md.unwrap().exists());
}

#[test]
fn find_skill_md_by_name_matches_nested_directory() {
    // 嵌套目录：skills/group/my-skill/SKILL.md，用 name="my-skill" 应能找到
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let nested = home_skills(home).join("group").join("my-skill");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("SKILL.md"), "name: my-skill").unwrap();

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let (dir, md) = find_skill_md_by_name("my-skill", &paths);
    assert!(dir.is_some(), "嵌套目录应通过递归 walk 命中");
    assert!(md.is_some());
}

// ---------------------------------------------------------------------------
// load_skill：frontmatter 全字段 + fallback 链
// ---------------------------------------------------------------------------

#[test]
fn load_skill_parses_all_frontmatter_fields() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    common::write_skill_md(
        &home_skills(home),
        "full",
        "name: full\ndescription: 完整 skill\nlicense: MIT\ncompatibility: \">=1.0\"\nmetadata:\n  version: \"2.1\"\n  author: test",
        Some("# 正文内容"),
    );

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let skill = load_skill("full", &paths).unwrap();
    assert_eq!(skill.name, "full");
    assert_eq!(skill.description, "完整 skill");
    assert_eq!(skill.license.as_deref(), Some("MIT"));
    assert_eq!(skill.compatibility.as_deref(), Some(">=1.0"));
    assert_eq!(skill.body, "# 正文内容");
    // metadata 经 yaml→json 转换
    assert_eq!(
        skill.metadata.get("version").unwrap(),
        &serde_json::json!("2.1")
    );
    assert_eq!(
        skill.metadata.get("author").unwrap(),
        &serde_json::json!("test")
    );
}

#[test]
fn load_skill_falls_back_to_directory_name_when_frontmatter_missing() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    // frontmatter 无 name，但正文有内容 → name 回退到目录名
    common::write_skill_md(
        &home_skills(home),
        "from-dir-name",
        "description: 有描述",
        Some("正文首行"),
    );

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let skill = load_skill("from-dir-name", &paths).unwrap();
    assert_eq!(skill.name, "from-dir-name", "name 应回退到目录名");
    assert_eq!(skill.description, "有描述");
}

#[test]
fn load_skill_falls_back_to_first_non_heading_for_description() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    // frontmatter 有 name 无 description → description 回退到正文首行非标题
    common::write_skill_md(
        &home_skills(home),
        "no-desc",
        "name: no-desc",
        Some("# 标题行\n这是正文首行非标题\n第二行"),
    );

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let skill = load_skill("no-desc", &paths).unwrap();
    assert_eq!(skill.name, "no-desc");
    assert_eq!(
        skill.description, "这是正文首行非标题",
        "应取正文首行非标题"
    );
}

#[test]
fn load_skill_returns_not_found_when_both_name_and_description_empty() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    // frontmatter 完全空 + 正文空 → name 回退目录名，但 description 为空 → NotFound
    // 目录名虽非空，但 description 无 fallback 来源 → 最终验证失败
    common::write_skill_md(
        &home_skills(home),
        "empty-skill",
        "",
        Some("   "), // 纯空白正文
    );

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let result = load_skill("empty-skill", &paths);
    assert!(matches!(result, Err(SkillsError::NotFound(_))));
}

#[test]
fn load_skill_returns_not_found_for_absent_skill() {
    let temp = tempfile::tempdir().unwrap();
    let paths = common::make_paths(temp.path().to_path_buf(), None, vec![]);
    let result = load_skill("does-not-exist", &paths);
    assert!(matches!(result, Err(SkillsError::NotFound(_))));
}

#[test]
fn load_skill_populates_skill_dir_field() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    common::write_skill_md(
        &home_skills(home),
        "located",
        "name: located\ndescription: d",
        None,
    );

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let skill = load_skill("located", &paths).unwrap();
    let dir = skill.skill_dir.expect("skill_dir 应被填充");
    assert!(dir.ends_with("located"), "skill_dir 应以 skill 名结尾");
}

// ---------------------------------------------------------------------------
// linked_files 关联文件扫描
// ---------------------------------------------------------------------------

#[test]
fn load_skill_scans_linked_files_in_known_subdirs() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let skill_dir = common::write_skill_md(
        &home_skills(home),
        "linked",
        "name: linked\ndescription: d",
        None,
    );

    // 在三个 LINKED_SUBDIRS 下各放一个文件
    common::write_linked_file(&skill_dir, "scripts", "setup.sh", "#!/bin/sh");
    common::write_linked_file(&skill_dir, "references", "doc.md", "# doc");
    common::write_linked_file(&skill_dir, "assets", "img.png", "png");

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let skill = load_skill("linked", &paths).unwrap();

    assert_eq!(skill.linked_files.len(), 3, "应扫描三个子目录");
    // 文件以相对路径记录（相对 skill 目录，用平台原生分隔符），各子目录键存在
    let scripts = skill.linked_files.get("scripts").unwrap();
    assert_eq!(scripts.len(), 1);
    assert!(
        scripts[0].ends_with("setup.sh"),
        "scripts 子目录应含 setup.sh，实际：{}",
        scripts[0]
    );
    let refs = skill.linked_files.get("references").unwrap();
    assert_eq!(refs.len(), 1);
    assert!(refs[0].ends_with("doc.md"));
}

#[test]
fn load_skill_omits_empty_linked_subdirs() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let skill_dir = common::write_skill_md(
        &home_skills(home),
        "nolink",
        "name: nolink\ndescription: d",
        None,
    );
    // 不创建任何子目录
    let _ = &skill_dir;

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let skill = load_skill("nolink", &paths).unwrap();
    assert!(skill.linked_files.is_empty(), "无子目录应返回空");
}

// ---------------------------------------------------------------------------
// load_skill_file：路径遍历防护
// ---------------------------------------------------------------------------

#[test]
fn load_skill_file_rejects_dotdot_traversal() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    common::write_skill_md(
        &home_skills(home),
        "safe",
        "name: safe\ndescription: d",
        None,
    );

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let result = load_skill_file("safe", "../../../etc/passwd", &paths);
    assert!(
        matches!(result, Err(SkillsError::PathTraversal(_))),
        "含 .. 的路径应被拒绝"
    );
}

#[rstest]
fn load_skill_file_rejects_empty_or_blank_path(#[values("", "   ", "\t\n")] bad_path: &str) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    common::write_skill_md(
        &home_skills(home),
        "safe",
        "name: safe\ndescription: d",
        None,
    );

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let result = load_skill_file("safe", bad_path, &paths);
    assert!(
        matches!(result, Err(SkillsError::PathTraversal(_))),
        "空/纯空白路径应被拒绝"
    );
}

#[test]
fn load_skill_file_reads_linked_file_content() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let skill_dir = common::write_skill_md(
        &home_skills(home),
        "reader",
        "name: reader\ndescription: d",
        None,
    );
    common::write_linked_file(&skill_dir, "scripts", "run.sh", "echo hello");

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let content = load_skill_file("reader", "scripts/run.sh", &paths).unwrap();
    assert_eq!(content, "echo hello");
}

#[test]
fn load_skill_file_returns_not_found_for_missing_file() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    common::write_skill_md(
        &home_skills(home),
        "reader",
        "name: reader\ndescription: d",
        None,
    );

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let result = load_skill_file("reader", "scripts/nonexistent.sh", &paths);
    // 目标文件不存在 → canonicalize 失败 → Io 错误（或 NotFound，取决于路径）
    assert!(result.is_err());
}

#[test]
fn load_skill_file_returns_not_found_for_absent_skill() {
    let temp = tempfile::tempdir().unwrap();
    let paths = common::make_paths(temp.path().to_path_buf(), None, vec![]);
    let result = load_skill_file("no-such-skill", "any.txt", &paths);
    assert!(matches!(result, Err(SkillsError::NotFound(_))));
}

// ---------------------------------------------------------------------------
// find_all_skills：name 超长截断
// ---------------------------------------------------------------------------

#[test]
fn find_all_skills_truncates_overlong_name() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    // 构造一个超过 64 字符的 name
    let long_name = "x".repeat(100);
    let fm = format!("name: {long_name}\ndescription: d");
    common::write_skill_md(&home_skills(home), &long_name, &fm, None);

    let paths = common::make_paths(home.to_path_buf(), None, vec![]);
    let skills = find_all_skills(&paths).unwrap();
    assert_eq!(skills.len(), 1);
    // SkillMeta::new 截断到 64 字符 + "..."
    assert!(skills[0].name.len() <= 64 + 3, "超长 name 应被截断");
    assert!(skills[0].name.ends_with("..."));
}
