//! Skills 工具类型定义
//!
//! 定义 Skills 加载工具的结果类型。

use std::collections::HashMap;

/// Skill 元数据（用于列表显示）
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillMetaItem {
    /// Skill 名称
    pub name: String,
    /// Skill 描述
    pub description: String,
}

impl From<&fuyao_api::skill_types::SkillMeta> for SkillMetaItem {
    fn from(meta: &fuyao_api::skill_types::SkillMeta) -> Self {
        Self {
            name: meta.name.clone(),
            description: meta.description.clone(),
        }
    }
}

/// Skill 列表结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillListResult {
    /// Skill 列表
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillMetaItem>,
    /// Skill 数量
    pub count: usize,
    /// 消息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 提示
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Skill 详情结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillViewResult {
    /// Skill 名称
    pub name: String,
    /// Skill 描述
    pub description: String,
    /// Skill 内容
    pub content: String,
    /// 许可证
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// 兼容性
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    /// 元数据
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, serde_json::Value>>,
    /// 关联文件
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_files: Option<HashMap<String, Vec<String>>>,
    /// 使用提示
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_hint: Option<String>,
    /// Skill 目录绝对路径
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_dir: Option<String>,
}

/// Skill 关联文件结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillFileResult {
    /// 文件内容
    pub content: String,
}
