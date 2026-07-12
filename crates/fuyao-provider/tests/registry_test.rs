//! fuyao-provider 注册表隔离语义集成测试
//!
//! 钉死 PROVIDER_CACHE / MODEL_CACHE 全局单例的隔离契约：
//! - 不同 agent_paths_key 的注册互不干扰
//! - get 大小写归一（register "Aliyun" → get "ALIYUN" 命中）
//! - clear_cache 只清当前 key，不波及其他 agent_id
//!
//! 全局状态规避：每个测试用唯一 agent_id（integration/{test_name}）+ 收尾 clear_cache。
//! 注意：这里的 Provider 是 fuyao_api 的配置结构体（非 Provider trait）。

use std::collections::HashMap;
use std::path::PathBuf;

use fuyao_api::{
    AgentPaths, ModelCost, ModelLimit, ModelModalities, Provider as ProviderConfig, ProviderOptions,
};
use fuyao_provider::{
    agent_paths_cache_key, clear_cache, get_model, get_provider, list_models, list_providers,
    register_model, register_provider,
};

/// 构造唯一 agent_id 的 AgentPaths（隔离全局缓存）
fn unique_paths(test_name: &str) -> AgentPaths {
    AgentPaths {
        agent_id: Some(format!("integration/{test_name}")),
        workspace: None,
        // 注入 fuyao_home 避免触碰真实环境
        fuyao_home: PathBuf::from("/tmp/fuyao_test_home"),
        ..Default::default()
    }
}

fn sample_provider(name: &str) -> ProviderConfig {
    ProviderConfig {
        name: name.to_string(),
        models: HashMap::new(),
        options: ProviderOptions::default(),
        api_key_env_vars: Vec::new(),
    }
}

fn sample_model(name: &str) -> fuyao_api::Model {
    fuyao_api::Model {
        name: name.to_string(),
        cost: ModelCost::default(),
        limit: ModelLimit::default(),
        reasoning_efforts: vec![],
        modalities: ModelModalities::default(),
    }
}

// ---------------------------------------------------------------------------
// register / get 基础契约
// ---------------------------------------------------------------------------

#[test]
fn register_and_get_provider_roundtrip() {
    let paths = unique_paths("register_get_provider");
    let key = agent_paths_cache_key(&paths);
    register_provider("myprov", sample_provider("myprov"), &key);

    let got = get_provider("myprov", &paths);
    assert!(got.is_some(), "注册后应能 get 到");
    clear_cache(&paths);
}

#[test]
fn register_and_get_model_roundtrip() {
    let paths = unique_paths("register_get_model");
    let key = agent_paths_cache_key(&paths);
    register_model("myprov/mymodel", sample_model("mymodel"), &key);

    let got = get_model("myprov/mymodel", &paths);
    assert!(got.is_some(), "注册后应能 get 到");
    clear_cache(&paths);
}

// ---------------------------------------------------------------------------
// 不同 agent_paths 互不干扰（全局单例的隔离契约）
// ---------------------------------------------------------------------------

#[test]
fn different_agent_paths_do_not_share_cache() {
    let paths_a = unique_paths("isolation_a");
    let paths_b = unique_paths("isolation_b");
    let key_a = agent_paths_cache_key(&paths_a);

    // 只在 a 注册
    register_provider("shared-name", sample_provider("from-a"), &key_a);

    // b 不应看到 a 的注册
    let got_b = get_provider("shared-name", &paths_b);
    assert!(got_b.is_none(), "不同 agent_paths 不应共享缓存");

    // a 应能看到
    let got_a = get_provider("shared-name", &paths_a);
    assert!(got_a.is_some(), "a 应能 get 到自己的注册");

    clear_cache(&paths_a);
}

// ---------------------------------------------------------------------------
// 大小写归一
// ---------------------------------------------------------------------------

#[test]
fn provider_lookup_is_case_insensitive() {
    let paths = unique_paths("case_insensitive");
    let key = agent_paths_cache_key(&paths);
    register_provider("Aliyun", sample_provider("Aliyun"), &key);

    // 用大写查询应命中小写注册（内部归一化为小写）
    assert!(get_provider("ALIYUN", &paths).is_some(), "大写应命中");
    assert!(get_provider("aliyun", &paths).is_some(), "小写应命中");
    assert!(get_provider("AlIyUn", &paths).is_some(), "混合大小写应命中");

    clear_cache(&paths);
}

#[test]
fn model_lookup_is_case_insensitive() {
    let paths = unique_paths("model_case_insensitive");
    let key = agent_paths_cache_key(&paths);
    register_model("DeepSeek/V4-Flash", sample_model("V4-Flash"), &key);

    assert!(
        get_model("deepseek/v4-flash", &paths).is_some(),
        "全小写应命中混合大小写注册"
    );

    clear_cache(&paths);
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

#[test]
fn list_providers_returns_all_registered() {
    let paths = unique_paths("list_providers");
    let key = agent_paths_cache_key(&paths);
    register_provider("alpha", sample_provider("alpha"), &key);
    register_provider("beta", sample_provider("beta"), &key);

    let list = list_providers(&paths);
    assert_eq!(list.len(), 2);
    assert!(list.contains_key("alpha"));
    assert!(list.contains_key("beta"));

    clear_cache(&paths);
}

#[test]
fn list_models_returns_all_registered() {
    let paths = unique_paths("list_models");
    let key = agent_paths_cache_key(&paths);
    register_model("prov/model-a", sample_model("model-a"), &key);
    register_model("prov/model-b", sample_model("model-b"), &key);

    let list = list_models(&paths);
    assert_eq!(list.len(), 2);
    assert!(list.contains_key("prov/model-a"));
    assert!(list.contains_key("prov/model-b"));

    clear_cache(&paths);
}

// ---------------------------------------------------------------------------
// clear_cache 只清当前 key
// ---------------------------------------------------------------------------

#[test]
fn clear_cache_only_affects_current_agent_paths() {
    let paths_a = unique_paths("clear_scope_a");
    let paths_b = unique_paths("clear_scope_b");
    let key_a = agent_paths_cache_key(&paths_a);
    let key_b = agent_paths_cache_key(&paths_b);

    register_provider("prov-a", sample_provider("prov-a"), &key_a);
    register_provider("prov-b", sample_provider("prov-b"), &key_b);

    // 清 a，b 不受影响
    clear_cache(&paths_a);
    assert!(
        get_provider("prov-a", &paths_a).is_none(),
        "a 被清后应 get 不到"
    );
    assert!(
        get_provider("prov-b", &paths_b).is_some(),
        "清 a 不应影响 b"
    );

    clear_cache(&paths_b);
}

#[test]
fn clear_cache_on_empty_agent_paths_does_not_panic() {
    let paths = unique_paths("clear_empty");
    // 从未注册，clear 应安全（不 panic）
    clear_cache(&paths);
}

// ---------------------------------------------------------------------------
// cache_key 格式
// ---------------------------------------------------------------------------

#[test]
fn cache_key_includes_agent_id_and_workspace() {
    let paths = AgentPaths {
        agent_id: Some("integration/cache-key-test".to_string()),
        workspace: Some(PathBuf::from("/tmp/ws")),
        fuyao_home: PathBuf::from("/tmp/home"),
        ..Default::default()
    };
    let key = agent_paths_cache_key(&paths);
    assert!(
        key.contains("integration/cache-key-test"),
        "key 应含 agent_id"
    );
    assert!(key.contains("/tmp/ws"), "key 应含 workspace");
}
