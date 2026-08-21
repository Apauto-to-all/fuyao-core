//! Provider 运行时注册（存活 engine 的动态生效原语）
//!
//! 本模块集中 [`Engine`] 的「供应商路由表」运行时动作：
//! - [`Engine::register_provider`]：向存活 engine 注入一个供应商（配置进缓存 +
//!   实例进注册表），与启动期 [`ProviderRegistry::from_registered`] 的批量构造
//!   写同一张表、幂等共存（同名覆盖）
//! - [`Engine::unregister_provider`]：从存活 engine 移除一个供应商（实例 +
//!   缓存条目），下一轮 turn 解析缺失时报错由 turn 层给出（含实体标识）
//!
//! 两方法均为同步（无 await）：配置加载与实例构造是纯 CPU + 本地文件读，
//! 注册表操作为短临界区读写锁，锁内无 await。
//!
//! 「写盘 → 立即可用」的完整链路跨三层职责：管理面写盘（fuyao-app 的
//! ProviderManager）→ 本原语完成内存注册（缓存 + 实例表）→ 装配层负责在
//! 全部存活 engine 上编排调用（多 engine 场景每台各调一次）。

use super::*;

impl Engine {
    /// 向存活 engine 注入一个供应商（写盘后的内存注册，下一 turn 立即可用）
    ///
    /// 从三层合并后的落盘配置取该供应商定义，依次完成：
    /// 1. 配置注册进进程级缓存（Provider + 其全部 Model，幂等覆盖）——后续
    ///    `resolve_model` / `resolve_api_key` 等解析链全部走该缓存；
    /// 2. 三层 `.env` 增量补载（只补进程环境缺失的变量）——管理面写回的
    ///    密钥变量发生在启动期加载之后，不补载则指针解析落空；
    /// 3. 构造 `OpenAIProvider` 实例插入本 engine 的 [`ProviderRegistry`]——
    ///    下一个 turn 起按 provider_id 路由到新实例。
    ///
    /// 与启动期构造路径幂等共存：对启动时已注册的供应商再次调用，效果是配置与
    /// 实例刷新为新落盘状态（运行中的 turn 持旧实例跑完本轮，下一轮取新实例）。
    ///
    /// # 错误（均含实体标识，可诊断）
    /// - 三层配置加载失败（TOML 语法 / IO）→ [`EngineError::Provider`]
    /// - 该 provider_id 未在三层配置中定义 → [`EngineError::Provider`]
    /// - API Key 无法解析（`options.api_key` 未配置且指针变量均未设置）→
    ///   [`EngineError::Provider`]，信息列出全部指针变量名，指向 `.env` 排查方向
    ///
    /// # 参数
    /// - `provider_id`：供应商 id（`[providers.<id>]` 键，大小写不敏感）
    pub fn register_provider(&self, provider_id: &str) -> Result<(), EngineError> {
        let agent_paths = &self.params.agent_paths;

        // 1. 三层合并加载：拿该供应商的当前落盘定义（含全部模型）
        let config = fuyao_api::load_config(agent_paths).map_err(|e| {
            EngineError::Provider(format!(
                "供应商 '{provider_id}' 注册失败：三层配置加载失败（{e}）"
            ))
        })?;
        // 1. 三层合并加载：拿该供应商的当前落盘定义（含全部模型）。
        //    键查找大小写不敏感：注册缓存与实例表均以小写形态归一键，model_id
        //    路由拆分也小写化 provider 段，落盘键的原始大小写不应造成注册 miss
        let provider = config
            .and_then(|c| {
                c.providers
                    .get(provider_id)
                    .or_else(|| {
                        c.providers
                            .iter()
                            .find(|(key, _)| key.eq_ignore_ascii_case(provider_id))
                            .map(|(_, value)| value)
                    })
                    .cloned()
            })
            .ok_or_else(|| {
                EngineError::Provider(format!(
                    "供应商 '{provider_id}' 未在三层 fuyao.toml 的 [providers.{provider_id}] 段中定义（请先落盘再注册）"
                ))
            })?;

        // 2. 配置进缓存（幂等覆盖）：Provider 本体 + 旗下全部 Model
        let cache_key = fuyao_provider::agent_paths_cache_key(agent_paths);
        fuyao_provider::register_provider(provider_id, provider.clone(), &cache_key);
        for (model_id, model) in &provider.models {
            let full_id = format!("{provider_id}/{model_id}");
            fuyao_provider::register_model(&full_id, model.clone(), &cache_key);
        }

        // 3. .env 运行时增量补载：管理面写回 global 层 .env 的密钥变量发生在
        //    启动期 load_env 之后，不补载则指针变量不在进程环境、解析必然落空
        fuyao_api::load_env_missing(agent_paths);

        // 4. API Key 预检：构造实例对 Key 的唯一硬依赖，缺失时给出含指针变量名单的
        //    可诊断错误（.env 变量被手删是常见成因，错误信息直指排查方向）
        if fuyao_provider::resolve_api_key(provider_id, agent_paths).is_none() {
            return Err(EngineError::Provider(format!(
                "供应商 '{provider_id}' 注册失败：API Key 未解析到（options.api_key 未配置，\
                 环境变量 {:?} 均未设置——请检查 global 层 .env 是否写入了对应变量）",
                provider.api_key_env_vars
            )));
        }

        // 5. 构造实例并插入注册表（同名覆盖 = 刷新；key 的大小写归一由注册表保证）
        let instance =
            fuyao_provider::OpenAIProvider::new(provider_id, agent_paths).ok_or_else(|| {
                EngineError::Provider(format!(
                    "供应商 '{provider_id}' 注册失败：Provider 实例构造失败（HTTP 客户端构建异常）"
                ))
            })?;
        self.providers
            .register(provider_id, std::sync::Arc::new(instance));

        tracing::info!(
            provider_id = %provider_id,
            model_count = provider.models.len(),
            "供应商已运行时注册（存活 engine 立即可用，下一 turn 生效）"
        );
        Ok(())
    }

