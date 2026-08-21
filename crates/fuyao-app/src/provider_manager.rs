//! 供应商管理门面：供应商与模型粒度的创建 / 更新 / 删除（写回 global 层）
//!
//! 与 [`crate::SessionManager`]（会话管理门面）、[`crate::Discovery`]（选择支持
//! 门面）平级正交的第三个管理面：供应商与模型配置是**全局资源**（跨工作区
//! 共享），本门面把「领域存储 = global 层 fuyao.toml + .env」的增量写回封装成
//! provider / model 两级 CRUD；写盘结果满足配置加载的必填校验规则（fail-loud）
//! 与 `api_key_env_vars` 指针解析链，写前读后语义一致。
//!
//! # 写回策略
//!
//! - **落点只在 global 层**（`~/.fuyao/fuyao.toml`、`~/.fuyao/.env`）：供应商是
//!   全局资源；agent / workspace 层是高级用户手写领地，管理 API 不触碰。
//! - **fuyao.toml 增量 patch**（toml_edit）：只动目标 `[providers.<id>]` 子树，
//!   其余注释、未知字段、手写格式逐字保留。
//! - **密钥隔离**：api_key 明文只进 global 层 `.env`（单行级读-改-写，用户
//!   手写的其他变量不动）；toml 里只写 `api_key_env_vars` 指针，变量名按
//!   `{供应商 id 大写}_API_KEY` 约定生成。目标变量已有不同值时返回冲突信号
//!   （`EnvKeyConflict`，不静默覆盖），由调用方决定是否以 `overwrite_api_key`
//!   确认覆盖后重试。
//!
//! # id 不可变
//!
//! 供应商 id（`[providers.<id>]` 键）与模型 id（`models` 子表的键）是会话缓存
//! 与历史引用的字符串锚点，**创建后不可改名**——API 签名以 id 定位目标、不
//! 接受新 id，改名需求以「删旧建新」满足。
//!
//! # 写入不等于立即生效
//!
//! 本门面只负责写盘。写盘结果经配置加载（`load_config`）读回语义一致；
//! 存活 engine 的「立即可用」由引擎侧运行时注册原语（`Engine::register_provider`
//! / `Engine::unregister_provider`，内部完成缓存注册 + 实例表插入）承接，
//! 在全部存活 engine 上的刷新编排属装配层职责，不在本门面内。

use std::collections::HashMap;
use std::fmt;

use fuyao_api::{
    AgentPaths, Model, ModelOption, ProviderModelOption, ProviderOption, ProviderSource,
    load_config, load_provider_sources_from,
};
use toml_edit::{Array, Item, Table};

use crate::provider_store::{
    self, EnvWriteError, GlobalStore, ProviderSpecData, generated_env_var_name, model_to_table,
    provider_to_table, providers_table_mut,
};

/// 供应商管理错误
///
/// 每个变体面向最终用户（含明确修正建议）；`EnvKeyConflict` 携带变量名而非
/// 已有值——已存在的 api_key 可能是用户手写密钥，不进错误信息（敏感信息红线）。
#[derive(Debug, thiserror::Error)]
pub enum ProviderAdminError {
    /// 入参校验失败（id 非法、必填字段缺失、limit.context 非正整数等）
    #[error("供应商配置校验失败: {0}")]
    Invalid(String),

    /// 创建目标已存在（供应商 / 模型 id 冲突）
    #[error("供应商配置已存在: {0}")]
    AlreadyExists(String),

    /// 更新 / 删除的目标不存在
    #[error("供应商配置不存在: {0}")]
    NotFound(String),

    /// .env 目标变量已有不同的值且未确认覆盖（携带变量名，由调用方决定）
    #[error("环境变量 {0} 已有手写值，未确认覆盖（以 overwrite_api_key = true 重试可覆盖）")]
    EnvKeyConflict(String),

    /// global 层 fuyao.toml 解析失败（写回前需人工修复）
    #[error("global 层 fuyao.toml 解析失败: {0}")]
    TomlParse(String),

    /// 段结构无法承载写回（providers 段 / 目标条目不是 table、点键写法等）
    #[error("配置段结构非法: {0}")]
    InvalidSection(String),

    /// 文件读写失败
    #[error("配置文件读写失败: {0}")]
    Io(String),
}

/// 供应商写回载荷（create / update 共用）
///
/// `api_key` 为 `None` 时不写 .env（密钥保持现状）；`Some` 表示把明文
/// 写入 global 层 `.env` 的约定变量并同步 toml 指针（若 toml 段内残留
/// `options.api_key` 明文，一并移除——明文密钥不进 toml）。
///
/// `base_url` / `name` 为完整期望状态：update 时 `base_url = None` 表示清除
/// 该项（调用方提交表单的完整状态，而非增量）。
pub struct ProviderSpec {
    /// 供应商显示名（必填）
    pub name: String,
    /// 自定义 base URL（None = 不配置 / 清除）
    pub base_url: Option<String>,
    /// API Key 明文（None = 不动 .env）
    pub api_key: Option<String>,
    /// .env 目标变量已有不同值时是否覆盖（false = 返回冲突信号）
    pub overwrite_api_key: bool,
}

impl fmt::Debug for ProviderSpec {
    /// api_key 不出现在 Debug 输出中（敏感信息红线），以占位符标注有无
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderSpec")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field(
                "api_key",
                &if self.api_key.is_some() {
                    "<已隐藏>"
                } else {
                    "<未提供>"
                },
            )
            .field("overwrite_api_key", &self.overwrite_api_key)
            .finish()
    }
}

