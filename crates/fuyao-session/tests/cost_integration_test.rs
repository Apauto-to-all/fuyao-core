//! fuyao-session 集成测试：费用计算与全局 model 注册表的真实集成
//!
//! src/cost.rs 的单元测试覆盖纯计算逻辑（calculate_cost 收价格表参数，
//! 不碰注册表）。本文件聚焦**跨 crate 的真实集成**——单测的空白带：
//!
//! - 注册表 → 计费的端到端接线：register_model 注入价格表 → get_model 查出
//!   ModelCost → fill_message_cost 算出 cost 填入 Message
//! - 全局 MODEL_CACHE 的隔离契约：不同 agent_paths_key 不串扰
//!
//! 全局状态规避：用唯一 agent_id 隔离 MODEL_CACHE，测完 clear_cache 收尾。

mod common;

use common::{priced_model, unique_paths};
use fuyao_api::Message;
use fuyao_provider::{agent_paths_cache_key, clear_cache, get_model, register_model};

/// 注册带固定价格（输入 2/M、输出 12/M、缓存 0.4/M）的模型并返回 agent_paths + key + full_id
struct RegisteredModel {
    paths: fuyao_api::AgentPaths,
    full_id: String,
}

/// 注册模型并返回路径信息（调用方测完需调 cleanup）
fn register_priced_model(tag: &str) -> RegisteredModel {
    let paths = unique_paths(tag);
    let key = agent_paths_cache_key(&paths);
    let full_id = "prov/test-model".to_string();
    register_model(&full_id, priced_model("test-model"), &key);
    RegisteredModel { paths, full_id }
}

/// 收尾：清理全局 MODEL_CACHE 中该 agent_paths 的条目
fn cleanup(paths: &fuyao_api::AgentPaths) {
    clear_cache(paths);
}

// ============================================================================
// 注册表 → 计费端到端
// ============================================================================

#[test]
fn registry_model_costs_message_end_to_end() {
    // 注册价格表后，get_model 查出 ModelCost 喂 fill_message_cost，
    // Message.cost 得到非零费用（跨 session ↔ provider registry 的真实集成）
    let reg = register_priced_model("e2e");
    let mut msg = Message::assistant(Some("resp".to_string()));
    msg.prompt_tokens = 1000;
    msg.completion_tokens = 500;
    msg.reasoning_tokens = 0;
    msg.cached_tokens = 0;

    let model = get_model(&reg.full_id, &reg.paths).expect("注册后应能查到模型");
    fuyao_session::fill_message_cost(&mut msg, &model.cost);

    // 1000×2/M + 500×12/M = 0.008
    assert!((msg.cost - 0.008).abs() < 0.0000001, "cost = {}", msg.cost);
    cleanup(&reg.paths);
}

#[test]
fn unregistered_model_misses_lookup_before_billing() {
    // 未注册的 model_id：查价 miss 由调用方处理，计费函数只收价格表
    let paths = unique_paths("unregistered");
    assert!(get_model("prov/unknown", &paths).is_none());
    cleanup(&paths);
}

// ============================================================================
// 全局 MODEL_CACHE 隔离契约
// ============================================================================

#[test]
fn model_cache_isolated_per_agent_paths() {
    // 不同 agent_paths_key 的注册表互不串扰：
    // agent A 注册了模型，agent B 查不到（get_model 返回 None）
    let paths_a = unique_paths("iso_a");
    let paths_b = unique_paths("iso_b");
    let key_a = agent_paths_cache_key(&paths_a);
    register_model("prov/m", priced_model("m"), &key_a);

    // A 能查到，B 查不到
    assert!(
        get_model("prov/m", &paths_a).is_some(),
        "A 应能查到自注册的模型"
    );
    assert!(
        get_model("prov/m", &paths_b).is_none(),
        "B 不应查到 A 的模型"
    );

    cleanup(&paths_a);
    cleanup(&paths_b);
}

#[test]
fn clear_cache_removes_registered_model() {
    // clear_cache 后，已注册的模型查不到（验证收尾清理生效，避免污染其他测试）
    let reg = register_priced_model("clear");
    assert!(
        get_model(&reg.full_id, &reg.paths).is_some(),
        "清理前应能查到"
    );
    cleanup(&reg.paths);
    assert!(
        get_model(&reg.full_id, &reg.paths).is_none(),
        "clear_cache 后应查不到"
    );
}
