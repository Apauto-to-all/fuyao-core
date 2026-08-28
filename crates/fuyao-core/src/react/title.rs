//! 会话标题自动生成（fire-and-forget 旁路功能）
//!
//! 首轮 user 消息落库后触发，不等 AI 回复。判定成本：每 session 仅一次——
//! [`SessionCtx::title_gate`](super::SessionCtx::title_gate) 保证首个批次判定过后
//! 原子跳过后续所有轮次，判定本身只发一条 COUNT 查询（不加载消息体），
//! 标题内容直接取自刚注入的批（不回读 DB）。
//!
//! 两层标题：判定通过后**同步**落占位标题（首条 user 内容截断，本地零成本，
//! 与 `enabled` 无关）并广播 Title 事件，列表立即可辨识；随后 LLM 异步生成，
//! 成功后覆盖占位（第二个 Title 事件），失败或生成关闭时占位即终值。

use super::SessionCtx;
use fuyao_api::message::EventBase;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::output::{TitleMessage, TitlePayload};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// 首轮用户消息注入后触发标题自动生成（每 session 至多判定一次）
///
/// 在 `inject_user_messages` 落库后由主循环调用（早于 `run_turn`，不等 AI 回复），
/// 解决旧逻辑「等 AI 整轮回复完成才生成」的延迟硬伤与长回复拖累问题。
///
/// 触发条件（按序短路，全部满足才进入标题流程）：
/// - 本 session 首次经过本函数：[`SessionCtx::title_gate`] 原子换防，之后所有轮次
///   零成本返回（含配置关闭 / 非首轮的情形——标题配置为进程级静态，
///   首次判定即终局，无需每轮重评）
/// - DB 中 `role=user` 的普通消息数严格等于 1（首轮判定：计数法比
///   `title=="新会话"` 更稳——用户可能改过 title；COUNT 查询不加载消息体）
/// - 能取到首条 user content（调用方从刚注入的批传入，不回读 DB）
///
/// 通过判定后两步走：
/// 1. 同步落占位标题（首条 user 内容截断）并广播 Title 事件——本地零成本，
///    与 `enabled` 无关，保证生成关闭 / 失败时列表也有可辨识标题
/// 2. `[session.title] enabled = true` 时 `tokio::spawn` 异步 LLM 生成，成功后覆盖占位
///
/// 执行模型：LLM 生成是 `tokio::spawn` 独立 task，不阻塞主 ReAct 循环。
/// spawn 的 future 是 `'static` 的——标题直接走 `SessionStore::update_session`
/// 局部 SQL 落库，内存态不更新（下次 resume 时从 DB 自然读回）。
///
/// 多 session 并发天然安全：clone `Arc<store>` / `Arc<providers>` / `emitter` /
/// `hooks` / `agent_paths` 进 task，各 session task 独立，零共享零协调。
pub(super) async fn maybe_spawn_title(ctx: &SessionCtx, first_user_content: Option<&str>) {
    // 每 session 一次：首个批次原子换防，后续轮次零成本返回
    if ctx.title_gate.swap(true, Ordering::Relaxed) {
        return;
    }

    // 计数法判定首轮：注入后 user 消息数严格等于 1（COUNT 查询，不加载消息体）。
    // 等价于「全新会话且本批恰 1 条」——恢复的带历史会话计数 >1，自然跳过
    let user_count = match ctx
        .store
        .count_user_messages(ctx.emitter.session_id())
        .await
    {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(
                session_id = ctx.emitter.session_id(),
                cause = %e,
                "标题生成前统计 user 消息失败，跳过"
            );
            return;
        }
    };
    if user_count != 1 {
        return;
    }

    // 首条 user content 取自调用方传入的本批首条（不回读 DB）；
    // to_string 拿所有权——spawn 的 future 是 'static，不能借用本函数参数
    let Some(user_content) = first_user_content.map(str::to_string) else {
        return;
    };

    // 占位标题：首条 user 内容首行截断，同步落库并广播（不等 LLM）。
    // 内容为空白（如纯图片消息）时跳过，保持「新会话」
    let placeholder = placeholder_title(&user_content);
    if !placeholder.is_empty() {
        match ctx
            .store
            .update_session(ctx.emitter.session_id(), Some(&placeholder), None)
            .await
        {
            Ok(()) => {
                // 占位标题同样走 dispatch 管道广播，UI 即时可见
                crate::dispatch::dispatch(
                    &ctx.emitter,
                    &ctx.hooks,
                    OutputEvent::Title(TitleMessage {
                        base: EventBase::default(),
                        payload: TitlePayload { title: placeholder },
                    }),
                )
                .await;
            }
            Err(e) => {
                tracing::warn!(
                    session_id = ctx.emitter.session_id(),
                    cause = %e,
                    "占位标题落库失败"
                );
            }
        }
    }

    if !fuyao_api::get_config().session.title.enabled {
        return;
    }

    // 标题生成回退用的主模型 ID：从 session_params 现读模型配置（ReAct 主循环同款快照）。
    // 读不到则空串，maybe_generate_title 内部会因 model_id 无法解析返回 None。
    let main_model_id = {
        let params = ctx.session_params.lock().await;
        params.model_config.model_id.clone()
    };

    // clone 'static 依赖进 spawn（所有字段都是 Send + 'static）
    let store = Arc::clone(&ctx.store);
    let providers = Arc::clone(&ctx.providers);
    let emitter = ctx.emitter.clone();
    let hooks = ctx.hooks.clone();
    let agent_paths = ctx.agent_paths.clone();
    let session_id = ctx.emitter.session_id().to_string();

    tokio::spawn(async move {
        match fuyao_session::maybe_generate_title(
            &user_content,
            &main_model_id,
            &providers,
            &agent_paths,
        )
        .await
        {
            Some(title) => {
                // 局部 UPDATE 落库（失败仅 warn，不影响主流程）
                if let Err(e) = store.update_session(&session_id, Some(&title), None).await {
                    tracing::warn!(session_id = %session_id, cause = %e, "标题落库失败");
                    return;
                }
                // 发 Title 事件：经 dispatch 管道（拦截 → 发送 → 观察）
                crate::dispatch::dispatch(
                    &emitter,
                    &hooks,
                    OutputEvent::Title(TitleMessage {
                        base: EventBase::default(),
                        payload: TitlePayload { title },
                    }),
                )
                .await;
            }
            None => tracing::debug!(session_id = %session_id, "标题生成跳过（无可用标题）"),
        }
    });
}

