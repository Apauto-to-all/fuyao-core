//! fuyao-session 集成测试：标题生成端到端
//!
//! src/title_generator.rs 的单元测试只覆盖了纯函数（truncate_chars / clean_title），
//! maybe_generate_title 本身（含 ProviderRegistry 取实例、provider.chat、清洗全链路）
//! 完全未覆盖——本文件补足这一最大单测空白。
//!
//! 覆盖：
//! - fast 模型未配置时回退主模型路径（默认 config 的 models.fast 是 None）
//! - 主模型可用时正常生成 + clean_title 清洗（去引号、去前缀、限长）
//! - 主模型调用失败时返回 None（放弃重命名）
//!
//! 标题仅基于用户首条消息生成（不等 AI 回复），测试入参只传 user_message。
//!
//! mock 决策：手写 FakeTitleProvider（Provider trait 标了 #[async_trait]，automock 不适用）。
//! 全局状态规避：maybe_generate_title 读 get_config().models.fast，默认 None → 走 fallback，
//! 不调 set_config（规避 OnceLock 串扰）。

mod common;

use common::{registry_with_failing_provider, registry_with_title_provider};
use fuyao_session::maybe_generate_title;

/// 默认 config 下 fast 模型未配置，maybe_generate_title 直接走主模型路径
fn main_model_id() -> &'static str {
    // "prov/any" → parse_model_id 得 provider_id="prov"，从 registry.get("prov") 取 fake 实例
    "prov/any"
}

// ============================================================================
// 正常生成路径：fast 未配置 → 主模型 → 生成 + 清洗
// ============================================================================

#[tokio::test]
async fn generates_title_from_main_model_when_fast_unconfigured() {
    // 默认 config（fast=None）→ 回退主模型 → fake chat 返回标题 → clean_title 清洗
    let providers = registry_with_title_provider("关于 Rust 异步的讨论");
    let paths = common::unique_paths("title_ok");

    let title =
        maybe_generate_title("讲讲 Rust 的 async", main_model_id(), &providers, &paths).await;

    assert_eq!(title.as_deref(), Some("关于 Rust 异步的讨论"));
    common::cleanup_unused(&paths);
}

#[tokio::test]
async fn clean_title_strips_quotes_from_generated() {
    // fake 返回带引号的标题 → clean_title 应去引号
    let providers = registry_with_title_provider("\"测试结果分析\"");
    let paths = common::unique_paths("title_quotes");

    let title = maybe_generate_title("分析测试", main_model_id(), &providers, &paths).await;

    assert_eq!(title.as_deref(), Some("测试结果分析"));
    common::cleanup_unused(&paths);
}

#[tokio::test]
async fn clean_title_strips_title_prefix() {
    // fake 返回带 "标题:" 前缀 → clean_title 应去前缀
    let providers = registry_with_title_provider("标题: 数据库设计要点");
    let paths = common::unique_paths("title_prefix");

    let title = maybe_generate_title("设计数据库", main_model_id(), &providers, &paths).await;

    assert_eq!(title.as_deref(), Some("数据库设计要点"));
    common::cleanup_unused(&paths);
}

#[tokio::test]
async fn clean_title_truncates_overlong() {
    // fake 返回超长标题 → clean_title 按 max_len（默认 80）截断并加 "..."
    // 构造明确超过 80 字符的标题（100 个字符）
    let long = "深度学习模型训练优化策略与超参数调优方法详解研究".repeat(5); // 22 × 5 = 110 字符
    let providers = registry_with_title_provider(&long);
    let paths = common::unique_paths("title_long");

    let title = maybe_generate_title("讲讲训练", main_model_id(), &providers, &paths)
        .await
        .expect("应生成标题");

    // 默认 max_len=80，超长应被截断并加省略号
    assert!(
        title.ends_with("..."),
        "超长标题应被截断并以 ... 结尾，实际：{title}"
    );
    assert!(
        title.chars().count() <= 80 + 3,
        "截断后长度应 <= max_len + 3 个省略号字符，实际：{}",
        title.chars().count()
    );
    common::cleanup_unused(&paths);
}

// ============================================================================
// 失败路径：返回 None
// ============================================================================

#[tokio::test]
async fn returns_none_when_main_model_call_fails() {
    // fast 未配置 + 主模型 chat 失败（fake 返回错误）→ maybe_generate_title 返回 None
    let providers = registry_with_failing_provider();
    let paths = common::unique_paths("title_fail");

    let title = maybe_generate_title("提问", main_model_id(), &providers, &paths).await;

    assert!(title.is_none(), "主模型失败时应返回 None");
    common::cleanup_unused(&paths);
}

#[tokio::test]
async fn returns_none_when_generated_title_empty_after_clean() {
    // fake 返回纯空白 → clean_title 清洗后为空 → should return None（generate_title 内部判定）
    let providers = registry_with_title_provider("   ");
    let paths = common::unique_paths("title_empty");

    let title = maybe_generate_title("提问", main_model_id(), &providers, &paths).await;

    assert!(title.is_none(), "清洗后为空应返回 None");
    common::cleanup_unused(&paths);
}
