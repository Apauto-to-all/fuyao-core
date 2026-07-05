//! Agent 运行时类型定义

use crate::agent::AgentPaths;
use crate::provider::ThinkingType;
use std::collections::HashSet;
use std::sync::Arc;

/// 共享 Agent 上下文
pub type SharedAgentCtx = Arc<std::sync::Mutex<AgentContext>>;

/// 模型运行配置
///
/// 聚合「用哪个模型」+「怎么思考」三个运行时模型参数。
/// 三字段默认 `None`，请求体不发对应字段，走模型自身默认行为。
#[derive(Debug, Clone, Default)]
pub struct ModelConfig {
    /// 模型 ID，如 aliyun/qwen3.6-plus。None 时自动使用默认模型
    pub model_id: Option<String>,

    /// 思考开关（对应 thinking.type 字段）。None 时不发，走模型默认
    pub thinking_type: Option<ThinkingType>,

    /// 思考强度档位名（用户自定义字符串，透传给服务器）。None 时不发，走模型默认
    pub reasoning_effort: Option<String>,
}

/// Agent 运行上下文
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    /// 模型运行配置（聚合模型选择 + 思考控制）
    pub model_config: ModelConfig,

    /// 要加载的 Session ID，None 时创建新 Session
    pub session_id: Option<String>,

    /// Agent 三层目录的身份证明，默认使用全局层
    pub agent_paths: AgentPaths,

    /// 工具运行器配置
    pub tool_runner_config: ToolRunnerConfig,
}

/// 工具运行器配置
///
/// 定义工具执行策略：并行规则、最大并发数等。
///
/// 判断顺序：
/// 1. never_parallel_tools → 强制串行
/// 2. path_scoped_tools → 检查路径重叠，重叠则串行，否则跳过后续检查
/// 3. parallel_safe_tools → 在此列表则可并行，否则串行
#[derive(Debug, Clone)]
pub struct ToolRunnerConfig {
    /// 最大并发执行的工具数量
    pub max_concurrent: u32,

    /// 必须串行执行的工具（如需要用户交互）
    pub never_parallel_tools: HashSet<String>,

    /// 只读工具，无共享可变状态，可安全并行
    ///
    /// 注意：path_scoped_tools 中的工具会先做路径检查，检查通过后跳过此检查。
    /// 例如 read 同时在两个列表中，但只走 path_scoped_tools 检查路径重叠，
    /// 不会走到 parallel_safe_tools 的通用检查。
    pub parallel_safe_tools: HashSet<String>,

    /// 文件工具，可并行但要检查路径是否重叠
    ///
    /// 注意：这些工具会先做特殊检查（路径重叠），检查通过后跳过 parallel_safe_tools 检查。
    /// 例如 read 在此列表中，会检查路径是否重叠，重叠则串行，不重叠则可并行。
    pub path_scoped_tools: HashSet<String>,
}

impl Default for ToolRunnerConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 8,
            never_parallel_tools: HashSet::from(["bash".to_string(), "todowrite".to_string()]),
            parallel_safe_tools: HashSet::from([
                "read".to_string(),
                "glob".to_string(),
                "grep".to_string(),
                "skill".to_string(),
                "webfetch".to_string(),
            ]),
            path_scoped_tools: HashSet::from([
                "read".to_string(),
                "write".to_string(),
                "edit".to_string(),
            ]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_context_default_has_no_ids() {
        let ctx = AgentContext::default();
        assert!(ctx.model_config.model_id.is_none());
        assert!(ctx.session_id.is_none());
        assert!(ctx.agent_paths.agent_id.is_none());
    }

    #[test]
    fn model_config_default_all_none() {
        let config = ModelConfig::default();
        assert!(config.model_id.is_none());
        assert!(config.thinking_type.is_none());
        assert!(config.reasoning_effort.is_none());
    }

    #[test]
    fn tool_runner_config_default_max_concurrent_is_8() {
        let config = ToolRunnerConfig::default();
        assert_eq!(config.max_concurrent, 8);
    }

    #[test]
    fn tool_runner_config_default_never_parallel_contains_bash() {
        let config = ToolRunnerConfig::default();
        assert!(config.never_parallel_tools.contains("bash"));
        assert!(config.never_parallel_tools.contains("todowrite"));
    }

    #[test]
    fn tool_runner_config_default_parallel_safe_contains_read() {
        let config = ToolRunnerConfig::default();
        assert!(config.parallel_safe_tools.contains("read"));
        assert!(config.parallel_safe_tools.contains("glob"));
        assert!(config.parallel_safe_tools.contains("grep"));
    }

    #[test]
    fn tool_runner_config_default_path_scoped_contains_file_tools() {
        let config = ToolRunnerConfig::default();
        assert!(config.path_scoped_tools.contains("read"));
        assert!(config.path_scoped_tools.contains("write"));
        assert!(config.path_scoped_tools.contains("edit"));
    }
}
