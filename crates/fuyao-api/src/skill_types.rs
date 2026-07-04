//! Skills 类型定义
//!
//! 符合 Agent Skills 官方规范 (https://agentskills.io/specification)

use std::collections::HashMap;

/// 允许的关联文件子目录（符合官方规范）
pub const LINKED_SUBDIRS: &[&str] = &["scripts", "references", "assets"];

/// Skill 元数据（渐进披露 Tier 1：最小信息）
///
/// 用于 skill_list 返回，节省 token。
/// 符合官方规范的必填字段。
#[derive(Debug, Clone)]
pub struct SkillMeta {
    /// Skill 名称（最多 64 字符）
    pub name: String,
    /// Skill 描述（最多 1024 字符）
    pub description: String,
}

impl SkillMeta {
    /// 创建 SkillMeta，自动截断超长字段
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: truncate_str(&name.into(), 64),
            description: truncate_str(&description.into(), 1024),
        }
    }
}

/// 截断字符串到指定长度，超长加省略号
fn truncate_str(s: &str, max_len: usize) -> String {
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

/// Skill 完整定义（渐进披露 Tier 2：完整内容）
///
/// 用于 skill_view 返回，包含完整内容。
/// 符合 Agent Skills 官方规范。
#[derive(Debug, Clone)]
pub struct SkillDefinition {
    // —— 官方规范字段 ——
    /// Skill 名称（最多 64 字符）
    pub name: String,
    /// Skill 描述（最多 1024 字符）
    pub description: String,
    /// 许可证名称或引用
    pub license: Option<String>,
    /// 环境要求描述（最多 500 字符）
    pub compatibility: Option<String>,
    /// 扩展元数据
    pub metadata: HashMap<String, serde_json::Value>,

    // —— Markdown 内容 ——
    /// SKILL.md 内容，不包含 frontmatter
    pub body: String,

    // —— 实现细节字段 ——
    /// Skill 目录绝对路径
    pub skill_dir: Option<String>,
    /// 关联文件映射 {subdir_name: [relative_path, ...]}
    pub linked_files: HashMap<String, Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_meta_new_truncates() {
        let meta = SkillMeta::new("a".repeat(100), "b".repeat(2000));
        assert!(meta.name.len() <= 64);
        assert!(meta.description.len() <= 1024);
    }

    #[test]
    fn skill_meta_new_empty() {
        let meta = SkillMeta::new("", "");
        assert_eq!(meta.name, "");
        assert_eq!(meta.description, "");
    }
}
