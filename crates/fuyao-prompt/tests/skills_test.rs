//! Skills 公开 API 契约集成测试
//!
//! 钉死 skills 读取出口（list_skills / load_skill / load_skill_file）的契约：
//! - 文件系统四层优先级（workspace > agent > global > extra）与内置兜底的叠加
//! - frontmatter 全字段解析 + fallback 链（name→目录名、description→正文首行非标题）
//! - load_skill_file 路径遍历防护
//! - linked_files 关联文件扫描
//!
//! 全部走 tempdir 真实文件系统，零 mock、零网络。

mod common;

use common::{make_agent_paths, write_linked_file, write_skill_md};
use fuyao_prompt::{SkillsError, list_skills, load_skill, load_skill_file};
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
// list_skills：分层去重与内置追加
// ---------------------------------------------------------------------------

#[test]
fn list_skills_dedup_keeps_higher_priority_layer_and_appends_builtin() {
    // 同名 skill 在 global 与 workspace 两层，去重保留 workspace（高优先级）；
    // 内置技能（fuyao-config）恒定追加在末尾
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let ws = temp.path().join("project");
    std::fs::create_dir_all(ws_skills(&ws)).unwrap();

    write_skill_md(
        &home_skills(home),
        "dup",
        "name: dup\ndescription: 全局层",
        None,
    );
    write_skill_md(
        &ws_skills(&ws),
        "dup",
        "name: dup\ndescription: workspace 层",
        None,
    );

    let paths = make_agent_paths(home.to_path_buf(), Some(ws), vec![]);
    let skills = list_skills(&paths).unwrap();
    let dup: Vec<_> = skills.iter().filter(|s| s.name == "dup").collect();
    assert_eq!(dup.len(), 1, "同名技能应去重为 1 条");
    assert_eq!(dup[0].description, "workspace 层", "应保留高优先级层");
    assert!(
        skills.iter().any(|s| s.name == "fuyao-config"),
        "内置技能应追加在列"
    );
}

// ---------------------------------------------------------------------------
// load_skill：frontmatter 全字段 + fallback 链
// ---------------------------------------------------------------------------

#[test]
fn load_skill_parses_all_frontmatter_fields() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    write_skill_md(
        &home_skills(home),
        "full",
        "name: full\ndescription: 完整 skill\nlicense: MIT\ncompatibility: \">=1.0\"\nmetadata:\n  version: \"2.1\"\n  author: test",
        Some("# 正文内容"),
    );

    let paths = make_agent_paths(home.to_path_buf(), None, vec![]);
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
    write_skill_md(
        &home_skills(home),
        "from-dir-name",
        "description: 有描述",
        Some("正文首行"),
    );

    let paths = make_agent_paths(home.to_path_buf(), None, vec![]);
    let skill = load_skill("from-dir-name", &paths).unwrap();
    assert_eq!(skill.name, "from-dir-name", "name 应回退到目录名");
    assert_eq!(skill.description, "有描述");
}

#[test]
fn load_skill_falls_back_to_first_non_heading_for_description() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    // frontmatter 有 name 无 description → description 回退到正文首行非标题
    write_skill_md(
        &home_skills(home),
        "no-desc",
        "name: no-desc",
        Some("# 标题行\n这是正文首行非标题\n第二行"),
    );

    let paths = make_agent_paths(home.to_path_buf(), None, vec![]);
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
    write_skill_md(
        &home_skills(home),
        "empty-skill",
        "",
        Some("   "), // 纯空白正文
    );

    let paths = make_agent_paths(home.to_path_buf(), None, vec![]);
    let result = load_skill("empty-skill", &paths);
    assert!(matches!(result, Err(SkillsError::NotFound(_))));
}

#[test]
fn load_skill_returns_not_found_for_absent_skill() {
    let temp = tempfile::tempdir().unwrap();
    let paths = make_agent_paths(temp.path().to_path_buf(), None, vec![]);
    let result = load_skill("does-not-exist", &paths);
    assert!(matches!(result, Err(SkillsError::NotFound(_))));
}

#[test]
fn load_skill_populates_skill_dir_field() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    write_skill_md(
        &home_skills(home),
        "located",
        "name: located\ndescription: d",
        None,
    );

    let paths = make_agent_paths(home.to_path_buf(), None, vec![]);
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
    let skill_dir = write_skill_md(
        &home_skills(home),
        "linked",
        "name: linked\ndescription: d",
        None,
    );

    // 在三个 LINKED_SUBDIRS 下各放一个文件
    write_linked_file(&skill_dir, "scripts", "setup.sh", "#!/bin/sh");
    write_linked_file(&skill_dir, "references", "doc.md", "# doc");
    write_linked_file(&skill_dir, "assets", "img.png", "png");

    let paths = make_agent_paths(home.to_path_buf(), None, vec![]);
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
    let skill_dir = write_skill_md(
        &home_skills(home),
        "nolink",
        "name: nolink\ndescription: d",
        None,
    );
    // 不创建任何子目录
    let _ = &skill_dir;

    let paths = make_agent_paths(home.to_path_buf(), None, vec![]);
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
    write_skill_md(
        &home_skills(home),
        "safe",
        "name: safe\ndescription: d",
        None,
    );

    let paths = make_agent_paths(home.to_path_buf(), None, vec![]);
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
    write_skill_md(
        &home_skills(home),
        "safe",
        "name: safe\ndescription: d",
        None,
    );

    let paths = make_agent_paths(home.to_path_buf(), None, vec![]);
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
    let skill_dir = write_skill_md(
        &home_skills(home),
        "reader",
        "name: reader\ndescription: d",
        None,
    );
    write_linked_file(&skill_dir, "scripts", "run.sh", "echo hello");

    let paths = make_agent_paths(home.to_path_buf(), None, vec![]);
    let content = load_skill_file("reader", "scripts/run.sh", &paths).unwrap();
    assert_eq!(content, "echo hello");
}

#[test]
fn load_skill_file_returns_not_found_for_missing_file() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    write_skill_md(
        &home_skills(home),
        "reader",
        "name: reader\ndescription: d",
        None,
    );

    let paths = make_agent_paths(home.to_path_buf(), None, vec![]);
    let result = load_skill_file("reader", "scripts/nonexistent.sh", &paths);
    // 目标文件不存在 → canonicalize 失败 → Io 错误（或 NotFound，取决于路径）
    assert!(result.is_err());
}

#[test]
fn load_skill_file_returns_not_found_for_absent_skill() {
    let temp = tempfile::tempdir().unwrap();
    let paths = make_agent_paths(temp.path().to_path_buf(), None, vec![]);
    let result = load_skill_file("no-such-skill", "any.txt", &paths);
    assert!(matches!(result, Err(SkillsError::NotFound(_))));
}
