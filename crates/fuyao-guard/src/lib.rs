//! 防护模块
//!
//! 提供 AI 行为安全防护功能：
//! - `loop_guard`: 循环检测插件，防止 AI 重复执行相同操作或输出重复内容
// TODO：循环防护插件有一个问题，AI连续调用触发最高级别的中断取消，后续再让其调用工具的试试，直接无工具结果，这个是有问题的
pub mod loop_guard;

pub use fuyao_api::LoopGuardConfig;
pub use loop_guard::LoopGuardPlugin;
