//! 供应商管理域的功能底座：写回落存储原语、载荷类型与校验
//!
//! 与 `registry`（运行时注册缓存）、`resolver`（读侧字段解析）正交的第三个面
//! 的**功能实现层**：管理门面（`ProviderManager` 的列举与 CRUD 编排）不在本
//! crate——由装配层（fuyao-app 的 `provider_manager` 模块）承载，本域向其提供
//! 全部底层原语。模型是供应商的组成内容（聚合成员），随供应商载荷整体写入
//! 与替换，不设独立的模型接口。写盘结果满足配置加载的必填校验规则
//! （fail-loud）与 `api_key_env_vars` 指针解析链，写前读后语义一致。
//!
//! 模块内分工（扁平文件，各管一件事）：
//! - [`error`]：管理面公开错误（面向最终用户，含修正建议）与配置加载错误映射
//! - [`spec`]：写回载荷类型（[`ProviderSpec`]）与入参校验（fail-loud，写入前拦下）
//! - [`global_store`]：global 层两落点（fuyao.toml / .env）的文件定位与读写句柄
//! - [`toml_patch`]：fuyao.toml 段级变更原语（insert / patch / remove）
//! - [`serialize`]：领域对象 → TOML 表的序列化（含 models 整表替换）
//! - [`env_file`]：.env 单行级原语（格式化 / upsert / 组合备写）
//!
//! # 写回策略
//!
//! - **落点只在 global 层**（`~/.fuyao/fuyao.toml`、`~/.fuyao/.env`）：供应商是
//!   全局资源；agent / workspace 层是高级用户手写领地，管理 API 不触碰。
//! - **fuyao.toml 增量 patch**（toml_edit）：只动目标 `[providers.<id>]` 子树，
//!   其余注释、未知字段、手写格式逐字保留；段内 `models` 子表按载荷**整表
//!   替换**（未携带的模型消失、载荷 id 即落盘键，模型 id 因此可变更）。
//! - **密钥隔离**：api_key 明文只进 global 层 `.env`（单行级读-改-写，用户
//!   手写的其他变量与注释不动）；toml 里只写 `api_key_env_vars` 指针。
//! - **变量名用户自设**：`api_key_env_var` 与供应商 id 解耦（id 变更不影响
//!   变量名），明文 upsert 同名覆盖、异名新加；.env 只增改不删除——变量与
//!   值均归用户持有，删除供应商也不清理 .env。
//!
//! # 供应商 id 不可变
//!
//! 供应商 id（`[providers.<id>]` 键）是会话缓存与历史引用的字符串锚点，
//! **创建后不可改名**——API 签名以 id 定位目标、不接受新 id，换 id 需求以
//! 「建新（沿用原变量名，.env 密钥行不动）+ 删旧」组合满足。模型 id 不是
//! 独立锚点，随 models 整表替换自由变更。

mod env_file;
mod error;
mod global_store;
mod serialize;
mod spec;
mod toml_patch;

pub use env_file::{format_env_line, prepare_env_upsert, upsert_env_line};
pub use error::{ProviderAdminError, map_config_error};
pub use global_store::GlobalStore;
pub use spec::{
    ProviderModelSpec, ProviderSpec, ProviderSpecData, validate_provider_id, validate_spec,
};
pub use toml_patch::{insert_provider, patch_provider, remove_provider};
