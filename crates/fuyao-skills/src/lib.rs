//! Skills 模块
//!
//! 符合 Agent Skills 官方规范 (<https://agentskills.io/specification>)
//!
//! 提供 Skill 发现、加载、解析功能。
//! - `finder`: 扫描三层目录发现 Skill
//! - `loader`: 按需加载 Skill 完整内容和关联文件
//! - `helpers`: frontmatter 解析、关联文件扫描

mod error;
mod finder;
mod helpers;
mod loader;

pub use error::SkillsError;
pub use finder::{find_all_skills, find_skill_md_by_name};
pub use fuyao_api::{SkillDefinition, SkillMeta};
pub use loader::{load_skill, load_skill_file};
