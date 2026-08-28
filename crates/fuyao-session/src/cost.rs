//! 费用计算器
//!
//! 所有费用相关的计算**统一在本模块**，业务层（core 等）只调本模块的函数，
//! 不直接做任何 cost 运算（避免精度处理散落）。
//!
//! 提供：
//! - [`calculate_cost`]：单条消息费用（Decimal 精确，按模型价格表算）
//! - [`fill_message_cost`]：按 msg 已填的 token 字段算出 cost 并填入
//!
//! 两者都是纯函数：价格表（[`ModelCost`]）由调用方查好传入——「查价格」归
//! 调用方（注册表 / 事件流在其职责域），本模块只管「怎么算」。
//!
//! session 总计（total_* / total_cost）的累积不由本模块负责——在
//! [`SessionStore::insert_message`] 的事务内（DB 唯一数据源，SQL 原子自增）。
//! 需要精确总额时从 `messages.cost` 列 `SUM` 重算。

use fuyao_api::{Message, ModelCost};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

/// 价格 / 百万 token
const ONE_MILLION: Decimal = Decimal::from_parts(1000000, 0, 0, false, 0);

/// 单条消息的 token 用量（计费输入）
///
/// 四个桶的语义：
/// - `prompt`：输入总量（含 `cached` 命中部分）
/// - `completion`：输出总量（含 `reasoning` 部分）
/// - `reasoning`：其中推理 token 数
/// - `cached`：其中缓存命中 token 数
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// 输入 token 数（含缓存命中）
    pub prompt: i64,
    /// 输出 token 数（含推理）
    pub completion: i64,
    /// 推理 token 数（completion 的子集）
    pub reasoning: i64,
    /// 缓存命中 token 数（prompt 的子集）
    pub cached: i64,
}

impl TokenUsage {
    /// 从 assistant Message 的四个 token 字段提取
    pub fn from_message(msg: &Message) -> Self {
        Self {
            prompt: msg.prompt_tokens,
            completion: msg.completion_tokens,
            reasoning: msg.reasoning_tokens,
            cached: msg.cached_tokens,
        }
    }
}

/// token 桶 × 单价的行项费用
///
/// 桶非正（含负值钳制）或缺价记 0——缺价的桶免费是价格表的语义
/// （如梯度内未配某字段）。
fn line(tokens: i64, price: Option<Decimal>) -> Decimal {
    match price {
        Some(p) if tokens > 0 => Decimal::from(tokens) * p / ONE_MILLION,
        _ => Decimal::ZERO,
    }
}

/// 计算单条消息费用
///
/// 计价规则：四个互不重叠的 token 桶各乘单价后求和——
/// 非缓存输入（prompt − cached）、普通输出（completion − reasoning）、
/// 推理、缓存命中。推理桶缺独立价时回退输出价（多数供应商 reasoning 与
/// output 同价或未单列）。
///
/// # Arguments
/// * `cost` - 模型价格表（平价 + 梯度；梯度按 `usage.prompt` 命中）
/// * `usage` - token 用量四桶
///
/// # Returns
/// 费用（Decimal）；全缺价 / 全零桶时返回 `Decimal::ZERO`
pub fn calculate_cost(cost: &ModelCost, usage: TokenUsage) -> Decimal {
    let prices = cost.unit_prices(usage.prompt);

    line(usage.prompt - usage.cached, prices.input)
        + line((usage.completion - usage.reasoning).max(0), prices.output)
        + line(usage.reasoning, prices.reasoning.or(prices.output))
        + line(usage.cached, prices.cache)
}