/// 供应商管理器：封装 global 层 fuyao.toml / .env 的供应商与模型增量写回
///
/// 持有 [`AgentPaths`]（构造时注入一次）：路径身份决定写回落点
/// （`fuyao_home` 下的 global 层文件）与模型列举（Provider 注册缓存按
/// agent_paths 维度隔离）。与 [`crate::SessionManager`] 同属「独立管理面
/// 封装领域存储 CRUD」的组织形态。
pub struct ProviderManager {
    /// 路径身份（启动时注入，供应商管理全程只读）
    agent_paths: AgentPaths,
    /// global 层写回落存储句柄（fuyao.toml + .env）
    store: GlobalStore,
}

impl ProviderManager {
    /// 构造供应商管理门面
    ///
    /// 公开构造：管理操作是纯文件写回 + 注册缓存读取，不依赖引擎装配，上层
    /// 可在引擎启动前独立使用。
    pub fn new(agent_paths: AgentPaths) -> Self {
        let store = GlobalStore::new(&agent_paths.fuyao_home);
        Self { agent_paths, store }
    }

    // ── 模型列举（来自 Provider / Model 注册缓存）────────────────

    /// 列举可选 model（来自 Provider / Model 注册缓存，仅启动后有内容）
    ///
    /// 启动前（未跑 [`crate::init_engine`]）注册缓存为空，返回空列表。
    /// `id` 为纯模型名、`provider_id` 独立字段；调用方按需拼成 `provider_id/id`
    /// 设给 model_id。`provider_name`（供应商显示名）来自同链注册的 Provider
    /// 配置缓存，仅供展示，不参与身份与路由。
    ///
    /// 顺序契约：按 `provider_id` 字母序分组，组内按模型 `id` 字母序——注册缓存为
    /// HashMap（遍历序每次进程启动随机），排序保证列表跨启动稳定，消费方（UI
    /// 下拉等）可直接沿用本序呈现。
    ///
    /// 本列表反映**注册缓存**（引擎装配时写入），面向模型选择场景、不携带来源；
    /// 供应商管理场景（需来源层标注、写盘后即时可见）用
    /// [`ProviderManager::list_providers_with_source`]。
    pub fn list_models(&self) -> Vec<ModelOption> {
        // Provider 注册缓存建 id → 显示名映射；key 两边均为注册时的小写化形态，直接命中
        let provider_names: HashMap<String, String> =
            fuyao_provider::list_providers(&self.agent_paths)
                .into_iter()
                .map(|(id, provider)| (id, provider.name))
                .collect();
        let mut options: Vec<ModelOption> = fuyao_provider::list_models(&self.agent_paths)
            .into_iter()
            .map(|(full_id, model)| {
                // 缓存 key 形如 "provider/model"，拆成独立 provider 与纯模型 id
                let (provider, id) = match full_id.split_once('/') {
                    Some((p, i)) => (p.to_string(), i.to_string()),
                    None => (String::new(), full_id),
                };
                // Provider 与 Model 在装配链同批注册，正常必命中；缓存异常缺 Provider
                // 时退回 id，保证显示名字段始终有值
                let provider_name = provider_names
                    .get(&provider)
                    .cloned()
                    .unwrap_or_else(|| provider.clone());
                ModelOption {
                    id,
                    provider_id: provider,
                    provider_name,
                    model,
                }
            })
            .collect();
        // 注册 key 为小写形态，普通字节序比较即字典序；先供应商后模型，两层排序
        options.sort_by(|a, b| (&a.provider_id, &a.id).cmp(&(&b.provider_id, &b.id)));
        options
    }

    // ── 供应商管理列举（带来源层，直读三层落盘）──────────────────

    /// 列举全部供应商与旗下模型，每个实体标注来源层（global / agent / workspace）
    ///
    /// 供应商管理面的数据基础：三层深合并中 global 层优先级最低，来源不明的
    /// 盲写会出现「UI 改了不生效」陷阱——本列表让各实体的定义层对调用方诚实
    /// 可见，非 global 层实体据此只读展示。
    ///
    /// 数据流：实时读三层落盘（合并加载取完整值 + 来源判定取各实体定义层），
    /// **不经注册缓存**——本门面写盘的新增 / 修改即刻反映在列表中，无需等
    /// 缓存刷新（与 [`ProviderManager::list_models`] 的缓存路径正交）。
    ///
    /// 来源判定语义：实体（供应商键 / 模型键）在多层出现时标**最高优先级层**
    /// （与深合并覆盖方向一致）；供应商与旗下模型各自独立判定（同一供应商下
    /// 不同模型可来自不同层）。
    ///
    /// 顺序契约：供应商按 id 字母序，组内模型按 id 字母序（与 `list_models`
    /// 的排序口径一致，跨启动稳定）。
    ///
    /// # 错误
    /// 三层配置加载失败（TOML 语法 / IO / providers 段校验）整体 fail-loud，
    /// 映射为 [`ProviderAdminError`]（`TomlParse` / `Io` / `Invalid`）——坏配置
    /// 下列表不可用与 CRUD 一致，引导用户先修复配置。
    pub fn list_providers_with_source(&self) -> Result<Vec<ProviderOption>, ProviderAdminError> {
        // 合并加载取完整值（含 providers 段的 fail-loud 校验）；无任何配置 = 空列表
        let config = load_config(&self.agent_paths).map_err(map_config_error)?;
        let providers = config.map(|c| c.providers).unwrap_or_default();

        // 来源判定与合并加载走同一组三层路径，键形态与 providers 输出一致
        let sources = load_provider_sources_from(&self.agent_paths).map_err(map_config_error)?;

        let mut options: Vec<ProviderOption> = providers
            .into_iter()
            .map(|(id, provider)| {
                // 旗下模型：独立判定来源，按模型 id 字母序
                let mut models: Vec<ProviderModelOption> = provider
                    .models
                    .into_iter()
                    .map(|(model_id, model)| ProviderModelOption {
                        id: model_id.clone(),
                        // 判定表按原始键命中；极端场景（判定时层文件已变）缺项
                        // 回退 Global——三层里只要定义过至少 global 在合并结果中
                        source: sources
                            .models
                            .get(&format!("{id}/{model_id}"))
                            .copied()
                            .unwrap_or(ProviderSource::Global),
                        model,
                    })
                    .collect();
                models.sort_by(|a, b| a.id.cmp(&b.id));

                ProviderOption {
                    source: sources
                        .providers
                        .get(&id)
                        .copied()
                        .unwrap_or(ProviderSource::Global),
                    id,
                    name: provider.name,
                    base_url: provider.options.base_url,
                    api_key_env_vars: provider.api_key_env_vars,
                    models,
                }
            })
            .collect();
        options.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(options)
    }

