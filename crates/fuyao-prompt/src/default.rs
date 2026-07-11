//! 默认 Fuyao Agent 定义
//!
//! 框架内置的默认 Agent，硬编码确保框架稳定性。
//! 用户可通过 `agents/default.md` 覆盖此定义。

use fuyao_api::{AgentDefinition, AgentMode};
use std::sync::LazyLock;

/// 默认系统提示词
const DEFAULT_SYSTEM_PROMPT: &str = r#"# Fuyao Agent

你是 Fuyao（扶摇），一个智能 AI 助手。

## 核心能力
- 代码编写、审查和重构
- 文件系统操作（读取、写入、搜索）
- 问题分析和解决方案设计
- 技术文档编写

## 工作原则
- **自主决策**：有明确任务时，使用工具获取信息，而非询问用户
- **持续执行**：完成任务直到真正完成，不中途停止
- **验证结果**：执行操作后验证结果是否符合预期
- **清晰沟通**：简洁明了地汇报进展和结果

## 行为准则
- 谨慎处理敏感操作（删除、覆盖），必要时先备份
- 遇到错误时分析原因并调整策略，不轻易放弃
- 保持代码风格与项目现有风格一致
- 使用工具前了解其功能，选择最合适的工具"#;

/// 默认 Agent 定义（全局单例）
pub static DEFAULT_FUYAO_AGENT: LazyLock<AgentDefinition> = LazyLock::new(|| AgentDefinition {
    name: "fuyao".to_string(),
    description: "Fuyao 默认助手".to_string(),
    version: "1.0.0".to_string(),
    author: "Fuyao".to_string(),
    mode: AgentMode::All,
    system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
    source_path: None,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_agent_has_correct_name() {
        assert_eq!(DEFAULT_FUYAO_AGENT.name, "fuyao");
    }

    #[test]
    fn default_agent_has_system_prompt() {
        assert!(!DEFAULT_FUYAO_AGENT.system_prompt.is_empty());
        assert!(DEFAULT_FUYAO_AGENT.system_prompt.contains("Fuyao"));
    }

    #[test]
    fn default_agent_is_cloneable() {
        let clone = DEFAULT_FUYAO_AGENT.clone();
        assert_eq!(clone.name, DEFAULT_FUYAO_AGENT.name);
    }
}
