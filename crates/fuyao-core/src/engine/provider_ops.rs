//! Provider 运行时刷新（存活 engine 的动态生效原语）
//!
//! 本模块集中 [`Engine`] 的「供应商路由表」运行时动作：
//! - [`Engine::reload_providers`]：把供应商注册表对齐到当前落盘状态——落盘
//!   存在的逐个注册（配置进缓存 + 实例进注册表，同名覆盖即刷新），落盘已
//!   不存在的实例与缓存条目清除
//!
//! 方法为同步（无 await）：配置加载与实例构造是纯 CPU + 本地文件读，注册表
//! 操作为短临界区读写锁，锁内无 await。
//!
//! 「写盘 → 立即可用」的完整链路跨三层职责：管理面写盘（fuyao-app 的
//! ProviderManager）→ 本原语完成全量内存对齐（缓存 + 实例表）→ 装配层负责在
//! 全部存活 engine 上编排调用（多 engine 场景每台各调一次）。

use super::*;

impl Engine {
    /// 把供应商注册表对齐到当前落盘状态（全量同步，幂等）
    ///
    /// 读取三层合并后的落盘 providers，依次完成：
    /// 1. `.env` 增量补载（只补进程环境缺失的变量）——管理面写回的密钥变量
    ///    发生在启动期加载之后，不补载则指针解析落空；
    /// 2. 落盘存在的每个供应商：配置注册进进程级缓存（Provider + 其全部
    ///    Model，幂等覆盖）+ 按配置的 `api_protocol` 经构造工厂分派实例、
    ///    插入本 engine 的 [`ProviderRegistry`]（同名覆盖 = 刷新）；
    /// 3. 注册表与落盘不再对应的清理：落盘已不存在的供应商、落盘存在但实例
    ///    构造失败的供应商，实例从注册表移除，前者的缓存条目（含旗下全部
    ///    Model）一并清除。
    ///
    /// 对齐语义 = 「引擎注册表 := 落盘中可构造实例的子集」：任何落盘变更
    /// （管理面 CRUD、用户手改 fuyao.toml）之后调一次本方法即完成内存对账，
    /// 天然幂等；无配置文件时所有已注册实例被清除（落盘为空即内存为空）。
    ///
    /// 单个供应商实例构造失败（API Key 未解析到等）不阻断整体：配置照常进
    /// 缓存，实例跳过并计入返回名单——与启动期 `from_registered` 的渐进配置
    /// 语义一致，部分供应商配错也能刷新其他。运行中 turn 零中断：已在跑的
    /// turn 持有旧实例的 `Arc` 克隆，跑完本轮。
    ///
    /// # 返回
    /// 实例构造被跳过的供应商 id 名单（小写形态；空 = 全部成功注册）。调用方
    /// 可据此向用户呈现「已落盘但暂不可用」的供应商。
    ///
    /// # 错误
    /// 三层配置加载失败（TOML 语法 / IO）→ [`EngineError::Provider`]——落盘
    /// 不可读时无从对齐，整体 fail-loud。
    pub fn reload_providers(&self) -> Result<Vec<String>, EngineError> {
        let agent_paths = &self.params.agent_paths;

        // 1. 三层合并加载：全量落盘定义是对齐的目标状态
        let config = fuyao_api::load_config(agent_paths).map_err(|e| {
            EngineError::Provider(format!("供应商刷新失败：三层配置加载失败（{e}）"))
        })?;
        let providers = config.map(|c| c.providers).unwrap_or_default();

        // 2. .env 运行时增量补载：管理面写回 global 层 .env 的密钥变量发生在
        //    启动期 load_env 之后，不补载则指针变量不在进程环境、解析必然落空
        fuyao_api::load_env_missing(agent_paths);

        // 3. 落盘存在的逐个注册：配置进缓存 + 实例进注册表
        let cache_key = fuyao_provider::agent_paths_cache_key(agent_paths);
        let mut skipped: Vec<String> = Vec::new();
        for (provider_id, provider) in &providers {
            fuyao_provider::register_provider(provider_id, provider.clone(), &cache_key);
            for (model_id, model) in &provider.models {
                let full_id = format!("{provider_id}/{model_id}");
                fuyao_provider::register_model(&full_id, model.clone(), &cache_key);
            }

            // API Key 预检：构造实例对 Key 的唯一硬依赖，缺失时给出含指针变量
            // 名单的可诊断警告并跳过实例（.env 变量被手删是常见成因）。
            // 实例不进注册表，已有旧实例则移除——注册表严格等于可构造子集，
            // 不留旧凭证的隐藏状态
            if fuyao_provider::resolve_api_key(provider_id, agent_paths).is_none() {
                tracing::warn!(
                    provider_id = %provider_id,
                    env_vars = ?provider.api_key_env_vars,
                    "供应商实例构造跳过：API Key 未解析到（options.api_key 未配置且指针变量未设置，请检查 global 层 .env）"
                );
                skipped.push(provider_id.to_lowercase());
                self.providers.unregister(provider_id);
                continue;
            }

            let instance = match fuyao_provider::build_provider(provider_id, provider, agent_paths)
            {
                Ok(instance) => instance,
                Err(cause) => {
                    tracing::warn!(
                        provider_id = %provider_id,
                        cause = %cause,
                        "供应商实例构造跳过"
                    );
                    skipped.push(provider_id.to_lowercase());
                    self.providers.unregister(provider_id);
                    continue;
                }
            };
            self.providers.register(provider_id, instance);
        }

        // 4. 注册表中有、落盘没有的：实例与缓存条目清除（注册表键为小写形态，
        //    与小写化的落盘键集合做差）
        let disk_ids: std::collections::HashSet<String> =
            providers.keys().map(|k| k.to_lowercase()).collect();
        let stale_ids: Vec<String> = self
            .providers
            .provider_ids()
            .into_iter()
            .filter(|id| !disk_ids.contains(id))
            .collect();
        for stale in &stale_ids {
            self.providers.unregister(stale);
            fuyao_provider::unregister_provider(stale, &cache_key);
            // 缓存中的模型条目按 "{id}/" 前缀清除（full_id 以小写形态存储）
            let prefix = format!("{stale}/");
            let model_ids: Vec<String> = fuyao_provider::list_models(agent_paths)
                .keys()
                .filter(|full_id| full_id.starts_with(&prefix))
                .cloned()
                .collect();
            for full_id in model_ids {
                fuyao_provider::unregister_model(&full_id, &cache_key);
            }
        }

        tracing::info!(
            registered = providers.len(),
            skipped = skipped.len(),
            removed = stale_ids.len(),
            "供应商注册表已对齐落盘（存活 engine 下一 turn 生效）"
        );
        Ok(skipped)
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
             api_protocol = \"openai-completions\"\n\
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

    /// 刷新：落盘后注册表与缓存立即可查，模型条目一并注册，跳过名单为空
    #[tokio::test]
    async fn reload_registers_disk_providers() {
        let home = tempfile::tempdir().unwrap();
        let paths = unique_paths("reload_register", home.path());
        write_global_config(home.path(), "alpha", "m1");
        let engine = bare_engine(paths.clone()).await;

        assert!(!engine.providers.contains("alpha"), "刷新前不在注册表");
        let skipped = engine.reload_providers().expect("刷新应成功");

        assert!(skipped.is_empty(), "key 齐备不应有跳过：{skipped:?}");
        assert!(engine.providers.contains("alpha"), "刷新后实例可查");
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

    /// 对账：落盘删除后再次刷新，实例与缓存条目（含旗下模型）一并清除
    #[tokio::test]
    async fn reload_removes_providers_absent_from_disk() {
        let home = tempfile::tempdir().unwrap();
        let paths = unique_paths("reload_remove", home.path());
        write_global_config(home.path(), "alpha", "m1");
        let engine = bare_engine(paths.clone()).await;
        engine.reload_providers().unwrap();
        assert!(engine.providers.contains("alpha"), "首次刷新后实例可查");

        // 落盘换成另一个供应商（alpha 消失）
        write_global_config(home.path(), "beta", "m2");
        engine.reload_providers().unwrap();

        assert!(!engine.providers.contains("alpha"), "落盘已删的实例应清除");
        assert!(
            fuyao_provider::get_provider("alpha", &paths).is_none(),
            "配置缓存应清"
        );
        assert!(
            fuyao_provider::get_model("alpha/m1", &paths).is_none(),
            "模型缓存条目应清"
        );
        assert!(engine.providers.contains("beta"), "新落盘的实例可查");

        fuyao_provider::clear_cache(&paths);
    }

    /// 落盘键大小写混合（如 [providers.Alpha]）：以小写形态注册仍能命中
    /// （注册缓存与实例表的键均小写归一，落盘键大小写不应造成刷新 miss）
    #[tokio::test]
    async fn reload_matches_case_insensitive_disk_key() {
        let home = tempfile::tempdir().unwrap();
        let paths = unique_paths("reload_case_key", home.path());
        write_global_config(home.path(), "Alpha", "m1");
        let engine = bare_engine(paths.clone()).await;

        let skipped = engine.reload_providers().expect("刷新应成功");
        assert!(skipped.is_empty(), "大写落盘键不应导致跳过：{skipped:?}");

        assert!(engine.providers.contains("alpha"), "实例以小写键可查");
        assert!(
            fuyao_provider::get_model("alpha/m1", &paths).is_some(),
            "模型条目以小写复合键可查"
        );

        fuyao_provider::clear_cache(&paths);
    }

    /// API Key 缺失（options 未配、指针变量未设）：进跳过名单、实例不注册，
    /// 已有旧实例移除（注册表不留旧凭证状态），其他供应商照常刷新
    #[tokio::test]
    async fn reload_skips_provider_with_missing_key() {
        let home = tempfile::tempdir().unwrap();
        let paths = unique_paths("reload_no_key", home.path());

        // 先落盘可用供应商 gamma（明文 key），刷新使其实例入表
        write_global_config(home.path(), "gamma", "m1");
        let engine = bare_engine(paths.clone()).await;
        engine.reload_providers().unwrap();
        assert!(engine.providers.contains("gamma"), "key 齐备时实例入表");

        // 落盘改写：key 换成未设置的指针变量（.env 未写、环境变量未设）
        std::fs::write(
            home.path().join("fuyao.toml"),
            "[providers.gamma]\nname = \"G\"\napi_protocol = \"openai-completions\"\n\
             api_key_env_vars = [\"GAMMA_API_KEY\"]\n\
             [providers.delta]\nname = \"D\"\n\
             api_protocol = \"openai-completions\"\n\
             options = { api_key = \"sk-ok\" }\n\
             [providers.delta.models.\"m2\"]\nname = \"m2\"\nlimit = { context = 64000 }\n",
        )
        .unwrap();
        let skipped = engine.reload_providers().expect("刷新应成功");

        assert_eq!(skipped, vec!["gamma".to_string()], "仅 gamma 进跳过名单");
        assert!(
            !engine.providers.contains("gamma"),
            "key 缺失的旧实例应移除"
        );
        assert!(
            fuyao_provider::get_provider("gamma", &paths).is_some(),
            "落盘存在的配置仍进缓存"
        );
        assert!(engine.providers.contains("delta"), "其他供应商照常刷新");

        fuyao_provider::clear_cache(&paths);
    }

    /// 三层配置加载失败（TOML 语法坏）：整体 fail-loud
    #[tokio::test]
    async fn reload_bad_config_fails_loud() {
        let home = tempfile::tempdir().unwrap();
        let paths = unique_paths("reload_bad_toml", home.path());
        std::fs::write(home.path().join("fuyao.toml"), "not [ valid").unwrap();
        let engine = bare_engine(paths.clone()).await;

        let err = engine.reload_providers().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("三层配置加载失败"), "错误指向配置加载：{msg}");

        fuyao_provider::clear_cache(&paths);
    }

    /// 配置了未实现协议（如 anthropic-messages）：加载可过、配置进缓存，但实例
    /// 构造在工厂分派处明确报「尚未实现」跳过——其余供应商照常刷新
    #[tokio::test]
    async fn reload_skips_unimplemented_protocol() {
        let home = tempfile::tempdir().unwrap();
        let paths = unique_paths("reload_unimplemented", home.path());
        // alpha 为可用供应商（openai-completions + 明文 key）；beta 配置合法但
        // 协议未实现
        std::fs::write(
            home.path().join("fuyao.toml"),
            "[providers.alpha]\nname = \"A\"\napi_protocol = \"openai-completions\"\n\
             options = { api_key = \"sk-ok\" }\n\
             [providers.alpha.models.\"m1\"]\nname = \"m1\"\nlimit = { context = 64000 }\n\
             [providers.beta]\nname = \"B\"\napi_protocol = \"anthropic-messages\"\n\
             options = { api_key = \"sk-ok\" }\n\
             [providers.beta.models.\"m2\"]\nname = \"m2\"\nlimit = { context = 64000 }\n",
        )
        .unwrap();
        let engine = bare_engine(paths.clone()).await;

        let skipped = engine.reload_providers().expect("刷新应成功");

        assert_eq!(skipped, vec!["beta".to_string()], "仅未实现协议进跳过名单");
        assert!(!engine.providers.contains("beta"), "未实现协议不注册实例");
        assert!(
            fuyao_provider::get_provider("beta", &paths).is_some(),
            "落盘存在的配置仍进缓存（实例与配置解耦）"
        );
        assert!(engine.providers.contains("alpha"), "其余供应商照常刷新");

        fuyao_provider::clear_cache(&paths);
    }
}