    // ── 供应商粒度 CRUD ──────────────────────────────────────────

    /// 创建供应商：global 层 fuyao.toml 新增 `[providers.<id>]` 段
    ///
    /// - id / 必填字段先校验（fail-loud），已存在的 id 拒绝创建
    /// - toml 段内写 `name` / `options.base_url`（有则写）/ `api_key_env_vars`
    ///   指针（约定变量 `{ID大写}_API_KEY`）；**不写 `options.api_key` 明文**
    /// - `spec.api_key` 为 `Some` 时把明文写入 global 层 `.env` 的约定变量
    ///   （单行增量）；目标变量已有不同值且未确认覆盖 → 整个创建失败返回
    ///   [`ProviderAdminError::EnvKeyConflict`]，**两个文件都不动**
    ///   （冲突在写盘前检查，不留半写状态）
    ///
    /// # 返回
    /// 生成的 api_key 环境变量名（供表单只读展示）。
    pub fn create_provider(
        &self,
        provider_id: &str,
        spec: ProviderSpec,
    ) -> Result<String, ProviderAdminError> {
        validate_provider_id(provider_id)?;
        validate_provider_name(&spec.name)?;
        if let Some(api_key) = &spec.api_key {
            validate_api_key(api_key)?;
        }

        let env_var = generated_env_var_name(provider_id);

        // 冲突前置检查（写盘前）：.env 目标变量已有不同值 → 直接失败，不留半写状态
        let env_write = self.prepare_env_write(&env_var, &spec)?;

        let mut doc = self.store.read_toml()?;
        let providers = providers_table_mut(&mut doc)?;
        if providers.contains_key(provider_id) {
            return Err(ProviderAdminError::AlreadyExists(format!(
                "providers.{provider_id} 已存在（id 不可改名，如需换 id 请删除后新建）"
            )));
        }
        let data = ProviderSpecData {
            name: spec.name,
            base_url: spec.base_url,
        };
        let table = provider_to_table(&data, &env_var);
        providers.insert(provider_id, Item::Table(table));
        self.store.write_toml(&doc)?;

        self.commit_env_write(env_write)?;
        tracing::info!(
            provider_id = %provider_id,
            api_key_env_var = %env_var,
            "供应商已创建（写回 global 层 fuyao.toml）"
        );
        Ok(env_var)
    }

    /// 更新供应商：patch 目标 `[providers.<id>]` 段内的管理字段
    ///
    /// 完整期望状态语义：`name` / `base_url` 覆盖为载荷值（`base_url = None`
    /// 清除该项，options 子表随之在空时移除）；段内其他键（models、用户
    /// 手写的未知字段）不动。
    ///
    /// `api_key` 为 `Some` 时：明文写 .env 约定变量（冲突语义同
    /// [`ProviderManager::create_provider`]，写盘前检查），同时移除段内残留的
    /// `options.api_key` 明文并把约定变量并入 `api_key_env_vars`（用户自加的
    /// 其他变量保留，只做去重追加）——保持两文件指针一致。
    ///
    /// id 经签名定位，API 不提供改名；目标不存在返回 `NotFound`。
    pub fn update_provider(
        &self,
        provider_id: &str,
        spec: ProviderSpec,
    ) -> Result<(), ProviderAdminError> {
        validate_provider_id(provider_id)?;
        validate_provider_name(&spec.name)?;
        if let Some(api_key) = &spec.api_key {
            validate_api_key(api_key)?;
        }

        let env_var = generated_env_var_name(provider_id);
        let env_write = self.prepare_env_write(&env_var, &spec)?;

        let mut doc = self.store.read_toml()?;
        let providers = providers_table_mut(&mut doc)?;
        let table = crate::provider_store::provider_table_mut(providers, provider_id)?.ok_or_else(
            || ProviderAdminError::NotFound(format!("providers.{provider_id} 不存在")),
        )?;

        // name：覆盖为载荷值
        table.insert("name", toml_edit::value(spec.name));

        // base_url：Some 覆盖 / None 清除（options 兼容表头与内联两种手写形态，
        // 空则整体移除）
        match &spec.base_url {
            Some(base_url) => {
                options_set_base_url(table, provider_id, base_url)?;
            }
            None => options_remove_key(table, "base_url"),
        }

        // api_key：明文走 .env，段内只保指针——移除残留明文（含内联形态）并把
        // 约定变量并入指针
        if spec.api_key.is_some() {
            options_remove_key(table, "api_key");
            let existing = crate::provider_store::read_provider_pointers(table);
            let mut vars: Vec<String> = existing.api_key_env_vars;
            if !vars.iter().any(|v| v == &env_var) {
                vars.push(env_var.clone());
            }
            let mut arr = Array::new();
            for var in vars {
                arr.push(var);
            }
            table.insert("api_key_env_vars", toml_edit::value(arr));
        }

        self.store.write_toml(&doc)?;
        self.commit_env_write(env_write)?;
        tracing::info!(
            provider_id = %provider_id,
            "供应商已更新（patch global 层 fuyao.toml 目标段）"
        );
        Ok(())
    }

