//! 防护模块
//!
//! 提供 AI 行为安全防护功能：
//! - `loop_guard`: 循环检测插件，防止 AI 重复执行相同操作或输出重复内容
mod loop_guard;

pub use fuyao_api::LoopGuardConfig;
pub use loop_guard::LoopGuardPlugin;
