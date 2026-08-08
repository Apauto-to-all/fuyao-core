//! Skills 辅助函数
//!
//! 纯工具函数：frontmatter 解析、关联文件扫描。

use fuyao_api::{LINKED_SUBDIRS, SkillDefinition};
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

/// frontmatter 分隔解析正则
///
/// 模式为编译期常量，提升为进程级静态量只编译一次，避免每次解析 SKILL.md 重复编译。
static FRONTMATTER_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?s)^---\s*\n(.*?)\n?---\s*\n(.*)$").expect("无效的 frontmatter 正则")
});

/// 解析 SKILL.md 内容
///
/// 解析 frontmatter（--- 分隔的 YAML 头部）和 body。
/// 缺失字段用空字符串填充，后续在 loader 中处理 fallback。
pub fn parse_skill_frontmatter(content: &str) -> SkillDefinition {
    let (frontmatter, body) = if let Some(caps) = FRONTMATTER_RE.captures(content) {
        let fm_str = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let body = caps
            .get(2)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_else(|| content.trim().to_string());
        (parse_yaml_dict(fm_str), body)
    } else {
        (serde_yaml::Mapping::new(), content.trim().to_string())
    };

    let name = get_string(&frontmatter, "name");
    let description = get_string(&frontmatter, "description");
    let license = get_optional_string(&frontmatter, "license");
    let compatibility = get_optional_string(&frontmatter, "compatibility");
    let metadata = frontmatter
        .get("metadata")
        .and_then(|v| {
            if let serde_yaml::Value::Mapping(m) = v {
                let mut map = HashMap::new();
                for (k, v) in m {
                    if let serde_yaml::Value::String(key) = k {
                        map.insert(key.clone(), serde_yaml_to_json(v));
                    }
                }
                Some(map)
            } else {
                None
            }
        })
        .unwrap_or_default();

    SkillDefinition {
        name,
        description,
        license,
        compatibility,
        metadata,
        body,
        skill_dir: None,
        linked_files: HashMap::new(),
    }
}

/// 扫描 Skill 目录的关联文件
///
/// 返回 {subdir_name: [relative_path, ...]}
pub fn scan_linked_files(skill_dir: &Path) -> HashMap<String, Vec<String>> {
    let mut linked = HashMap::new();

    for subdir_name in LINKED_SUBDIRS {
        let subdir = skill_dir.join(subdir_name);
        if !subdir.exists() {
            continue;
        }
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&subdir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file()
                    && let Ok(rel) = path.strip_prefix(skill_dir)
                {
                    files.push(rel.to_string_lossy().to_string());
                }
            }
        }
        if !files.is_empty() {
            files.sort();
            linked.insert(subdir_name.to_string(), files);
        }
    }

    linked
}

fn parse_yaml_dict(s: &str) -> serde_yaml::Mapping {
    if s.trim().is_empty() {
        return serde_yaml::Mapping::new();
    }
    serde_yaml::from_str::<serde_yaml::Value>(s)
        .ok()
        .and_then(|v| {
            if let serde_yaml::Value::Mapping(m) = v {
                Some(m)
            } else {
                None
            }
        })
        .unwrap_or_default()
}