    /// 删除供应商：移除 `[providers.<id>]` 段（级联删其全部模型）并同步清理 .env
    ///
    /// toml 侧只移除目标段（其他供应商与全局其他配置逐字保留；providers 段
    /// 删空后连带移除空表头）；.env 侧清理约定变量 `{ID大写}_API_KEY` 的那一行
    /// （该变量是管理 API 的产物；用户手写引用的其他变量不属管理面产物，不删）。
    ///
    /// 既有会话对该供应商的引用不受删除阻挡（运行中 turn 跑完即止，后续取用
    /// 报错由上层兜底）——删除后果的告知属调用方确认流程。
    pub fn delete_provider(&self, provider_id: &str) -> Result<(), ProviderAdminError> {
        validate_provider_id(provider_id)?;

        let mut doc = self.store.read_toml()?;
        let providers = providers_table_mut(&mut doc)?;
        if providers.remove(provider_id).is_none() {
            return Err(ProviderAdminError::NotFound(format!(
                "providers.{provider_id} 不存在"
            )));
        }
        // providers 段删空后连带移除键，避免残留空 [providers] 表头
        if providers.is_empty() {
            doc.as_table_mut().remove("providers");
        }
        self.store.write_toml(&doc)?;

        // .env 同步清理约定变量（无该行时原样跳过；跨行值拒绝删除见 EnvWriteError）
        let env_var = generated_env_var_name(provider_id);
        let content = self.store.read_env()?;
        let next =
            provider_store::remove_env_line(&content, &env_var).map_err(map_env_write_error)?;
        if next != content {
            self.store.write_env(&next)?;
        }
        tracing::info!(
            provider_id = %provider_id,
            "供应商已删除（含全部模型与 .env 密钥变量）"
        );
        Ok(())
    }

    // ── 模型粒度 CRUD ────────────────────────────────────────────

    /// 在既有供应商下创建模型：新增 `[providers.<id>.models.<mid>]` 表
    ///
    /// 模型必填校验（name 非空、limit.context 正整数，fail-loud）；供应商不
    /// 存在或模型 id 已占用时报错。
    pub fn create_model(
        &self,
        provider_id: &str,
        model_id: &str,
        model: Model,
    ) -> Result<(), ProviderAdminError> {
        validate_provider_id(provider_id)?;
        validate_model_id(model_id)?;
        validate_model(&model)?;

        let mut doc = self.store.read_toml()?;
        let table = self.provider_table_or_not_found(&mut doc, provider_id)?;
        let models = crate::provider_store::models_table_mut(table, provider_id)?;
        if models.contains_key(model_id) {
            return Err(ProviderAdminError::AlreadyExists(format!(
                "providers.{provider_id}.models.{model_id} 已存在（id 不可改名，如需换 id 请删除后新建）"
            )));
        }
        models.insert(model_id, Item::Table(model_to_table(&model)));
        self.store.write_toml(&doc)?;
        tracing::info!(
            provider_id = %provider_id,
            model_id = %model_id,
            "模型已创建（写回 global 层 fuyao.toml）"
        );
        Ok(())
    }

    /// 更新模型：整表替换 `[providers.<id>.models.<mid>]`
    ///
    /// 模型段是管理粒度的完整期望状态——载荷即替换后的全量字段（段内此前
    /// 手写的未知字段随之消失；供应商段内其他部分不动）。校验与创建同语义。
    pub fn update_model(
        &self,
        provider_id: &str,
        model_id: &str,
        model: Model,
    ) -> Result<(), ProviderAdminError> {
        validate_provider_id(provider_id)?;
        validate_model_id(model_id)?;
        validate_model(&model)?;

        let mut doc = self.store.read_toml()?;
        let table = self.provider_table_or_not_found(&mut doc, provider_id)?;
        let models = crate::provider_store::models_table_mut(table, provider_id)?;
        if models.get(model_id).is_none() {
            return Err(ProviderAdminError::NotFound(format!(
                "providers.{provider_id}.models.{model_id} 不存在"
            )));
        }
        // 整表替换：先移除旧条目（连带其子表 / 数组表）再插入新表
        models.remove(model_id);
        models.insert(model_id, Item::Table(model_to_table(&model)));
        self.store.write_toml(&doc)?;
        tracing::info!(
            provider_id = %provider_id,
            model_id = %model_id,
            "模型已更新（整表替换目标段）"
        );
        Ok(())
    }

    /// 删除模型：移除 `[providers.<id>.models.<mid>]` 表
    ///
    /// models 子表删空后连带移除空 `models` 键，保持供应商段整洁。
    pub fn delete_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<(), ProviderAdminError> {
        validate_provider_id(provider_id)?;
        validate_model_id(model_id)?;

        let mut doc = self.store.read_toml()?;
        let table = self.provider_table_or_not_found(&mut doc, provider_id)?;
        let models = crate::provider_store::models_table_mut(table, provider_id)?;
        if models.remove(model_id).is_none() {
            return Err(ProviderAdminError::NotFound(format!(
                "providers.{provider_id}.models.{model_id} 不存在"
            )));
        }
        // models 子表删空后连带移除空 `models` 键；provider 表经文档根重新取
        // （上面的 `table` 可变借用已随 `models` 消耗）
        if models.is_empty()
            && let Some(provider_table) = doc
                .as_table_mut()
                .get_mut("providers")
                .and_then(|i| i.as_table_mut())
                .and_then(|t| t.get_mut(provider_id))
                .and_then(|i| i.as_table_mut())
        {
            provider_table.remove("models");
        }
        self.store.write_toml(&doc)?;
        tracing::info!(
            provider_id = %provider_id,
            model_id = %model_id,
            "模型已删除"
        );
        Ok(())
    }

