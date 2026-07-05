//! 模型和供应商模块
//!
//! 定义 LLM 模型配置和供应商信息。
//! - `model`: 模型配置（Model、ModelCost、ModelLimit、ModelModalities、PriceTier）
//! - `provider`: 供应商配置（Provider、ProviderOptions）

pub mod model;
pub mod provider;

pub use model::{Model, ModelCost, ModelLimit, ModelModalities, PriceTier};
pub use provider::{Provider, ProviderOptions};