fn get_string(mapping: &serde_yaml::Mapping, key: &str) -> String {
    mapping
        .get(serde_yaml::Value::String(key.to_string()))
        .and_then(|v| {
            if let serde_yaml::Value::String(s) = v {
                Some(s.trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

fn get_optional_string(mapping: &serde_yaml::Mapping, key: &str) -> Option<String> {
    mapping
        .get(serde_yaml::Value::String(key.to_string()))
        .and_then(|v| {
            if let serde_yaml::Value::String(s) = v {
                let s = s.trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            } else {
                None
            }
        })
}

/// 从正文找第一个非标题行作为 description fallback
pub fn find_first_non_heading(body: &str) -> String {
    for line in body.lines() {
        let line = line.trim();
        if !line.is_empty() && !line.starts_with('#') {
            return line.to_string();
        }
    }
    String::new()
}

/// 截断字符串到指定长度，超长加省略号
pub fn truncate_str(s: &str, max_len: usize) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    if s.len() > max_len {
        format!("{}...", &s[..max_len - 3])
    } else {
        s.to_string()
    }
}

/// 截断 Skill 字段到规范长度
///
/// 返回 (截断后的 name, 截断后的 description, 截断后的 compatibility)
pub fn truncate_skill_fields(
    name: &str,
    description: &str,
    compatibility: Option<&str>,
) -> (String, String, Option<String>) {
    let truncated_name = truncate_str(name, 64);
    let truncated_desc = truncate_str(description, 1024);
    let truncated_compat = compatibility.and_then(|c| {
        let c = c.trim().to_string();
        if c.is_empty() {
            None
        } else if c.len() > 500 {
            Some(c[..497].to_string() + "...")
        } else {
            Some(c)
        }
    });
    (truncated_name, truncated_desc, truncated_compat)
}

fn serde_yaml_to_json(v: &serde_yaml::Value) -> serde_json::Value {
    match v {
        serde_yaml::Value::Null => serde_json::Value::Null,
        serde_yaml::Value::Bool(b) => serde_json::Value::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                serde_json::json!(i)
            } else if let Some(f) = n.as_f64() {
                serde_json::json!(f)
            } else {
                serde_json::Value::Null
            }
        }
        serde_yaml::Value::String(s) => serde_json::Value::String(s.clone()),
        serde_yaml::Value::Sequence(seq) => {
            let arr: Vec<serde_json::Value> = seq.iter().map(serde_yaml_to_json).collect();
            serde_json::Value::Array(arr)
        }
        serde_yaml::Value::Mapping(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                let key = match k {
                    serde_yaml::Value::String(s) => s.clone(),
                    other => format!("{other:?}"),
                };
                obj.insert(key, serde_yaml_to_json(v));
            }
            serde_json::Value::Object(obj)
        }
        serde_yaml::Value::Tagged(t) => serde_yaml_to_json(&t.value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_basic() {
        let content = "---\nname: test\ndescription: A test skill\n---\nBody content here";
        let def = parse_skill_frontmatter(content);
        assert_eq!(def.name, "test");
        assert_eq!(def.description, "A test skill");
        assert_eq!(def.body, "Body content here");
    }

    #[test]
    fn parse_frontmatter_empty() {
        let content = "---\n---\nBody only";
        let def = parse_skill_frontmatter(content);
        assert_eq!(def.name, "");
        assert_eq!(def.body, "Body only");
    }

    #[test]
    fn parse_frontmatter_no_frontmatter() {
        let content = "Just some markdown content";
        let def = parse_skill_frontmatter(content);
        assert_eq!(def.name, "");
        assert_eq!(def.body, "Just some markdown content");
    }

    #[test]
    fn parse_frontmatter_with_optional_fields() {
        let content = "---\nname: test\nlicense: MIT\ncompatibility: Linux only\n---\nBody";
        let def = parse_skill_frontmatter(content);
        assert_eq!(def.name, "test");
        assert_eq!(def.license, Some("MIT".to_string()));
        assert_eq!(def.compatibility, Some("Linux only".to_string()));
    }

    #[test]
    fn parse_frontmatter_with_metadata() {
        let content = "---\nname: test\nmetadata:\n  key1: value1\n  key2: 42\n---\nBody";
        let def = parse_skill_frontmatter(content);
        assert_eq!(
            def.metadata.get("key1").unwrap(),
            &serde_json::json!("value1")
        );
        assert_eq!(def.metadata.get("key2").unwrap(), &serde_json::json!(42));
    }

    #[test]
    fn find_first_non_heading_works() {
        assert_eq!(
            find_first_non_heading("# Title\nSome description"),
            "Some description"
        );
        assert_eq!(find_first_non_heading("# Title\n## Sub\nDesc"), "Desc");
        assert_eq!(find_first_non_heading(""), "");
        assert_eq!(find_first_non_heading("# Only\n# headings"), "");
    }

    #[test]
    fn find_first_non_heading_with_whitespace() {
        assert_eq!(
            find_first_non_heading("# Title\n  \n  Some description  "),
            "Some description"
        );
    }

    #[test]
    fn truncate_str_works() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello world", 8), "hello...");
        assert_eq!(truncate_str("", 10), "");
        assert_eq!(truncate_str("  hi  ", 10), "hi");
    }

    #[test]
    fn truncate_skill_fields_works() {
        let (name, desc, compat) = truncate_skill_fields("test", "desc", Some("Linux only"));
        assert_eq!(name, "test");
        assert_eq!(desc, "desc");
        assert_eq!(compat, Some("Linux only".to_string()));
    }

    #[test]
    fn truncate_skill_fields_truncates() {
        let (name, desc, compat) =
            truncate_skill_fields(&"a".repeat(100), &"b".repeat(2000), Some(&"x".repeat(600)));
        assert!(name.len() <= 64);
        assert!(desc.len() <= 1024);
        assert!(compat.unwrap().len() <= 500);
    }

    #[test]
    fn truncate_skill_fields_empty_compat() {
        let (_, _, compat) = truncate_skill_fields("test", "desc", Some(""));
        assert_eq!(compat, None);

        let (_, _, compat) = truncate_skill_fields("test", "desc", Some("   "));
        assert_eq!(compat, None);
    }

    #[test]
    fn scan_linked_files_empty_dir() {
        let dir = std::env::temp_dir().join("fuyao_test_scan_linked_empty");
        std::fs::create_dir_all(&dir).unwrap();
        let result = scan_linked_files(&dir);
        assert!(result.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_linked_files_with_scripts() {
        let dir = std::env::temp_dir().join("fuyao_test_scan_linked_scripts");
        let scripts = dir.join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(scripts.join("setup.sh"), "#!/bin/bash").unwrap();

        let result = scan_linked_files(&dir);
        assert!(result.contains_key("scripts"));
        assert_eq!(result["scripts"].len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