    // ── 内部：写回流程的公共步骤 ─────────────────────────────────

    /// 取目标供应商表，不存在时报 `NotFound`
    fn provider_table_or_not_found<'a>(
        &'a self,
        doc: &'a mut toml_edit::DocumentMut,
        provider_id: &str,
    ) -> Result<&'a mut Table, ProviderAdminError> {
        let providers = providers_table_mut(doc)?;
        crate::provider_store::provider_table_mut(providers, provider_id)?
            .ok_or_else(|| ProviderAdminError::NotFound(format!("providers.{provider_id} 不存在")))
    }

    /// .env 写入的两段式——先备好新内容（含冲突检查），提交推迟到 toml 写盘成功后
    ///
    /// 冲突在写盘前暴露：返回 `Err` 时 fuyao.toml 尚未被触碰，调用方重试或放弃
    /// 都不会留下「toml 有指针、.env 没写」的半状态。`None` 表示不写 .env。
    fn prepare_env_write(
        &self,
        env_var: &str,
        spec: &ProviderSpec,
    ) -> Result<Option<String>, ProviderAdminError> {
        let Some(api_key) = &spec.api_key else {
            return Ok(None);
        };
        let new_line = provider_store::format_env_line(env_var, api_key)?;
        let content = self.store.read_env()?;
        let next =
            provider_store::upsert_env_line(&content, env_var, &new_line, spec.overwrite_api_key)
                .map_err(map_env_write_error)?;
        Ok(Some(next))
    }

    /// 提交 `.env` 写入（toml 已成功落盘后调用）
    fn commit_env_write(&self, prepared: Option<String>) -> Result<(), ProviderAdminError> {
        if let Some(next) = prepared {
            self.store.write_env(&next)?;
        }
        Ok(())
    }
}

/// 把 .env 行级写入错误映射为公开错误变体
fn map_env_write_error(e: EnvWriteError) -> ProviderAdminError {
    match e {
        EnvWriteError::Conflict(var) => ProviderAdminError::EnvKeyConflict(var),
        EnvWriteError::MultiLine(var) => ProviderAdminError::Invalid(format!(
            "环境变量 {var} 的现有值跨多行，无法单行级改写（请手工整理 .env 后重试）"
        )),
    }
}

/// 把三层配置加载错误映射为公开错误变体
///
/// 列表 API 与 CRUD 共用同一 fail-loud 口径：语法坏 → `TomlParse`（先修复才能
/// 继续管理），值校验失败（providers 段 / 模型必填项）→ `Invalid`，IO → `Io`。
fn map_config_error(e: fuyao_api::ConfigError) -> ProviderAdminError {
    match e {
        fuyao_api::ConfigError::TomlError(err) => ProviderAdminError::TomlParse(err.to_string()),
        fuyao_api::ConfigError::InvalidModel(msg) => ProviderAdminError::Invalid(msg),
        fuyao_api::ConfigError::InvalidProvidersSection(msg) => {
            ProviderAdminError::InvalidSection(msg)
        }
        fuyao_api::ConfigError::IoError(err) => ProviderAdminError::Io(err.to_string()),
        fuyao_api::ConfigError::FileNotFound(path) => {
            ProviderAdminError::Io(format!("配置文件不存在: {path}"))
        }
    }
}

/// 把 base_url 写入供应商段的 options 子段
///
/// options 兼容两种手写形态：表头（`[providers.<id>.options]`）与内联
/// （`options = { ... }`），就地 patch 现有形态；不存在时新建表头形态
/// （渲染为 `[providers.<id>.options]`）。options 段是其他形态（标量等）时
/// 拒绝写回。
fn options_set_base_url(
    table: &mut Table,
    provider_id: &str,
    base_url: &str,
) -> Result<(), ProviderAdminError> {
    match table.get_mut("options") {
        Some(Item::Table(options)) => {
            options.insert("base_url", toml_edit::value(base_url));
            Ok(())
        }
        Some(Item::Value(value)) => {
            let Some(inline) = value.as_inline_table_mut() else {
                return Err(ProviderAdminError::InvalidSection(format!(
                    "providers.{provider_id}.options 段不是 table，无法写入 base_url"
                )));
            };
            inline.insert("base_url", base_url.into());
            Ok(())
        }
        _ => {
            let mut options = Table::new();
            options.insert("base_url", toml_edit::value(base_url));
            table.insert("options", Item::Table(options));
            Ok(())
        }
    }
}

/// 从供应商段的 options 子段（表头 / 内联两形态）移除指定键；移除后 options
/// 为空则整体移除该键
fn options_remove_key(table: &mut Table, key: &str) {
    match table.get_mut("options") {
        Some(Item::Table(options)) => {
            options.remove(key);
            if options.is_empty() {
                table.remove("options");
            }
        }
        Some(Item::Value(value)) => {
            if let Some(inline) = value.as_inline_table_mut() {
                inline.remove(key);
                if inline.is_empty() {
                    table.remove("options");
                }
            }
        }
        _ => {}
    }
}

// ── 入参校验（fail-loud）──────────────────────────────────────

/// 供应商 id 校验：非空，字符集限 `[A-Za-z0-9_-]`
///
/// 该字符集同时满足三个消费者：TOML 裸键（无需引号转义）、环境变量名惯例
/// （大写后作 `.env` 变量名主体）、注册缓存的小写化 key。不满足即拒绝，
/// 不做自动改写（id 是身份锚点，静默变形会造成写盘键与调用方预期不一致）。
fn validate_provider_id(provider_id: &str) -> Result<(), ProviderAdminError> {
    if provider_id.is_empty() {
        return Err(ProviderAdminError::Invalid(
            "供应商 id 不能为空".to_string(),
        ));
    }
    if !provider_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(ProviderAdminError::Invalid(format!(
            "供应商 id 含非法字符（{provider_id}）：仅允许字母、数字、下划线、连字符"
        )));
    }
    Ok(())
}

