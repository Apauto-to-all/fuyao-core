//! 费用计算器
//!
//! 所有费用相关的计算与累积**统一在本模块**，业务层（core 等）只调本模块的函数，
//! 不直接做任何 cost 运算（避免精度处理散落）。
//!
//! 提供：
//! - [`calculate_cost`]：单条消息费用（Decimal 精确，按模型价格表算）
//! - [`fill_message_cost`]：填 Message 的 token + cost 字段（算 + 填一步到位）
//! - [`accumulate_session_total`]：累积 Session.total_*（cost 用 Decimal 精确累加，
//!   避免 f64 直接相加的精度误差）

use fuyao_api::{AgentPaths, Message, PriceTier, Session};
use fuyao_provider::{StreamUsage, get_model};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};

/// 价格 / 百万 token
const ONE_MILLION: Decimal = Decimal::from_parts(1000000, 0, 0, false, 0);

/// Token 数量（内部计算用）
struct TokenCounts {
    prompt: i64,
    completion: i64,
    reasoning: i64,
    cached: i64,
}

/// 根据 prompt_tokens 判断使用哪个 tier
fn get_tier(tiers: &[PriceTier], prompt_tokens: i64) -> &PriceTier {
    let mut sorted: Vec<&PriceTier> = tiers.iter().collect();
    sorted.sort_by_key(|t| t.max_tokens);
    for tier in &sorted {
        if prompt_tokens <= tier.max_tokens as i64 {
            return tier;
        }
    }
    sorted.last().unwrap()
}

/// 将 Option<f64> 转换为 Decimal
fn to_decimal(val: Option<f64>) -> Option<Decimal> {
    val.and_then(Decimal::from_f64)
}

/// 根据价格配置计算费用（Decimal 精确计算，返回 Decimal）
fn calculate_amount(
    input_price: Option<f64>,
    output_price: Option<f64>,
    reasoning_price: Option<f64>,
    cache_price: Option<f64>,
    tokens: &TokenCounts,
) -> Decimal {
    let input_dec = to_decimal(input_price);
    let output_dec = to_decimal(output_price);
    let reasoning_dec = to_decimal(reasoning_price);
    let cache_dec = to_decimal(cache_price);

    let mut amount = Decimal::ZERO;

    // 1. 输入费用（排除缓存部分）
    let non_cached_input = tokens.prompt - tokens.cached;
    if let Some(input) = input_dec
        && non_cached_input > 0
    {
        amount += Decimal::from(non_cached_input) * input / ONE_MILLION;
    }

    // 2. 输出费用
    if let (Some(reasoning), true) = (reasoning_dec, tokens.reasoning > 0) {
        if reasoning != output_dec.unwrap_or(Decimal::ZERO) {
            // 分开计算 reasoning 和普通输出
            let non_reasoning_tokens = tokens.completion - tokens.reasoning;
            if let Some(output) = output_dec
                && non_reasoning_tokens > 0
            {
                amount += Decimal::from(non_reasoning_tokens) * output / ONE_MILLION;
            }
            amount += Decimal::from(tokens.reasoning) * reasoning / ONE_MILLION;
        } else if let Some(output) = output_dec
            && tokens.completion > 0
        {
            amount += Decimal::from(tokens.completion) * output / ONE_MILLION;
        }
    } else if let Some(output) = output_dec
        && tokens.completion > 0
    {
        amount += Decimal::from(tokens.completion) * output / ONE_MILLION;
    }

    // 3. 缓存费用
    if let Some(cache) = cache_dec
        && tokens.cached > 0
    {
        amount += Decimal::from(tokens.cached) * cache / ONE_MILLION;
    }

    amount
}

