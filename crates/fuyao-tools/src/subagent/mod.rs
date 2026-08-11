//! 子代理工具模块
//!
//! 派生子代理执行独立子任务，同步等结果回喂父 ReAct。
//! 子代理跑完整 ReAct（可能多轮工具调用），产出最终回复后即结束退出。
//!
//! 递归防护：本工具标 `child_invisible = true`，对子 session 隐藏——
//! 子代理不能再派生子代理（[`SubagentOps`] 注入虽在，但 LLM 看不到本工具就不会调）。

mod handler;
mod types;

use std::collections::HashMap;

use fuyao_api::ToolEntry;
use fuyao_api::{ToolDefinition, ToolFn};
use handler::subagent_handler;

/// 注册 subagent 工具
pub fn register(map: &mut HashMap<&'static str, ToolEntry>) {
    let handler: ToolFn = std::sync::Arc::new(|args, ctx, cancel| {
        Box::pin(async move { subagent_handler(args, &ctx, cancel).await })
    });

    let definition = ToolDefinition::builder(
        "subagent",
        "派生子代理执行独立子任务并返回最终回复。子代理拥有完整 ReAct 循环（可调工具），但不可再派生子代理（递归防护）。任务指令应自包含所有必要上下文。subagent_type 从系统提示词「子代理」索引中选取，决定子代理的人格与系统提示词。",
    )
    .string(
        "subagent_type",
        "子代理定义名（系统提示词中「子代理」索引列出的 name，如 explore / executor）",
    )
    .required()
    .string("description", "3-5 词任务描述（简明扼要，供追踪显示）")
    .required()
    .string("prompt", "给子代理的完整任务指令（应包含所有必要上下文，子代理不继承父会话历史）")
    .required()
    .build();

    map.insert(
        "subagent",
        ToolEntry {
            definition,
            handler,
            child_invisible: true, // 对子 session 隐藏（递归防护：子代理不能再派生子代理）
        },
    );
}
