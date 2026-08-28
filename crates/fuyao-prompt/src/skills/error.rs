//! Skills 错误类型

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SkillsError {
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML 解析错误: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("Skill 未找到: {0}")]
    NotFound(String),

    #[error("路径遍历攻击: {0}")]
    PathTraversal(String),
}