/// 由首条 user 内容构造占位标题
///
/// 取首行（多行输入只保留第一行，避免标题含换行），超长按 `[session.title]
/// max_len` 截断并加省略号「…」（不切断多字节字符）。内容为空白时返回空串，
/// 调用方据此跳过占位、保持「新会话」。
fn placeholder_title(content: &str) -> String {
    let first_line = content.trim().lines().next().unwrap_or("");
    if first_line.is_empty() {
        return String::new();
    }
    let max_len = fuyao_api::get_config().session.title.max_len;
    if first_line.chars().count() > max_len {
        let truncated: String = first_line.chars().take(max_len).collect();
        format!("{truncated}…")
    } else {
        first_line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::placeholder_title;

    #[test]
    fn placeholder_title_takes_first_line() {
        assert_eq!(placeholder_title("第一行\n第二行"), "第一行");
    }

    #[test]
    fn placeholder_title_truncates_with_ellipsis() {
        let long = "很".repeat(100);
        let title = placeholder_title(&long);
        assert_eq!(title.chars().count(), 81);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn placeholder_title_keeps_short_content() {
        assert_eq!(placeholder_title("  简短问题 "), "简短问题");
    }

    #[test]
    fn placeholder_title_empty_for_blank_content() {
        assert_eq!(placeholder_title("   \n  "), "");
    }
}
