//! 工具执行器函数类型与 handler 侧辅助
//!
//! 定义工具执行器的函数签名（[`ToolFn`]），以及注册时吸收闭包包装体操的
//! [`tool_handler`]、handler 入口的类型化参数解析 [`parse_args`]。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

/// 工具执行器函数类型
///
/// 接收 LLM 传入的 JSON 参数、框架注入的调用上下文、取消令牌，返回统一结果信封。
///
/// # 参数
/// - `serde_json::Value`: LLM 传入的工具调用参数
/// - `crate::ToolCallContext`: 框架注入的调用上下文（会话 ID、Agent 路径等）
/// - `CancellationToken`: 本次工具批次的中断信号（派生自 session shutdown_token）
///
///   短任务 handler 可忽略（参数加占位下划线，被 abort 即可，双保险兜底）；
///   长任务 handler 应在内部 `select!` 监听 `cancelled()`，命中后优雅收尾
///   （杀子进程 / 释放外部资源）并返回取消标记。
///
/// # 返回
/// - [`crate::tool::ToolOutput`]: 统一结果信封（JSON 对象 / 纯文本 / 错误）
pub type ToolFn = Arc<
    dyn Fn(
            serde_json::Value,
            crate::ToolCallContext,
            CancellationToken,
        ) -> Pin<Box<dyn Future<Output = crate::tool::ToolOutput> + Send>>
        + Send
        + Sync,
>;

/// 把规范签名的异步函数包装成 [`ToolFn`]
///
/// 工具 handler 统一写规范签名 `async fn(args, ctx, cancel) -> ToolOutput`
/// （不用的参数加下划线），注册时经本函数一步包装——
/// `Arc` / `Box::pin` 的类型体操集中在此，工具站点零样板：
///
/// ```no_run
/// # use fuyao_api::{tool_handler, ToolDefinition, ToolEntry, ToolOutput};
/// # use fuyao_api::{CancellationToken, ToolCallContext};
/// # use std::collections::HashMap;
/// # async fn read_impl(
/// #     args: serde_json::Value, ctx: ToolCallContext, _cancel: CancellationToken,
/// # ) -> ToolOutput { todo!() }
/// let entry = ToolEntry::new(
///     ToolDefinition::builder("read", "读取文件").build(),
///     tool_handler(read_impl),
///     false,
/// );
/// let mut map = HashMap::new();
/// fuyao_api::insert_tool(&mut map, entry);
/// ```
pub fn tool_handler<F, Fut>(f: F) -> ToolFn
where
    F: Fn(serde_json::Value, crate::ToolCallContext, CancellationToken) -> Fut
        + Send
        + Sync
        + 'static,
    Fut: Future<Output = crate::tool::ToolOutput> + Send + 'static,
{
    Arc::new(move |args, ctx, cancel| Box::pin(f(args, ctx, cancel)))
}

/// 把 LLM 传入的 JSON 参数类型化解析为参数结构体
///
/// 工具参数结构体用 `#[derive(Deserialize)]` 声明字段类型与默认值，
/// 经本函数一步反序列化——替代 handler 内逐字段 `args.get(...).as_str()`
/// 手搓解析（字段名 / 类型 / 默认值在 schema 构建器与结构体间共享常量，不再分叉）。
///
/// 解析失败返回含修正建议的错误信封（常见 serde 错误译为中文），帮助 LLM 对照参数说明自行修正重试。
pub fn parse_args<T: DeserializeOwned>(
    args: serde_json::Value,
) -> Result<T, crate::tool::ToolError> {
    serde_json::from_value(args).map_err(|e| {
        crate::tool::ToolError::new(translate_parse_error(&e.to_string())).with(
            "suggestion",
            "请对照工具参数说明检查字段名与类型，可选字段可省略",
        )
    })
}

/// 把常见 serde_json 反序列化错误译为中文（面向 LLM 的错误文本统一中文）
///
/// 只翻译两种最高频的形态（缺必填字段 / 类型不匹配），其余原样透传，
/// 并裁掉对 LLM 无意义的行号后缀（" at line 1 column 2"）。
fn translate_parse_error(msg: &str) -> String {
    let msg = msg.split(" at line ").next().unwrap_or(msg);
    if let Some(rest) = msg.strip_prefix("missing field `") {
        let field = rest.trim_end_matches('`');
        format!("缺少必填参数 `{field}`")
    } else if let Some(rest) = msg.strip_prefix("invalid type: ") {
        format!("参数类型不正确：{rest}")
    } else {
        msg.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCallContext;
    use crate::tool::ToolOutput;
    use serde_json::json;

    /// 规范签名的示例 handler（忽略 cancel）
    async fn sample_handler(
        _args: serde_json::Value,
        _ctx: ToolCallContext,
        _cancel: CancellationToken,
    ) -> ToolOutput {
        ToolOutput::text("done")
    }

    #[tokio::test]
    async fn tool_handler_wraps_canonical_fn() {
        let f = tool_handler(sample_handler);
        let out = f(
            json!({}),
            ToolCallContext::default(),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(out.to_wire(), "done");
    }

    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct SampleArgs {
        path: String,
        #[serde(default = "default_limit")]
        limit: i64,
    }

    fn default_limit() -> i64 {
        500
    }

    #[test]
    fn parse_args_deserializes_typed_struct_with_default() {
        let args: SampleArgs = parse_args(json!({"path": "a.txt"})).expect("解析失败");
        assert_eq!(
            args,
            SampleArgs {
                path: "a.txt".into(),
                limit: 500
            }
        );
    }

    #[test]
    fn parse_args_missing_required_field_returns_suggestive_error() {
        let err = parse_args::<SampleArgs>(json!({})).expect_err("缺必填字段应报错");
        assert!(
            err.message.contains("缺少必填参数"),
            "实际：{}",
            err.message
        );
        assert!(err.message.contains("path"), "实际：{}", err.message);
        assert_eq!(
            err.extras["suggestion"],
            json!("请对照工具参数说明检查字段名与类型，可选字段可省略")
        );
    }

    #[test]
    fn parse_args_type_mismatch_translated_to_chinese() {
        // limit 给字符串 → invalid type 错误译为中文且不带行号后缀
        let err = parse_args::<SampleArgs>(json!({"path": "a.txt", "limit": "many"}))
            .expect_err("类型不匹配应报错");
        assert!(
            err.message.contains("参数类型不正确"),
            "实际：{}",
            err.message
        );
        assert!(!err.message.contains(" at line "), "实际：{}", err.message);
    }
}
