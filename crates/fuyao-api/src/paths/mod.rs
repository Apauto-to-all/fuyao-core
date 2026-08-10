//! Fuyao 路径系统
//!
//! 无依赖模块，可被任何模块安全导入。
//!
//! 包含：
//! - 三层目录架构的基础路径函数
//! - 分层路径结构（LayeredPaths）
//! - 工具并行策略（路径冲突检测）
//! - 工作目录路径归一化
//!
//! 三层目录架构：
//! - Layer 1: 全局层 (`~/.fuyao/`)
//! - Layer 2: Agent 目录层 (`~/.fuyao/fuyao-agents/{agent-id}/`)
//! - Layer 3: 工作目录层 (`{workspace}/.fuyao/`)
//!
//! 路径优先级（从高到低）：工作目录 → Agent 目录 → 全局 → 额外目录（extra）

mod global;
mod layered;
mod normalize;
mod workspace;

pub use layered::LayeredPaths;

// 全局层路径
pub use global::{get_fuyao_agents_dir, get_fuyao_home};

// 工作目录层路径
pub use workspace::{get_agent_root, get_workspace_agents_dir, get_workspace_root};

// 工作目录路径归一化
pub use normalize::{normalize_workspace, normalize_workspace_str};