/// 计算单条消息费用
///
/// 根据 model_id 查找价格配置，使用 Decimal 精确计算。
///
/// # Arguments
/// * `model_id` - 完整模型 ID（如 `provider/model` 形式）
/// * `prompt_tokens` - 输入 token 数
/// * `completion_tokens` - 输出 token 数
/// * `reasoning_tokens` - 推理 token 数
/// * `cached_tokens` - 缓存命中 token 数
/// * `agent_paths` - Agent 三层目录身份证明（按此查模型注册表）
///
/// # Returns
/// 费用（Decimal），无价格配置时返回 `Decimal::ZERO`
pub fn calculate_cost(
    model_id: &str,
    prompt_tokens: i64,
    completion_tokens: i64,
    reasoning_tokens: i64,
    cached_tokens: i64,
    agent_paths: &AgentPaths,
) -> Decimal {
    let model = match get_model(model_id, agent_paths) {
        Some(m) => m,
        None => return Decimal::ZERO,
    };

    let tokens = TokenCounts {
        prompt: prompt_tokens,
        completion: completion_tokens,
        reasoning: reasoning_tokens,
        cached: cached_tokens,
    };

    if !model.cost.tiers.is_empty() {
        let tier = get_tier(&model.cost.tiers, prompt_tokens);
        return calculate_amount(tier.input, tier.output, tier.reasoning, tier.cache, &tokens);
    }

    calculate_amount(
        model.cost.input,
        model.cost.output,
        model.cost.reasoning,
        model.cost.cache,
        &tokens,
    )
}

/// 填 assistant Message 的 token + cost 字段
///
/// 由 emit_to_history 闭包调用——闭包构造 Message 时一步完成"填 token + 算 cost"。
/// 拦截不改 usage（token 是模型给的客观值），计费用原始 result.usage。
///
/// model_id 缺失（理论不应发生）时跳过 cost 计算只填 token——不会污染 session 总计。
///
/// 注：`msg.cost` 字段是 f64（DB schema 决定），Decimal → f64 转换在这一步发生。
/// 精确累积在 [`accumulate_session_total`] 里保证（用 Decimal 临时转换避免 f64 加法误差）。
pub fn fill_message_cost(
    msg: &mut Message,
    usage: &StreamUsage,
    model_id: Option<&str>,
    agent_paths: &AgentPaths,
) {
    // 1. 填 token 字段（整数，无精度问题）
    msg.prompt_tokens = usage.prompt_tokens as i64;
    msg.completion_tokens = usage.completion_tokens as i64;
    msg.reasoning_tokens = usage.completion_reasoning_tokens.unwrap_or(0) as i64;
    msg.cached_tokens = usage.prompt_cached_tokens.unwrap_or(0) as i64;

    // 2. 算 cost 并填进 msg.cost（Decimal 精确算，落 f64 时损失在所难免——DB schema 决定）
    if let Some(mid) = model_id {
        let cost = calculate_cost(
            mid,
            msg.prompt_tokens,
            msg.completion_tokens,
            msg.reasoning_tokens,
            msg.cached_tokens,
            agent_paths,
        );
        msg.cost = cost.to_f64().unwrap_or(0.0);
    }
}