/// 填 assistant Message 的 cost 字段（token 字段由调用方先行填好）
///
/// 「何时计费」的知识归 history 模块（进历史统一入口）——本函数只负责
/// 「怎么算」：读 msg 已填的四个 token 字段，按价格表算出 cost 并填入。
/// token 字段不经本函数转填：事件 payload 的 token 字段与 usage 同源
/// （拦截不改 usage），映射器直接从事件取值。
///
/// 注：`msg.cost` 字段是 f64（DB schema 决定），Decimal → f64 转换在这一步发生。
/// session 总计的累积由 `insert_message` 事务内 SQL 原子自增完成（DB 唯一数据源）；
/// 需要精确总额时从 `messages.cost` 列 `SUM` 重算，避免 f64 多次相加漂移。
pub fn fill_message_cost(msg: &mut Message, cost: &ModelCost) {
    let amount = calculate_cost(cost, TokenUsage::from_message(msg));
    msg.cost = amount.to_f64().unwrap_or(0.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::PriceTier;

    /// 构造 Decimal 价格字面量（字符串解析，测试内可读）
    fn d(v: &str) -> Decimal {
        v.parse().unwrap()
    }

    /// 平价表构造（价格/M）
    fn flat_cost(
        input: Option<Decimal>,
        output: Option<Decimal>,
        reasoning: Option<Decimal>,
        cache: Option<Decimal>,
    ) -> ModelCost {
        ModelCost {
            input,
            output,
            reasoning,
            cache,
            tiers: Vec::new(),
        }
    }

    #[test]
    fn calculate_simple_cost() {
        // 1000 输入 + 500 输出：1000×2/M + 500×12/M = 0.002 + 0.006 = 0.008
        let cost = calculate_cost(
            &flat_cost(Some(d("2")), Some(d("12")), None, None),
            TokenUsage {
                prompt: 1000,
                completion: 500,
                ..TokenUsage::default()
            },
        );
        assert_eq!(cost, d("0.008"));
    }

    #[test]
    fn calculate_with_cache() {
        // (1000-400)×2/M + 500×12/M + 400×0.4/M = 0.0012 + 0.006 + 0.00016
        let cost = calculate_cost(
            &flat_cost(Some(d("2")), Some(d("12")), None, Some(d("0.4"))),
            TokenUsage {
                prompt: 1000,
                completion: 500,
                cached: 400,
                ..TokenUsage::default()
            },
        );
        assert_eq!(cost, d("0.00736"));
    }

    #[test]
    fn calculate_zero_cost_when_no_prices() {
        let cost = calculate_cost(
            &flat_cost(None, None, None, None),
            TokenUsage {
                prompt: 1000,
                completion: 500,
                ..TokenUsage::default()
            },
        );
        assert_eq!(cost, Decimal::ZERO);
    }

    #[test]
    fn reasoning_without_own_price_bills_at_output_price() {
        // reasoning 缺独立价：全部 completion（含 reasoning）按输出价计
        // 600×12/M + 400×12/M = 1200×12/M = 0.0144
        let cost = calculate_cost(
            &flat_cost(None, Some(d("12")), None, None),
            TokenUsage {
                prompt: 0,
                completion: 1200,
                reasoning: 400,
                ..TokenUsage::default()
            },
        );
        assert_eq!(cost, d("0.0144"));
    }

    #[test]
    fn reasoning_at_output_price_equals_whole_completion() {
        // reasoning 价 == output 价：拆桶求和与整体计价结果一致
        let split = calculate_cost(
            &flat_cost(None, Some(d("12")), Some(d("12")), None),
            TokenUsage {
                prompt: 0,
                completion: 1200,
                reasoning: 400,
                ..TokenUsage::default()
            },
        );
        assert_eq!(split, d("0.0144"));
    }

    #[test]
    fn reasoning_with_own_price_bills_separately() {
        // reasoning 独立价 4/M：(1200-400)×12/M + 400×4/M = 0.0096 + 0.0016
        let cost = calculate_cost(
            &flat_cost(None, Some(d("12")), Some(d("4")), None),
            TokenUsage {
                prompt: 0,
                completion: 1200,
                reasoning: 400,
                ..TokenUsage::default()
            },
        );
        assert_eq!(cost, d("0.0112"));
    }

    #[test]
    fn negative_plain_output_bucket_clamps_to_zero() {
        // usage 异常（reasoning > completion）：普通输出桶钳为 0，不产生负费用
        let cost = calculate_cost(
            &flat_cost(None, Some(d("12")), Some(d("4")), None),
            TokenUsage {
                prompt: 0,
                completion: 100,
                reasoning: 300,
                ..TokenUsage::default()
            },
        );
        assert_eq!(cost, d("0.0012"));
    }

    #[test]
    fn tiers_override_flat_prices_per_matched_tier() {
        // 梯度非空时整体取代平价：命中梯度内缺价的桶免费，不回退平价
        let cost = ModelCost {
            input: Some(d("999")),
            output: Some(d("999")),
            reasoning: None,
            cache: Some(d("999")),
            tiers: vec![PriceTier {
                max_tokens: 200_000,
                input: Some(d("2")),
                output: Some(d("12")),
                reasoning: None,
                cache: None,
            }],
        };
        // 2000 prompt（全部未命中缓存）+ 500 输出：2000×2/M + 500×12/M = 0.01
        let amount = calculate_cost(
            &cost,
            TokenUsage {
                prompt: 2000,
                completion: 500,
                cached: 100,
                ..TokenUsage::default()
            },
        );
        // 梯度内无 cache 价：100 cached 不收费，且输入按非缓存口径 1900 计
        // 1900×2/M + 500×12/M = 0.0098
        assert_eq!(amount, d("0.0098"));
    }

    #[test]
    fn fill_message_cost_writes_cost_only() {
        // fill_message_cost 读四个 token 字段算 cost 填入，token 字段原样保留
        let mut msg = Message::assistant(Some("resp".to_string()));
        msg.prompt_tokens = 1000;
        msg.completion_tokens = 500;
        msg.reasoning_tokens = 0;
        msg.cached_tokens = 0;

        fill_message_cost(
            &mut msg,
            &flat_cost(Some(d("2")), Some(d("12")), None, None),
        );

        assert_eq!(msg.prompt_tokens, 1000);
        assert_eq!(msg.completion_tokens, 500);
        assert_eq!(msg.reasoning_tokens, 0);
        assert_eq!(msg.cached_tokens, 0);
        assert!((msg.cost - 0.008).abs() < 0.0000001);
    }
}
