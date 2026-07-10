//! 费用计算器
//!
//! 根据 model_id 查找价格配置，使用 Decimal 精确计算费用。
//! 从 fuyao-provider 移入，聚合在 session 模块下。

use fuyao_api::AgentPaths;
use fuyao_api::PriceTier;
use fuyao_provider::get_model;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;

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
/// * `model_id` - 模型 ID（如 "aliyun/qwen3.6-plus"）
/// * `prompt_tokens` - 输入 token 数
/// * `completion_tokens` - 输出 token 数
/// * `reasoning_tokens` - 推理 token 数
/// * `cached_tokens` - 缓存命中 token 数
/// * `agent_paths` - Agent 路径配置
///
/// # Returns
/// 费用（Decimal），无价格配置时返回 Decimal::ZERO
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
}