/// 累积 Session.total_*（cost 用 Decimal 精确累加，避免 f64 加法误差）
///
/// 仅 assistant 角色的消息累积（只有 LLM 调用产生 token / cost）。
/// token 是整数，加法无精度问题；cost 用 Decimal 临时转换做精确加法。
///
/// 落库走 `SessionStore::update`，全量 UPDATE 把 session.total_* 和 messages.cost 写入 DB。
pub fn accumulate_session_total(session: &mut Session, msg: &Message) {
    if msg.role != "assistant" {
        return;
    }

    // token 累加（整数，无精度问题）
    session.total_prompt_tokens += msg.prompt_tokens;
    session.total_completion_tokens += msg.completion_tokens;
    session.total_reasoning_tokens += msg.reasoning_tokens;
    session.total_cached_tokens += msg.cached_tokens;

    // cost 用 Decimal 精确累加：f64 → Decimal → 相加 → f64
    // 避免 f64 直接相加累积精度误差
    let total = Decimal::from_f64(session.total_cost).unwrap_or(Decimal::ZERO)
        + Decimal::from_f64(msg.cost).unwrap_or(Decimal::ZERO);
    session.total_cost = total.to_f64().unwrap_or(0.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculate_simple_cost() {
        // 测试简单计算：1000 输入 token，500 输出 token
        let cost = calculate_amount(
            Some(2.0),  // 输入价格 2/M
            Some(12.0), // 输出价格 12/M
            None,
            None,
            &TokenCounts {
                prompt: 1000,
                completion: 500,
                reasoning: 0,
                cached: 0,
            },
        );
        // 1000 * 2/1000000 + 500 * 12/1000000 = 0.002 + 0.006 = 0.008
        let expected = Decimal::from_f64(0.008).unwrap();
        assert!((cost - expected).abs() < Decimal::from_f64(0.0001).unwrap());
    }

    #[test]
    fn calculate_with_cache() {
        let cost = calculate_amount(
            Some(2.0),  // 输入价格 2/M
            Some(12.0), // 输出价格 12/M
            None,
            Some(0.4), // 缓存价格 0.4/M
            &TokenCounts {
                prompt: 1000,
                completion: 500,
                reasoning: 0,
                cached: 400,
            },
        );
        // (1000-400) * 2/1000000 + 500 * 12/1000000 + 400 * 0.4/1000000
        // = 0.0012 + 0.006 + 0.00016 = 0.00736
        let expected = Decimal::from_f64(0.00736).unwrap();
        assert!((cost - expected).abs() < Decimal::from_f64(0.00001).unwrap());
    }

    #[test]
    fn calculate_zero_cost() {
        let cost = calculate_amount(
            None,
            None,
            None,
            None,
            &TokenCounts {
                prompt: 0,
                completion: 0,
                reasoning: 0,
                cached: 0,
            },
        );
        assert_eq!(cost, Decimal::ZERO);
    }

    // ===== accumulate_session_total 测试 =====

    #[test]
    fn accumulate_skips_non_assistant_message() {
        // tool / user / system 角色不累积（只有 LLM 调用产生 token/cost）
        let mut session = Session::default();
        let mut msg = Message::user("hi".to_string());
        msg.cost = 1.5;
        accumulate_session_total(&mut session, &msg);
        assert_eq!(session.total_cost, 0.0, "user 消息不应累积");
        assert_eq!(session.total_prompt_tokens, 0);

        let mut msg = Message::tool_result("c1".into(), "result".into());
        msg.cost = 2.0;
        accumulate_session_total(&mut session, &msg);
        assert_eq!(session.total_cost, 0.0, "tool 消息不应累积");
    }

    #[test]
    fn accumulate_assistant_message_tokens_and_cost() {
        let mut session = Session::default();
        let mut msg = Message::assistant(Some("hi".to_string()));
        msg.prompt_tokens = 100;
        msg.completion_tokens = 50;
        msg.reasoning_tokens = 10;
        msg.cached_tokens = 20;
        msg.cost = 0.008;

        accumulate_session_total(&mut session, &msg);

        assert_eq!(session.total_prompt_tokens, 100);
        assert_eq!(session.total_completion_tokens, 50);
        assert_eq!(session.total_reasoning_tokens, 10);
        assert_eq!(session.total_cached_tokens, 20);
        assert!((session.total_cost - 0.008).abs() < 1e-9);
    }

    #[test]
    fn accumulate_uses_decimal_precision_for_repeated_adds() {
        // 多次累加同一个会损失 f64 精度的小数 cost——验证 Decimal 累积无误差
        // 0.1 + 0.1 + ... + 0.1 (10 次) = 1.0（f64 直接加可能得 0.9999999999...）
        let mut session = Session::default();
        let mut msg = Message::assistant(Some("x".to_string()));
        msg.cost = 0.1;
        for _ in 0..10 {
            accumulate_session_total(&mut session, &msg);
        }

        // f64 直接加 0.1 * 10 通常得 0.9999999999999999；Decimal 累加得精确 1.0
        let diff = (session.total_cost - 1.0).abs();
        assert!(
            diff < 1e-10,
            "Decimal 累积应精确等于 1.0，实际 = {}（误差 {}）",
            session.total_cost,
            diff
        );
    }

    #[test]
    fn accumulate_chain_does_not_drift() {
        // 长链累积：1000 条消息每条 0.001 cost——验证不会随累积次数漂移
        let mut session = Session::default();
        let mut msg = Message::assistant(Some("x".to_string()));
        msg.cost = 0.001;
        for _ in 0..1000 {
            accumulate_session_total(&mut session, &msg);
        }

        // 1000 * 0.001 = 1.0；f64 直接加会累积误差
        let diff = (session.total_cost - 1.0).abs();
        assert!(
            diff < 1e-9,
            "长链累积应精确等于 1.0，实际 = {}（误差 {}）",
            session.total_cost,
            diff
        );
    }
}
