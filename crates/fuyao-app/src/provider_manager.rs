//! 供应商管理门面：供应商与模型配置的创建 / 更新 / 删除 / 列举接口
//!
//! 供应商与模型配置是**全局资源**（跨工作区共享）：本门面把「领域存储 =
//! global 层 fuyao.toml + .env」的增量写回编排成供应商粒度的三个变更原语。
//! 底层功能（写回落存储、段级 patch、fail-loud 校验、错误映射）在
//! fuyao-provider 的 admin 域。
//!
//! # 单一事实源
//!
//! - **供应商定义只存在于 global 层**（`~/.fuyao/fuyao.toml`、`~/.fuyao/.env`）：
//!   配置加载对 agent / workspace 层出现的 `providers` 段直接报错，管理与加载
//!   读写同一份落盘，不存在跨层覆盖。
//! - **供应商 id 不可变**：id 是会话缓存与历史引用的字符串锚点，换 id 走
//!   「建新 + 删旧」；模型 id 随 models 整表替换自由变更。
//! - **写入不等于立即生效**：本门面只负责写盘。写盘结果经配置加载读回语义
//!   一致；存活 engine 的「立即可用」由调用方经引擎侧运行时注册原语编排刷新。

use fuyao_api::{AgentPaths, ProviderModelOption, ProviderOption, load_config};
use fuyao_provider::admin::{
    insert_provider, map_config_error, patch_provider, prepare_env_upsert, read_global_env,
    read_global_toml, remove_provider, validate_provider_id, validate_spec, write_global_env,
    write_global_toml,
};

pub use fuyao_provider::admin::{ProviderAdminError, ProviderModelSpec, ProviderSpec};

/// 供应商管理器：封装 global 层 fuyao.toml / .env 的供应商与模型增量写回
///
/// 持有 [`AgentPaths`]（构造时注入一次）：`fuyao_home` 决定读写落点。
pub struct ProviderManager {
    /// 路径身份（启动时注入，供应商管理全程只读）
    agent_paths: AgentPaths,
}

impl ProviderManager {
    /// 构造供应商管理门面
    ///
    /// 公开构造：纯文件读写，无引擎依赖，任何时机可用。典型传
    /// [`AgentPaths::default`]（仅 global 层）。
    pub fn new(agent_paths: AgentPaths) -> Self {
        Self { agent_paths }
    }

    // ── 供应商管理列举（直读落盘）───────────────────────────────