    /// 从存活 engine 移除一个供应商（删除后的内存一致性，下一轮缺失报错）
    ///
    /// 供应商删除后的内存一致性动作：实例从注册表移除（下一轮 turn 解析到该
    /// provider_id 时由 turn 层报错，信息含实体标识），缓存中的 Provider 配置与
    /// 旗下全部 Model 条目一并清除（上下文长度等解析链不再返回旧值）。
    ///
    /// 运行中 turn 零中断：已在跑的 turn 持有旧实例的 `Arc` 克隆，跑完本轮。
    ///
    /// # 返回
    /// 实例是否实际被移除（false = 本就不在注册表，幂等）。
    pub fn unregister_provider(&self, provider_id: &str) -> bool {
        let agent_paths = &self.params.agent_paths;

        // 实例表移除（大小写归一）
        let removed = self.providers.unregister(provider_id);

        // 缓存条目清除：Provider 本体 + 旗下全部 Model（按 "{id}/" 前缀过滤；
        // full_id 在缓存中以小写形态存储，前缀用小写化的 provider_id 匹配）
        let cache_key = fuyao_provider::agent_paths_cache_key(agent_paths);
        fuyao_provider::unregister_provider(provider_id, &cache_key);
        let prefix = format!("{}/", provider_id.to_lowercase());
        let model_ids: Vec<String> = fuyao_provider::list_models(agent_paths)
            .keys()
            .filter(|full_id| full_id.starts_with(&prefix))
            .cloned()
            .collect();
        for full_id in model_ids {
            fuyao_provider::unregister_model(&full_id, &cache_key);
        }

        tracing::info!(
            provider_id = %provider_id,
            removed,
            "供应商已运行时反注册（运行中 turn 跑完即止，下一轮解析将报错）"
        );
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolRegistry;
    use fuyao_hooks::PluginHost;

    /// 构造测试用 AgentPaths（临时 fuyao_home + 唯一 agent_id，隔离全局缓存）
    fn unique_paths(test_name: &str, home: &std::path::Path) -> fuyao_api::AgentPaths {
        fuyao_api::AgentPaths {
            agent_id: Some(format!("global/{test_name}")),
            workspace: None,
            extra_dirs: Vec::new(),
            fuyao_home: home.to_path_buf(),
        }
    }

    /// 落盘一份含目标供应商的 global 层 fuyao.toml（明文 api_key 走 options，
    /// 避免 .env 环境变量串扰）
    fn write_global_config(home: &std::path::Path, provider_id: &str, model_id: &str) {
        let content = format!(
            "[providers.{provider_id}]\n\
             name = \"Test\"\n\
             options = {{ api_key = \"sk-test\" }}\n\
             [providers.{provider_id}.models.\"{model_id}\"]\n\
             name = \"{model_id}\"\n\
             limit = {{ context = 64000 }}\n"
        );
        std::fs::write(home.join("fuyao.toml"), content).expect("写测试配置失败");
    }

    /// 构造不依赖 session 的最小 Engine（空工具表 + 空插件 + 空 store 路径）
    async fn bare_engine(paths: fuyao_api::AgentPaths) -> Arc<Engine> {
        let store = std::sync::Arc::new(
            fuyao_session::SessionStore::new(paths.sessions_db_path())
                .await
                .expect("构造 SessionStore 失败"),
        );
        Engine::new(
            fuyao_api::EngineParams {
                agent_paths: paths.clone(),
            },
            fuyao_provider::ProviderRegistry::default(),
            ToolRegistry::builder().build(),
            PluginHost::new(),
            store,
        )
        .await
    }

    /// 运行时注册：落盘后注册，注册表与缓存立即可查，模型条目一并注册
    #[tokio::test]
    async fn register_provider_populates_registry_and_cache() {
        let home = tempfile::tempdir().unwrap();
        let paths = unique_paths("rt_register", home.path());
        write_global_config(home.path(), "alpha", "m1");
        let engine = bare_engine(paths.clone()).await;

        assert!(!engine.providers.contains("alpha"), "注册前不在注册表");
        engine.register_provider("alpha").expect("注册应成功");

        assert!(engine.providers.contains("alpha"), "注册后实例可查");
        assert!(
            fuyao_provider::get_provider("alpha", &paths).is_some(),
            "配置缓存已注册"
        );
        assert!(
            fuyao_provider::get_model("alpha/m1", &paths).is_some(),
            "模型条目一并注册"
        );

        fuyao_provider::clear_cache(&paths);
    }

    /// 注册未落盘的供应商：报错含实体标识与落盘指引
    #[tokio::test]
    async fn register_provider_absent_errors_with_identity() {
        let home = tempfile::tempdir().unwrap();
        let paths = unique_paths("rt_absent", home.path());
        let engine = bare_engine(paths.clone()).await;

        let err = engine.register_provider("ghost").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ghost"), "错误信息含实体标识：{msg}");
        assert!(msg.contains("未在三层"), "错误指向配置缺失：{msg}");

        fuyao_provider::clear_cache(&paths);
    }

    /// 落盘键大小写混合（如 [providers.Alpha]）：以小写形态注册仍能命中
    /// （注册缓存与实例表的键均小写归一，落盘键大小写不应造成注册 miss）
    #[tokio::test]
    async fn register_provider_matches_case_insensitive_disk_key() {
        let home = tempfile::tempdir().unwrap();
        let paths = unique_paths("rt_case_key", home.path());
        write_global_config(home.path(), "Alpha", "m1");
        let engine = bare_engine(paths.clone()).await;

        engine
            .register_provider("alpha")
            .expect("小写注册应命中大写落盘键");

        assert!(engine.providers.contains("alpha"), "实例以小写键可查");
        assert!(
            fuyao_provider::get_model("alpha/m1", &paths).is_some(),
            "模型条目以小写复合键可查"
        );

        fuyao_provider::clear_cache(&paths);
    }

    /// API Key 缺失（options 未配、指针变量未设）：报错含指针变量名单与 .env 指引
    #[tokio::test]
    async fn register_provider_missing_key_errors_with_env_hint() {
        let home = tempfile::tempdir().unwrap();
        let paths = unique_paths("rt_no_key", home.path());
        std::fs::write(
            home.path().join("fuyao.toml"),
            "[providers.beta]\nname = \"B\"\napi_key_env_vars = [\"BETA_API_KEY\"]\n",
        )
        .unwrap();
        let engine = bare_engine(paths.clone()).await;

        let err = engine.register_provider("beta").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("beta"), "错误信息含实体标识：{msg}");
        assert!(
            msg.contains("BETA_API_KEY"),
            "错误信息含指针变量名单：{msg}"
        );
        assert!(msg.contains(".env"), "错误指向 .env 排查方向：{msg}");

        fuyao_provider::clear_cache(&paths);
    }

    /// 反注册：实例与缓存条目（含旗下模型）全部移除；不存在的幂等返回 false
    #[tokio::test]
    async fn unregister_provider_clears_registry_and_cache() {
        let home = tempfile::tempdir().unwrap();
        let paths = unique_paths("rt_unregister", home.path());
        write_global_config(home.path(), "gamma", "m1");
        let engine = bare_engine(paths.clone()).await;
        engine.register_provider("gamma").unwrap();

        assert!(engine.unregister_provider("GAMMA"), "移除存在的返回 true");

        assert!(!engine.providers.contains("gamma"), "实例已移除");
        assert!(
            fuyao_provider::get_provider("gamma", &paths).is_none(),
            "配置缓存已清"
        );
        assert!(
            fuyao_provider::get_model("gamma/m1", &paths).is_none(),
            "模型缓存条目已清"
        );
        assert!(
            !engine.unregister_provider("gamma"),
            "再次移除返回 false（幂等）"
        );

        fuyao_provider::clear_cache(&paths);
    }
}