/// 模型 id 校验：非空，禁 `/`（破坏 `provider/model` 复合 id 拆分）、引号、
/// 控制字符与 TOML 键上下文的保留符号
fn validate_model_id(model_id: &str) -> Result<(), ProviderAdminError> {
    if model_id.trim().is_empty() {
        return Err(ProviderAdminError::Invalid("模型 id 不能为空".to_string()));
    }
    let bad: &[char] = &['/', '"', '\'', '#', '=', '[', ']'];
    if model_id.chars().any(|c| c.is_control() || bad.contains(&c)) {
        return Err(ProviderAdminError::Invalid(format!(
            "模型 id 含非法字符（{model_id}）：禁止 / 引号 # = [ ] 与控制字符\
             （点号等其余字符可用，落盘时自动加引号键）"
        )));
    }
    if model_id != model_id.trim() {
        return Err(ProviderAdminError::Invalid(format!(
            "模型 id 首尾含空白（{model_id}）：请去除后重试"
        )));
    }
    Ok(())
}

/// 供应商显示名校验：非空（name 必填，空值拒写）
fn validate_provider_name(name: &str) -> Result<(), ProviderAdminError> {
    if name.trim().is_empty() {
        return Err(ProviderAdminError::Invalid(
            "供应商 name 不能为空（显示名，如 name = \"DeepSeek\"）".to_string(),
        ));
    }
    Ok(())
}

/// 模型必填校验：name 非空 + `limit.context` 正整数
///
/// `limit.context` 缺失 / 为 0 会在运行期导致压缩触发公式失效（usable=0、
/// 阈值恒真、每轮必压缩），写入期即拦下，保证写盘结果可被配置加载无损读回。
fn validate_model(model: &Model) -> Result<(), ProviderAdminError> {
    if model.name.trim().is_empty() {
        return Err(ProviderAdminError::Invalid(
            "模型 name 不能为空（显示名，如 name = \"deepseek-v4-flash\"）".to_string(),
        ));
    }
    if model.limit.context == 0 {
        return Err(ProviderAdminError::Invalid(
            "模型 limit.context 必须为正整数（上下文窗口 tokens，如 limit = { context = 128000 }）"
                .to_string(),
        ));
    }
    Ok(())
}