    /// 列举全部供应商与旗下模型
    ///
    /// 供应商管理与模型选择的唯一列举入口：实时读配置落盘（供应商定义的单一
    /// 事实源在 global 层），**不经注册缓存**——本门面写盘的新增 / 修改即刻反映
    /// 在列表中，引擎启动前同样可用。旗下模型的 `id` 为纯模型名，调用方按需
    /// 拼成 `provider_id/id` 设给 model_id。
    ///
    /// 顺序契约：供应商按 id 字母序，组内模型按 id 字母序（跨启动稳定，消费方
    /// 如 UI 下拉可直接沿用本序呈现）。
    ///
    /// # 错误
    /// 配置加载失败（TOML 语法 / IO / providers 段校验）整体 fail-loud，映射为
    /// [`ProviderAdminError`]（`TomlParse` / `Io` / `Invalid`）——坏配置下列表
    /// 不可用与 CRUD 一致，引导用户先修复配置。
    pub fn list_providers(&self) -> Result<Vec<ProviderOption>, ProviderAdminError> {
        // 加载配置取 providers（无任何配置 = 空列表）；providers 只存在于
        // global 层，结果的 providers 即 global 落盘内容
        let config = load_config(&self.agent_paths).map_err(map_config_error)?;
        let providers = config.map(|c| c.providers).unwrap_or_default();

        let mut options: Vec<ProviderOption> = providers
            .into_iter()
            .map(|(id, provider)| {
                // 旗下模型按模型 id 字母序
                let mut models: Vec<ProviderModelOption> = provider
                    .models
                    .into_iter()
                    .map(|(model_id, model)| ProviderModelOption {
                        id: model_id,
                        model,
                    })
                    .collect();
                models.sort_by(|a, b| a.id.cmp(&b.id));

                ProviderOption {
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

    /// 创建供应商：global 层 fuyao.toml 新增 `[providers.<id>]` 段（含全量模型）
    ///
    /// 载荷即完整期望状态：`name` / `base_url` / `api_key_env_vars` 指针 /
    /// 全部模型一次写盘落齐。`api_key` 为 `Some` 时把明文 upsert 进 .env 的
    /// 指定变量（同名覆盖、异名新加）；`None` 不动 .env。已存在的 id 拒绝
    /// 创建。写盘成功后由调用方编排存活 engine 的运行时注册。
    pub fn create_provider(
        &self,
        provider_id: &str,
        spec: ProviderSpec,
    ) -> Result<(), ProviderAdminError> {
        validate_provider_id(provider_id)?;
        validate_spec(&spec)?;

        // .env 先备好新内容（含跨行值拒绝），提交推迟到 toml 写盘成功后——
        // toml 侧失败时 .env 不动
        let env_write = match (&spec.api_key_env_var, &spec.api_key) {
            (Some(env_var), Some(api_key)) => Some(prepare_env_upsert(
                &read_global_env(&self.agent_paths)?,
                env_var,
                api_key,
            )?),
            _ => None,
        };
        let has_api_key = spec.api_key.is_some();
        let data = spec.into_data();

        let mut doc = read_global_toml(&self.agent_paths)?;
        insert_provider(&mut doc, provider_id, &data)?;
        write_global_toml(&self.agent_paths, &doc)?;
        if let Some(next) = env_write {
            write_global_env(&self.agent_paths, &next)?;
        }

        tracing::info!(
            provider_id = %provider_id,
            model_count = data.models.len(),
            has_api_key,
            "供应商已创建（含全量模型，写回 global 层 fuyao.toml）"
        );
        Ok(())
    }

    /// 更新供应商：目标段管理字段 patch + `models` 子表整表替换
    ///
    /// 完整期望状态语义：`name` / `base_url`（None 清除）、`api_key_env_vars`
    /// 指针按载荷所见即所得（Some 落单值、None 移除键）、`models` 整表替换
    /// 为载荷列表（未携带的模型消失，模型 id 可随替换变更）；段内其他键
    /// （用户手写的未知字段）不动。
    ///
    /// `api_key` 为 `Some` 时把明文 upsert 进 .env（同名覆盖、异名新加），
    /// 并移除段内残留的 `options.api_key` 明文（明文密钥不进 toml）；`None`
    /// 不动 .env。id 经签名定位不可变（换 id 走「建新 + 删旧」）；目标不
    /// 存在返回 `NotFound`。
    pub fn update_provider(
        &self,
        provider_id: &str,
        spec: ProviderSpec,
    ) -> Result<(), ProviderAdminError> {
        validate_provider_id(provider_id)?;
        validate_spec(&spec)?;

        // .env 先备好新内容，提交推迟到 toml 写盘成功后
        let env_write = match (&spec.api_key_env_var, &spec.api_key) {
            (Some(env_var), Some(api_key)) => Some(prepare_env_upsert(
                &read_global_env(&self.agent_paths)?,
                env_var,
                api_key,
            )?),
            _ => None,
        };
        let has_api_key = spec.api_key.is_some();
        let model_count = spec.models.len();

        let mut doc = read_global_toml(&self.agent_paths)?;
        patch_provider(&mut doc, provider_id, &spec)?;
        write_global_toml(&self.agent_paths, &doc)?;
        if let Some(next) = env_write {
            write_global_env(&self.agent_paths, &next)?;
        }

        tracing::info!(
            provider_id = %provider_id,
            model_count,
            has_api_key,
            "供应商已更新（管理字段 patch + models 整表替换）"
        );
        Ok(())
    }

    /// 删除供应商：移除 `[providers.<id>]` 段（级联其全部模型）
    ///
    /// .env 是用户持有的资产（变量名用户自设、可能被多个供应商共享引用），
    /// 删除不触碰 .env——密钥行由用户自行管理。toml 侧只移除目标段（其他
    /// 供应商与全局其他配置逐字保留；providers 段删空后连带移除空表头）。
    ///
    /// 既有会话对该供应商的引用不受删除阻挡（运行中 turn 跑完即止）——删除
    /// 后果的告知属调用方确认流程。
    pub fn delete_provider(&self, provider_id: &str) -> Result<(), ProviderAdminError> {
        validate_provider_id(provider_id)?;

        let mut doc = read_global_toml(&self.agent_paths)?;
        remove_provider(&mut doc, provider_id)?;
        write_global_toml(&self.agent_paths, &doc)?;

        tracing::info!(
            provider_id = %provider_id,
            "供应商已删除（toml 段级联删除全部模型；.env 归用户持有，不动）"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== 构造辅助 =====

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

    // ===== list_providers：直读落盘 =====

    /// 构造仅 global 层的 AgentPaths（fuyao_home 注入临时目录，agent_id /
    /// workspace 均空——与典型用法 `AgentPaths::default` 同构）
    fn global_agent_paths() -> (AgentPaths, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let agent_paths = AgentPaths {
            agent_id: None,
            workspace: None,
            extra_dirs: Vec::new(),
            fuyao_home: home.path().to_path_buf(),
        };
        (agent_paths, home)
    }

    /// 写盘后列表立即可见（不经注册缓存刷新）：create_provider（含内嵌模型）
    /// 完成即出现在列表，管理字段（base_url / 指针）齐备
    #[test]
    fn list_reflects_disk_write_immediately() {
        let (agent_paths, _home) = global_agent_paths();
        let manager = ProviderManager::new(agent_paths.clone());

        // 未注册任何缓存、未写盘：空列表
        assert!(manager.list_providers().unwrap().is_empty());

        let mut model = test_model("deepseek-v4-flash");
        model.limit.context = 128000;
        manager
            .create_provider(
                "deepseek",
                ProviderSpec {
                    name: "DeepSeek".to_string(),
                    base_url: Some("https://api.deepseek.com".to_string()),
                    api_key_env_var: Some("MY_DEEPSEEK_KEY".to_string()),
                    api_key: Some("sk-plain".to_string()),
                    models: vec![ProviderModelSpec {
                        id: "deepseek-v4-flash".to_string(),
                        model,
                    }],
                },
            )
            .unwrap();

        let list = manager.list_providers().unwrap();
        assert_eq!(list.len(), 1, "写盘后不经缓存刷新即可见");
        let provider = &list[0];
        assert_eq!(provider.id, "deepseek");
        assert_eq!(provider.name, "DeepSeek");
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://api.deepseek.com")
        );
        assert_eq!(provider.api_key_env_vars, vec!["MY_DEEPSEEK_KEY"]);
        assert_eq!(provider.models.len(), 1);
        assert_eq!(provider.models[0].id, "deepseek-v4-flash");
    }

    /// 乱序落盘多供应商多模型，输出按 (provider_id, model id) 字母序稳定排列
    #[test]
    fn list_sorted_stably() {
        let (agent_paths, home) = global_agent_paths();
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

        let list = ProviderManager::new(agent_paths).list_providers().unwrap();

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
    fn list_bad_toml_fails_loud() {
        let (agent_paths, home) = global_agent_paths();
        std::fs::write(home.path().join("fuyao.toml"), "not [ valid").unwrap();

        let err = ProviderManager::new(agent_paths)
            .list_providers()
            .unwrap_err();
        assert!(matches!(err, ProviderAdminError::TomlParse(_)));
    }
}
