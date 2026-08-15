//! fuyao-session 集成测试：费用计算与全局 model 注册表的真实集成
//!
//! src/cost.rs 的单元测试覆盖了 calculate_amount 的纯逻辑（用私有
//! calculate_amount 绕过全局注册表）。本文件聚焦**跨 crate 的真实集成**——单测的空白带：
//!
//! - calculate_cost 通过 fuyao_provider::get_model 查全局 MODEL_CACHE 算价（真实集成）
//! - register_model 注入价格表后，calculate_cost 返回非零费用（验证注册表联动）
//! - 全局 MODEL_CACHE 的隔离契约：不同 agent_paths_key 不串扰
//! - fill_message_cost 端到端：按 msg 已填 token 字段算出 cost 填入
//!
//! 全局状态规避：用唯一 agent_id 隔离 MODEL_CACHE，测完 clear_cache 收尾。

mod common;

use common::{priced_model, unique_paths};
use fuyao_api::Message;
use fuyao_provider::{agent_paths_cache_key, clear_cache, get_model, register_model};
use fuyao_session::{calculate_cost, fill_message_cost};
use rust_decimal::Decimal;

/// 用整数分数构造 Decimal，避免 f64 精度问题
fn dec(numerator: i64, denominator: i64) -> Decimal {
    Decimal::from(numerator) / Decimal::from(denominator)
}

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
// calculate_cost 真实集成：注册表联动
// ============================================================================

#[tokio::test]
async fn calculate_cost_returns_zero_for_unregistered_model() {
    // 未注册的 model_id：get_model 返回 None，calculate_cost 应返回零
    let paths = unique_paths("unregistered");
    let cost = calculate_cost("prov/unknown", 1000, 500, 0, 0, &paths);
    assert_eq!(cost, Decimal::ZERO);
    cleanup(&paths);
}

#[test]
fn calculate_cost_nonzero_after_register_model() {
    // 注册价格表后，calculate_cost 应返回非零费用（跨 session ↔ provider registry 的真实集成）
    let reg = register_priced_model("nonzero");
    // 输入 1000 token、输出 500 token：1000×2/M + 500×12/M = 0.002 + 0.006 = 0.008
    let cost = calculate_cost(&reg.full_id, 1000, 500, 0, 0, &reg.paths);
    assert!(cost > Decimal::ZERO, "注册后费用应非零");

    // 0.008 = 8/1000
    let expected = dec(8, 1000);
    assert!(
        (cost - expected).abs() < dec(1, 10000),
        "费用应约等于 0.008，实际 = {cost}"
    );
    cleanup(&reg.paths);
}

#[test]
fn calculate_cost_applies_cache_discount() {
    // 缓存命中的 token 按缓存价（0.4/M）计，非缓存部分按输入价（2/M）
    // 1000 prompt 中 400 cached：(1000-400)×2/M + 500×12/M + 400×0.4/M
    // = 0.0012 + 0.006 + 0.00016 = 0.00736
    let reg = register_priced_model("cache");
    let cost = calculate_cost(&reg.full_id, 1000, 500, 0, 400, &reg.paths);
    // 0.00736 = 736/100000
    let expected = dec(736, 100000);
    assert!(
        (cost - expected).abs() < dec(1, 1000000),
        "缓存折扣后费用应约等于 0.00736，实际 = {cost}"
    );
    cleanup(&reg.paths);
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

    // B 的 calculate_cost 为零（无价格表）
    let cost_b = calculate_cost("prov/m", 1000, 500, 0, 0, &paths_b);
    assert_eq!(cost_b, Decimal::ZERO);

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

// ============================================================================
// fill_message_cost 端到端
// ============================================================================

#[test]
fn fill_message_cost_sets_tokens_and_cost() {
    // fill_message_cost 读 msg 已填的 token 字段算 cost 填进 msg.cost
    //（token 字段由调用方——history 映射——自事件 payload 先行填好）
    let reg = register_priced_model("fill");
    let mut msg = Message::assistant(Some("resp".to_string()));
    msg.prompt_tokens = 1000;
    msg.completion_tokens = 500;
    msg.reasoning_tokens = 0;
    msg.cached_tokens = 0;

    fill_message_cost(&mut msg, &reg.full_id, &reg.paths);

    // token 字段保持调用方所填
    assert_eq!(msg.prompt_tokens, 1000);
    assert_eq!(msg.completion_tokens, 500);
    assert_eq!(msg.reasoning_tokens, 0);
    assert_eq!(msg.cached_tokens, 0);
    // cost 已算（约 0.008）
    assert!(msg.cost > 0.0, "cost 应非零");
    assert!((msg.cost - 0.008).abs() < 0.0001);
    cleanup(&reg.paths);
}
