//! 会话分裂与血统链解析（上下文压缩专用）
//!
//! 设计核心：外部 session_id 与数据库主键解耦。
//!
//! - 外部 session_id（用户、调度表、Emitter、事件标签）：根会话 id，永远不变。
//! - 数据库实际使用的记录：血统链的末端——"最后一个分裂出来的、没有更后 child 的"。
//!
//! 血统链结构（链式，靠 parent_session_id 串起来）：
//!
//! ```text
//! root (parent_session_id=NULL)  ←  用户持有的 id
//!   └── child1 (parent_session_id=root)   ←  第一次压缩 split 出来的
//!         └── child2 (parent_session_id=child1)  ←  第二次压缩 split 出来的（当前真实）
//! ```
//!
//! 每次压缩，链加一节；链的末端就是"当前使用的"会话。
//! 通过 [`SessionStore::resolve_current`](super::SessionStore::resolve_current) 从根 id 解析出当前末端 id。

use crate::error::SessionError;
use fuyao_api::{Message, Session};

impl super::SessionStore {
    /// 分裂会话（上下文压缩专用）
    ///
    /// 完成两件事：
    /// 1. 标记 `parent_id` 对应的会话为已结束（`end_reason='compression'` + `ended_at=now`）
    /// 2. 创建新会话作为它的子节点（`parent_session_id` 指向 `parent_id`），
    ///    新会话继承压缩后的消息历史（通常是 [摘要] + [保留窗口]）
    ///
    /// 新会话的 token 统计从 `compressed_messages` 累加（保持会话级统计准确）；
    /// 其他统计字段（tool_call_count 等）按消息角色统计。
    ///
    /// # 参数
    /// - `parent_id`：被压缩的会话 id（将成为新会话的 parent_session_id）
    /// - `new_system_prompt`：新会话的系统提示词（压缩后通常会重新构建）
    /// - `compressed_messages`：压缩后的消息列表
    /// - `title`：新会话标题
    ///
    /// # 错误
    /// - [`SessionError::NotFound`]：`parent_id` 在数据库中不存在
    pub async fn split_session(
        &self,
        parent_id: &str,
        new_system_prompt: Option<String>,
        compressed_messages: Vec<Message>,
        title: Option<String>,
    ) -> Result<Session, SessionError> {
        // 验证 parent 存在（不存在直接报错，避免创建孤儿 child）
        let parent_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)")
                .bind(parent_id)
                .fetch_one(&self.pool)
                .await?;
        if !parent_exists {
            return Err(SessionError::NotFound(parent_id.to_string()));
        }

        // 标记 parent 结束（end_reason 固定为 'compression'，便于后续按原因过滤）
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        sqlx::query("UPDATE sessions SET ended_at = ?2, end_reason = 'compression' WHERE id = ?1")
            .bind(parent_id)
            .bind(now)
            .execute(&self.pool)
            .await?;

        // 构造新会话：parent_session_id 指向老会话，建立血统链
        let mut new_session = Session::new(title, new_system_prompt);
        new_session.parent_session_id = Some(parent_id.to_string());

        // 累加压缩消息的统计到会话级别（保持 session 级统计与消息一致）
        for msg in &compressed_messages {
            new_session.total_prompt_tokens += msg.prompt_tokens;
            new_session.total_completion_tokens += msg.completion_tokens;
            new_session.total_reasoning_tokens += msg.reasoning_tokens;
            new_session.total_cached_tokens += msg.cached_tokens;
            new_session.total_cost += msg.cost;
            if msg.role == "tool" {
                new_session.tool_call_count += 1;
            }
        }
        new_session.message_count = compressed_messages.len() as i64;
        new_session.messages = compressed_messages;

        // 写入数据库（create 内部用事务，sessions + messages 原子写入）
        self.create(&new_session).await?;

        tracing::info!(
            parent_id = parent_id,
            new_session_id = %new_session.id,
            message_count = new_session.message_count,
            "会话分裂完成（上下文压缩）"
        );
        Ok(new_session)
    }

    /// 解析血统链的末端会话 id（当前真实使用的 id）
    ///
    /// 从 `root_id` 出发，沿着 `parent_session_id` 链递归查找直接 child：
    /// - 找到 → 当前 id 更新为 child id，继续查找下一节
    /// - 找不到 → 当前 id 即为末端，返回
    ///
    /// 多分支场景（同一 parent 有多个 child，如编排层的 fork）：按 `started_at DESC`
    /// 取最新创建的 child——压缩血统是线性的，但理论上 parent_session_id 字段
    /// 也可能被其他场景使用，此处保守取最新。
    ///
    /// # 返回
    /// - `Some(current_id)`：root 存在，返回末端 id（若未分裂过，等于 root_id 自身）
    /// - `None`：root_id 在数据库中不存在
    pub async fn resolve_current(&self, root_id: &str) -> Result<Option<String>, SessionError> {
        // 验证 root 存在（不存在返回 None，区别于内部 sqlx 错误）
        let root_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)")
                .bind(root_id)
                .fetch_one(&self.pool)
                .await?;
        if !root_exists {
            return Ok(None);
        }

        // 沿 parent_session_id 链递归找末端
        // ponytail: 递归深度等于压缩次数，实际场景不超过十几次，循环即可
        let mut current_id = root_id.to_string();
        loop {
            let child_id: Option<String> = sqlx::query_scalar(
                "SELECT id FROM sessions WHERE parent_session_id = ?1 ORDER BY started_at DESC LIMIT 1",
            )
            .bind(&current_id)
            .fetch_optional(&self.pool)
            .await?;

            match child_id {
                Some(child) => current_id = child,
                None => break, // 没有 child：current_id 即为末端
            }
        }

        Ok(Some(current_id))
    }

    /// 获取血统链末端的会话（含消息）
    ///
    /// 组合 [`resolve_current`](Self::resolve_current) + [`get`](Self::get)：
    /// 解析当前真实使用的 id，再加载该会话的完整数据（含 messages）。
    ///
    /// 这是引擎加载会话时的标准入口——外部 session_id 是稳定的根 id，
    /// 但内部实际使用的是分裂链的末端记录（压缩后切换过的新会话）。
    ///
    /// # 返回
    /// - `Some(Session)`：root 存在，返回末端会话（含消息）
    /// - `None`：root_id 在数据库中不存在
    pub async fn get_current(&self, root_id: &str) -> Result<Option<Session>, SessionError> {
        match self.resolve_current(root_id).await? {
            Some(current_id) => self.get(&current_id).await,
            None => Ok(None),
        }
    }
}