/// api_key 明文校验：不含引号 / 控制字符（.env 单行格式的安全边界）
fn validate_api_key(api_key: &str) -> Result<(), ProviderAdminError> {
    if api_key
        .chars()
        .any(|c| c == '\'' || c == '"' || c.is_control())
    {
        return Err(ProviderAdminError::Invalid(
            "API Key 含引号或控制字符，无法写入 .env（请检查输入）".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== list_models：注册缓存列举与排序契约 =====

    /// 构造仅含必填展示信息的最小 Provider 配置
    fn test_provider(name: &str) -> fuyao_api::Provider {
        fuyao_api::Provider {
            name: name.to_string(),
            ..Default::default()
        }
    }

    /// 构造最小 Model 配置（元信息取默认值）
    fn test_model(name: &str) -> fuyao_api::Model {
        fuyao_api::Model {
            name: name.to_string(),
            cost: Default::default(),
            limit: Default::default(),
            reasoning_efforts: vec![],
            modalities: Default::default(),
        }
    }

    /// 启动前（无 Provider 注册缓存）列举 model 应返回空列表，不 panic
    #[test]
    fn list_models_no_cache_returns_empty() {
        let temp = tempfile::tempdir().unwrap();
        let agent_paths = AgentPaths {
            fuyao_home: temp.path().to_path_buf(),
            ..Default::default()
        };
        let manager = ProviderManager::new(agent_paths);

        let models = manager.list_models();

        assert!(
            models.is_empty(),
            "未跑 init_engine 时缓存为空，应返回空 model 列表"
        );
    }

    /// 注册 Provider 与 Model 后列举，provider_name 应注入 Provider 的显示名
    #[test]
    fn list_models_injects_provider_display_name() {
        let temp = tempfile::tempdir().unwrap();
        let agent_paths = AgentPaths {
            fuyao_home: temp.path().to_path_buf(),
            agent_id: Some("global/injects_name".to_string()),
            ..Default::default()
        };
        let cache_key = fuyao_provider::agent_paths_cache_key(&agent_paths);
        fuyao_provider::register_provider("sensenova", test_provider("商汤 SenseNova"), &cache_key);
        fuyao_provider::register_model("sensenova/glm-5.2", test_model("glm-5.2"), &cache_key);

        let models = ProviderManager::new(agent_paths.clone()).list_models();

        assert_eq!(models.len(), 1, "应列举 1 个 model");
        assert_eq!(models[0].id, "glm-5.2", "id 为纯模型名");
        assert_eq!(
            models[0].provider_id, "sensenova",
            "provider_id 为供应商 id"
        );
        assert_eq!(
            models[0].provider_name, "商汤 SenseNova",
            "provider_name 应取 Provider 注册配置的显示名"
        );

        fuyao_provider::clear_cache(&agent_paths);
    }

    /// Model 对应的 Provider 未注册（缓存异常缺口）时，provider_name 应回退为 id
    #[test]
    fn list_models_missing_provider_falls_back_to_id() {
        let temp = tempfile::tempdir().unwrap();
        let agent_paths = AgentPaths {
            fuyao_home: temp.path().to_path_buf(),
            agent_id: Some("global/fallback_id".to_string()),
            ..Default::default()
        };
        let cache_key = fuyao_provider::agent_paths_cache_key(&agent_paths);
        // 仅注册 model，不注册 provider，制造 Provider 缓存缺口
        fuyao_provider::register_model(
            "orphan/lonely-model",
            test_model("lonely-model"),
            &cache_key,
        );

        let models = ProviderManager::new(agent_paths.clone()).list_models();

        assert_eq!(models.len(), 1, "应列举 1 个 model");
        assert_eq!(
            models[0].provider_name, "orphan",
            "Provider 缺失时 provider_name 应回退为供应商 id"
        );

        fuyao_provider::clear_cache(&agent_paths);
    }

    /// 乱序注册多供应商多模型后列举，输出应按 (provider_id, id) 字母序稳定排列
    ///
    /// 注册缓存为 HashMap（遍历序随机），排序契约保证列表顺序与注册顺序无关、
    /// 跨进程启动稳定，UI 下拉可直接沿用。
    #[test]
    fn list_models_sorted_by_provider_then_id() {
        let temp = tempfile::tempdir().unwrap();
        let agent_paths = AgentPaths {
            fuyao_home: temp.path().to_path_buf(),
            agent_id: Some("global/sorted_models".to_string()),
            ..Default::default()
        };
        let cache_key = fuyao_provider::agent_paths_cache_key(&agent_paths);
        // 故意按字母逆序注册：供应商 zhipu 在前、sensenova 在后，组内模型同理
        fuyao_provider::register_provider("zhipu", test_provider("智谱"), &cache_key);
        fuyao_provider::register_provider("sensenova", test_provider("商汤"), &cache_key);
        fuyao_provider::register_model("zhipu/glm-5.2", test_model("glm-5.2"), &cache_key);
        fuyao_provider::register_model("zhipu/glm-4.7", test_model("glm-4.7"), &cache_key);
        fuyao_provider::register_model("sensenova/sense-6.5", test_model("sense-6.5"), &cache_key);
        fuyao_provider::register_model("sensenova/sense-5.0", test_model("sense-5.0"), &cache_key);

        let models = ProviderManager::new(agent_paths.clone()).list_models();

        let got: Vec<(String, String)> = models
            .iter()
            .map(|m| (m.provider_id.clone(), m.id.clone()))
            .collect();
        let want = vec![
            ("sensenova".to_string(), "sense-5.0".to_string()),
            ("sensenova".to_string(), "sense-6.5".to_string()),
            ("zhipu".to_string(), "glm-4.7".to_string()),
            ("zhipu".to_string(), "glm-5.2".to_string()),
        ];
        assert_eq!(got, want, "应先按 provider_id 后按模型 id 字母序排列");

        fuyao_provider::clear_cache(&agent_paths);
    }

    // ===== list_providers_with_source：来源层标注（直读三层落盘） =====

    /// 构造三层齐全的 AgentPaths（agent 层经 global 前缀的 agent_id 落在
    /// fuyao_home 下，workspace 层独立临时目录）
    fn layered_agent_paths(test_name: &str) -> (AgentPaths, tempfile::TempDir, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let agent_paths = AgentPaths {
            agent_id: Some(format!("global/{test_name}")),
            workspace: Some(ws.path().to_path_buf()),
            extra_dirs: Vec::new(),
            fuyao_home: home.path().to_path_buf(),
        };
        (agent_paths, home, ws)
    }

    /// 三层布局下各实体的来源判定：供应商取最高优先级定义层、模型独立判定
    #[test]
    fn list_with_source_marks_layer_per_entity() {
        let (agent_paths, home, ws) = layered_agent_paths("layered_sources");
        // global 层：alpha（含 old-model）+ deepseek
        std::fs::write(
            home.path().join("fuyao.toml"),
            "[providers.alpha]\nname = \"A\"\n\
             [providers.alpha.models.old-model]\nname = \"old\"\nlimit = { context = 64000 }\n\
             [providers.deepseek]\nname = \"D\"\n",
        )
        .unwrap();
        // agent 层：charlie（含 cm）
        let agent_dir = home.path().join("fuyao-agents").join("layered_sources");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("fuyao.toml"),
            "[providers.charlie]\nname = \"C\"\n\
             [providers.charlie.models.cm]\nname = \"cm\"\nlimit = { context = 32000 }\n",
        )
        .unwrap();
        // workspace 层：beta + 给 alpha 补 new-model
        let ws_root = ws.path().join(".fuyao");
        std::fs::create_dir_all(&ws_root).unwrap();
        std::fs::write(
            ws_root.join("fuyao.toml"),
            "[providers.beta]\nname = \"B\"\n\
             [providers.alpha.models.new-model]\nname = \"new\"\nlimit = { context = 128000 }\n",
        )
        .unwrap();

        let list = ProviderManager::new(agent_paths)
            .list_providers_with_source()
            .unwrap();

        let find = |id: &str| {
            list.iter()
                .find(|p| p.id == id)
                .unwrap_or_else(|| panic!("应含供应商 {id}"))
        };
        // 供应商：多层出现的取最高优先级层，单层出现的记其定义层
        assert_eq!(find("alpha").source, ProviderSource::Workspace);
        assert_eq!(find("deepseek").source, ProviderSource::Global);
        assert_eq!(find("charlie").source, ProviderSource::Agent);
        assert_eq!(find("beta").source, ProviderSource::Workspace);
        // 模型独立判定：同一供应商（alpha）下两个模型来源不同
        let alpha = find("alpha");
        let model_source = |mid: &str| {
            alpha
                .models
                .iter()
                .find(|m| m.id == mid)
                .unwrap_or_else(|| panic!("alpha 应含模型 {mid}"))
                .source
        };
        assert_eq!(model_source("old-model"), ProviderSource::Global);
        assert_eq!(model_source("new-model"), ProviderSource::Workspace);
        // 合并值的完整性：new-model 的 limit 来自 workspace 层声明
        let new_model = alpha.models.iter().find(|m| m.id == "new-model").unwrap();
        assert_eq!(new_model.model.limit.context, 128000);
    }

    /// 写盘后列表立即可见（不经注册缓存刷新）：create_provider + create_model
    /// 完成即出现在列表，来源为 Global，管理字段（base_url / 指针）齐备
    #[test]
    fn list_with_source_reflects_disk_write_immediately() {
        let (agent_paths, _home, _ws) = layered_agent_paths("disk_write_visible");
        let manager = ProviderManager::new(agent_paths.clone());

        // 未注册任何缓存、未写盘：空列表
        assert!(manager.list_providers_with_source().unwrap().is_empty());

        manager
            .create_provider(
                "deepseek",
                ProviderSpec {
                    name: "DeepSeek".to_string(),
                    base_url: Some("https://api.deepseek.com".to_string()),
                    api_key: Some("sk-plain".to_string()),
                    overwrite_api_key: false,
                },
            )
            .unwrap();
        let mut model = test_model("deepseek-v4-flash");
        model.limit.context = 128000;
        manager
            .create_model("deepseek", "deepseek-v4-flash", model)
            .unwrap();

        let list = manager.list_providers_with_source().unwrap();
        assert_eq!(list.len(), 1, "写盘后不经缓存刷新即可见");
        let provider = &list[0];
        assert_eq!(provider.id, "deepseek");
        assert_eq!(provider.source, ProviderSource::Global);
        assert_eq!(provider.name, "DeepSeek");
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://api.deepseek.com")
        );
        assert_eq!(provider.api_key_env_vars, vec!["DEEPSEEK_API_KEY"]);
        assert_eq!(provider.models.len(), 1);
        assert_eq!(provider.models[0].id, "deepseek-v4-flash");
        assert_eq!(provider.models[0].source, ProviderSource::Global);
    }

    /// 乱序落盘多供应商多模型，输出按 (provider_id, model id) 字母序稳定排列
    #[test]
    fn list_with_source_sorted_stably() {
        let (agent_paths, home, _ws) = layered_agent_paths("sorted_sources");
        // 故意按字母逆序写盘
        std::fs::write(
            home.path().join("fuyao.toml"),
            "[providers.zhipu]\nname = \"Z\"\n\
             [providers.zhipu.models.\"glm-5.2\"]\nname = \"g52\"\nlimit = { context = 64000 }\n\
             [providers.zhipu.models.\"glm-4.7\"]\nname = \"g47\"\nlimit = { context = 64000 }\n\
             [providers.sensenova]\nname = \"S\"\n\
             [providers.sensenova.models.\"sense-6.5\"]\nname = \"s65\"\nlimit = { context = 64000 }\n",
        )
        .unwrap();

        let list = ProviderManager::new(agent_paths)
            .list_providers_with_source()
            .unwrap();

        let got: Vec<(&str, Vec<&str>)> = list
            .iter()
            .map(|p| {
                (
                    p.id.as_str(),
                    p.models.iter().map(|m| m.id.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("sensenova", vec!["sense-6.5"]),
                ("zhipu", vec!["glm-4.7", "glm-5.2"]),
            ],
            "先按供应商 id 后按模型 id 字母序排列"
        );
    }

    /// global 层 TOML 语法坏：列表整体 fail-loud（与 CRUD 同口径）
    #[test]
    fn list_with_source_bad_toml_fails_loud() {
        let (agent_paths, home, _ws) = layered_agent_paths("bad_toml");
        std::fs::write(home.path().join("fuyao.toml"), "not [ valid").unwrap();

        let err = ProviderManager::new(agent_paths)
            .list_providers_with_source()
            .unwrap_err();
        assert!(matches!(err, ProviderAdminError::TomlParse(_)));
    }

    // ===== 入参校验 =====

    #[test]
    fn validate_provider_id_accepts_bare_key_charset() {
        assert!(validate_provider_id("deepseek").is_ok());
        assert!(validate_provider_id("My-Vendor_9").is_ok());
    }

    #[test]
    fn validate_provider_id_rejects_empty_and_special_chars() {
        assert!(validate_provider_id("").is_err());
        assert!(validate_provider_id("a.b").is_err());
        assert!(validate_provider_id("a b").is_err());
        assert!(validate_provider_id("中文").is_err());
    }

    #[test]
    fn validate_model_id_allows_dots_but_rejects_slash_and_quotes() {
        assert!(validate_model_id("qwen3.6-plus").is_ok());
        assert!(validate_model_id("deepseek-v4-flash").is_ok());
        assert!(validate_model_id("a/b").is_err());
        assert!(validate_model_id("a\"b").is_err());
        assert!(validate_model_id(" a").is_err());
        assert!(validate_model_id("  ").is_err());
    }

    #[test]
    fn validate_model_enforces_name_and_positive_context() {
        let mut model = test_model("x");
        model.limit.context = 128000;
        assert!(validate_model(&model).is_ok());

        model.name = "  ".to_string();
        assert!(validate_model(&model).is_err());

        model.name = "x".to_string();
        model.limit.context = 0;
        assert!(validate_model(&model).is_err());
    }

    // ===== ProviderSpec 的 Debug 屏蔽 =====

    #[test]
    fn provider_spec_debug_masks_api_key() {
        let spec = ProviderSpec {
            name: "DeepSeek".to_string(),
            base_url: None,
            api_key: Some("sk-secret".to_string()),
            overwrite_api_key: false,
        };
        let debug = format!("{spec:?}");
        assert!(
            !debug.contains("sk-secret"),
            "Debug 输出不得泄露 api_key：{debug}"
        );
        assert!(debug.contains("<已隐藏>"), "应以占位符标注：{debug}");
    }
}
