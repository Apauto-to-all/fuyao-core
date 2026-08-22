//! 独立管理入口：不依赖引擎装配
//!
//! 仅凭路径身份即可用，[`crate::start`] 前后任何时机可调。

mod agent_ids;
mod provider_manager;

pub use agent_ids::list_agent_ids;
pub use provider_manager::{ProviderAdminError, ProviderManager, ProviderModelSpec, ProviderSpec};
