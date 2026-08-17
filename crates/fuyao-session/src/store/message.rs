//! 消息持久化与查询
//!
//! messages 表的全部操作归此:
//! - 写入:`insert_message`(事件级落库的唯一入口)/ `insert_messages_batch`
//!   (单事务批量落库,整段历史搬运用)
//! - 计数:`count_messages`(只数普通消息,排除压缩边界)
//! - 查询:
//!   - `load_full_history`(全量,审计用,seq 升序)
//!   - `list_messages_before`(游标分页,给人看的历史浏览,seq 倒序)
//!
//! 给 LLM 构造 `ChatRequest` 的「可见窗口」查询(压缩感知动态拼接)在 [`super::visible_window`]
//! 模块,与本模块的「给人看的」查询路径正交。
//!
//! # 设计要点
//!
//! 消息产生即落库(事件级落库),不进任何内存数组——单个 session 的内存占用恒定
//! (不随历史增长),多 session 并发无内存压力。需要历史消息时(构造 ChatRequest /
//! 压缩 / 标题生成 / 中断补发等)通过本模块的方法按需查询。
//!
//! seq 生成规则:插入时事务内以 `COALESCE(MAX(seq), 0) + 1` 分配新 seq,
//! 事务保证并发安全。普通消息与 compaction 边界消息共享同一 seq 序列。
//! 单条路径(`insert_message`)把该标量子查询折叠进 INSERT 的 VALUES 槽位,
//! `RETURNING seq` 取回分配值;批量路径(`insert_messages_batch`)事务内取一次
//! 起点后连续递增分配,分配规则相同。

use super::row::MessageRow;
use crate::error::SessionError;
use fuyao_api::Message;

impl super::SessionStore {
    // ── 写入 ───────────────────────────────────────────────────

    /// 单条消息落库:分配 seq 并 INSERT,回填到 msg.seq
    ///
    /// 这是消息进 DB 的唯一入口(事件级落库),所有产出消息(user / assistant /
    /// tool_result / 中断补发 / 工具执行结果)必经此入口。
    ///
    /// 事务内原子完成两件事(单一数据源,杜绝内存镜像覆盖):
    /// 1. INSERT 消息行——seq 分配折叠进 VALUES 的标量子查询(`COALESCE(MAX(seq), 0) + 1`,
    ///    并发安全),`RETURNING seq` 取回分配值;commit 成功后才回填 `msg.seq`
    ///    (失败路径不动调用方数据)
    /// 2. UPDATE sessions 累加计数与费用(条件化,见下)
    ///
    /// # sessions 表原子累加规则
    ///
    /// 仅 `kind=Message` 的普通消息触发累加(compaction 边界消息由
    /// [`mark_compaction`](super::SessionStore::mark_compaction) 独立路径处理,
    /// 不走本方法):
    /// - `message_count += 1`(任何普通消息)
    /// - `tool_call_count += 1`(仅 `role=Tool`)
    /// - `total_prompt_tokens += msg.prompt_tokens` 等 token 四项(仅 `role=Assistant`
    ///   有非零值;user/tool 消息这些字段恒为 0,加 0 无害)
    /// - `total_cost += msg.cost`(同上)
    /// - `last_active_at = unixepoch()`(每次落消息刷新最近活动时间)
    ///
    /// 事务保证消息与统计同生共死,不再有"消息进 DB 但计数遗漏"的中间态。
    /// 费用精度:`f64` 累加在超大 session 有漂移风险,真值源是 `messages.cost`,
    /// 需要精确总额时 `SUM(cost)` 重算。
    pub async fn insert_message(
        &self,
        session_id: &str,
        msg: &mut Message,
    ) -> Result<i64, SessionError> {
        let mut tx = self.pool.begin().await?;

        let next_seq = Self::insert_message_row_allocating_seq(&mut tx, session_id, msg).await?;

        // 普通消息(kind=Message):事务内原子累加 sessions 表统计字段。
        // compaction 边界消息由 mark_compaction 独立路径处理,不走本分支,不误增计数。
        // tool_call_count 按 role=Tool 单独 +1;token/cost 始终加(非 assistant 恒为 0,加 0 无害)。
        if matches!(msg.kind, fuyao_api::MessageKind::Message) {
            let tool_delta: i64 = if matches!(msg.role, fuyao_api::MessageRole::Tool) {
                1
            } else {
                0
            };
            sqlx::query(
                "UPDATE sessions SET
                    message_count = message_count + 1,
                    tool_call_count = tool_call_count + ?2,
                    total_prompt_tokens = total_prompt_tokens + ?3,
                    total_completion_tokens = total_completion_tokens + ?4,
                    total_reasoning_tokens = total_reasoning_tokens + ?5,
                    total_cached_tokens = total_cached_tokens + ?6,
                    total_cost = total_cost + ?7,
                    last_active_at = unixepoch()
                 WHERE id = ?1",
            )
            .bind(session_id)
            .bind(tool_delta)
            .bind(msg.prompt_tokens)
            .bind(msg.completion_tokens)
            .bind(msg.reasoning_tokens)
            .bind(msg.cached_tokens)
            .bind(msg.cost)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        msg.seq = next_seq;
        Ok(next_seq)
    }

    /// 批量消息落库:单事务内连续分配 seq 逐条 INSERT,统计一次聚合写回
    ///
    /// 面向一次搬运整段历史的场景(如 fork 派生会话复制可见消息)。与
    /// [`Self::insert_message`] 单条路径的语义一致:
    /// - **seq 分配**:事务内一次 `SELECT COALESCE(MAX(seq), 0) + 1` 取起点,随后逐条
    ///   递增(事务保证并发安全),分配结果回填到各 `msg.seq`
    /// - **统计累加**:条件与单条路径逐条累加完全等价——仅 `kind=Message` 的普通消息
    ///   参与,`message_count` 按普通消息条数、`tool_call_count` 按 `role=Tool` 条数、
    ///   token 四项与 cost 按各自值求和,一次 UPDATE 聚合写回;
    ///   `last_active_at = unixepoch()` 刷新一次即为终态(逐条路径刷新多次终态相同)
    /// - **原子性**:整批一个事务,任一条失败全部回滚,不产生"落了一半"的中间态
    ///
    /// 空切片直接返回(no-op):不开事务、不写任何行、不刷新统计。
    ///
    /// 费用精度:`f64` 求和与单条路径的逐条累加同为浮点加法,超大批量同样存在
    /// 漂移风险,真值源仍是 `messages.cost` 列(需要精确总额时 `SUM(cost)` 重算)。
    pub async fn insert_messages_batch(
        &self,
        session_id: &str,
        msgs: &mut [Message],
    ) -> Result<(), SessionError> {
        // 空切片 no-op:不开空事务、不做 MAX(seq) 查询
        if msgs.is_empty() {
            return Ok(());
        }

        let mut tx = self.pool.begin().await?;

        let base_seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE session_id = ?1",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;

        // 顺序 INSERT,seq 从起点连续递增分配(与单条路径逐条取 MAX+1 的结果一致)
        for (offset, msg) in msgs.iter().enumerate() {
            Self::insert_message_row(&mut tx, session_id, msg, base_seq + offset as i64).await?;
        }

        // 统计聚合:全部聚合值只对 kind=Message 的普通消息求和(与单条路径的
        // UPDATE 触发条件一致——整批无普通消息时同样不触碰 sessions 行,
        // compaction 边界消息不增计数、不计 token/费用、不刷新 last_active_at)。
        // 求和顺序与切片顺序一致,终态与逐条累加等价。
        let normal: Vec<&Message> = msgs
            .iter()
            .filter(|m| matches!(m.kind, fuyao_api::MessageKind::Message))
            .collect();
        if !normal.is_empty() {
            let message_delta: i64 = normal.len() as i64;
            let tool_delta: i64 = normal
                .iter()
                .filter(|m| matches!(m.role, fuyao_api::MessageRole::Tool))
                .count() as i64;
            let prompt_sum: i64 = normal.iter().map(|m| m.prompt_tokens).sum();
            let completion_sum: i64 = normal.iter().map(|m| m.completion_tokens).sum();
            let reasoning_sum: i64 = normal.iter().map(|m| m.reasoning_tokens).sum();
            let cached_sum: i64 = normal.iter().map(|m| m.cached_tokens).sum();
            let cost_sum: f64 = normal.iter().map(|m| m.cost).sum();

            sqlx::query(
                "UPDATE sessions SET
                    message_count = message_count + ?2,
                    tool_call_count = tool_call_count + ?3,
                    total_prompt_tokens = total_prompt_tokens + ?4,
                    total_completion_tokens = total_completion_tokens + ?5,
                    total_reasoning_tokens = total_reasoning_tokens + ?6,
                    total_cached_tokens = total_cached_tokens + ?7,
                    total_cost = total_cost + ?8,
                    last_active_at = unixepoch()
                 WHERE id = ?1",
            )
            .bind(session_id)
            .bind(message_delta)
            .bind(tool_delta)
            .bind(prompt_sum)
            .bind(completion_sum)
            .bind(reasoning_sum)
            .bind(cached_sum)
            .bind(cost_sum)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        // seq 回填放在 commit 成功之后(与单条路径一致:失败路径不动调用方数据)
        for (offset, msg) in msgs.iter_mut().enumerate() {
            msg.seq = base_seq + offset as i64;
        }

        Ok(())
    }

    /// 在事务内插入一条消息行并内联分配 seq(单条落库路径专用)
    ///
    /// seq 分配折叠进 INSERT 本身:VALUES 的 seq 槽位是标量子查询
    /// `COALESCE(MAX(seq), 0) + 1`(事务保证并发安全),`RETURNING seq`
    /// 把实际分配值带回,省掉一次独立的 MAX(seq) 预查询往返。
    /// 批量路径不适用此形态(一次取起点后连续递增),走显式 seq 的
    /// [`Self::insert_message_row`]。本函数只负责写行并返回分配的 seq,
    /// 不触碰 sessions 统计——统计累加条件由调用方维护。
    async fn insert_message_row_allocating_seq(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        session_id: &str,
        msg: &Message,
    ) -> Result<i64, SessionError> {
        // tool_calls 列存 flat 数组 JSON（typed 直接序列化：id / name / arguments 三字段平铺）
        let tool_calls_json = msg
            .tool_calls
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());

        // 图片列表存 JSON 数组(`[{mime_type, data}]`),空列表存 NULL
        let images_json = (!msg.images.is_empty())
            .then(|| serde_json::to_string(&msg.images).unwrap_or_default());

        let (next_seq,): (i64,) = sqlx::query_as(
            "INSERT INTO messages (session_id, model_id, role, content, images, tool_call_id,
                tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                (SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE session_id = ?1), ?17)
             RETURNING seq",
        )
        .bind(session_id)
        .bind(msg.model_id.as_deref())
        .bind(msg.role.as_str())
        .bind(msg.content.as_deref())
        .bind(images_json.as_deref())
        .bind(msg.tool_call_id.as_deref())
        .bind(tool_calls_json.as_deref())
        .bind(msg.tool_name.as_deref())
        .bind(msg.timestamp)
        .bind(msg.prompt_tokens)
        .bind(msg.completion_tokens)
        .bind(msg.reasoning_tokens)
        .bind(msg.cached_tokens)
        .bind(msg.cost)
        .bind(msg.finish_reason.as_deref())
        .bind(msg.reasoning.as_deref())
        .bind(msg.kind.as_str())
        .fetch_one(&mut **tx)
        .await?;
        Ok(next_seq)
    }

    /// 在事务内以指定 seq 插入一条消息行(批量落库路径的 INSERT)
    ///
    /// seq 由调用方分配(事务内取一次起点后连续递增),本函数只负责写行,
    /// 不触碰 sessions 统计——统计累加条件由调用方维护。
    async fn insert_message_row(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        session_id: &str,
        msg: &Message,
        seq: i64,
    ) -> Result<(), SessionError> {
        // tool_calls 列存 flat 数组 JSON（typed 直接序列化：id / name / arguments 三字段平铺）
        let tool_calls_json = msg
            .tool_calls
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());

        // 图片列表存 JSON 数组(`[{mime_type, data}]`),空列表存 NULL
        let images_json = (!msg.images.is_empty())
            .then(|| serde_json::to_string(&msg.images).unwrap_or_default());

        sqlx::query(
            "INSERT INTO messages (session_id, model_id, role, content, images, tool_call_id,
                tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        )
        .bind(session_id)
        .bind(msg.model_id.as_deref())
        .bind(msg.role.as_str())
        .bind(msg.content.as_deref())
        .bind(images_json.as_deref())
        .bind(msg.tool_call_id.as_deref())
        .bind(tool_calls_json.as_deref())
        .bind(msg.tool_name.as_deref())
        .bind(msg.timestamp)
        .bind(msg.prompt_tokens)
        .bind(msg.completion_tokens)
        .bind(msg.reasoning_tokens)
        .bind(msg.cached_tokens)
        .bind(msg.cost)
        .bind(msg.finish_reason.as_deref())
        .bind(msg.reasoning.as_deref())
        .bind(seq)
        .bind(msg.kind.as_str())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    // ── 计数 ───────────────────────────────────────────────────

    /// 统计 session 的消息总数(只数普通消息,排除 compaction 边界)
    ///
    /// 用于 session 元数据 message_count 维护。走 `idx_messages_session_kind_seq` 索引
    /// （session_id + kind 等值前缀）。
    /// 排除 `kind='compaction'`——压缩边界不是用户/助手的真实对话消息。
    pub async fn count_messages(&self, session_id: &str) -> Result<i64, SessionError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND kind = 'message'",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// 统计 session 的 user 消息数(只数普通消息,排除 compaction 边界)
    ///
    /// 用于会话标题的首轮判定(user 消息数严格等于 1 ⇔ 全新会话且刚注入首条)。
    /// 走 `idx_messages_session_kind_seq` 索引(session_id + kind 等值前缀),
    /// COUNT 只返回单值,不加载消息体。
    pub async fn count_user_messages(&self, session_id: &str) -> Result<i64, SessionError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND kind = 'message' AND role = 'user'",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    // ── 查询 ───────────────────────────────────────────────────

    /// 加载全量历史(含被压缩掉的旧消息)
    ///
    /// 用途:审计、调试、导出。不参与 ReAct 循环。按 seq 升序返回全部消息
    /// (普通消息 + compaction 边界),不经过可见窗口的动态拼接逻辑。
    pub async fn load_full_history(&self, session_id: &str) -> Result<Vec<Message>, SessionError> {
        let rows = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, model_id, role, content, images, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind
             FROM messages WHERE session_id = ?1 ORDER BY seq",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// 游标分页加载历史消息(给人看的历史浏览,seq 倒序)
    ///
    /// 这是给人浏览会话历史的查询路径:打开会话先看最新一页,向上滚动加载更早消息。
    /// 与给 LLM 构造请求的可见窗口([`Self::load_visible_messages`])是正交两条路径,
    /// 互不影响。
    ///
    /// # 游标分页(不用 OFFSET)
    ///
    /// 消息是持续追加的流,OFFSET 基于「位置」分页,新消息插入会让整页内容向后漂移、
    /// 重复或遗漏。游标分页基于消息的稳定标识 `seq`(单调递增、插入后永不改、
    /// `UNIQUE(session_id, seq)`),锚点之前的内容永远固定,追加多少新消息都不影响。
    ///
    /// - `before_seq = None`:从最新一条开始(第一页)
    /// - `before_seq = Some(N)`:取 `seq < N` 的更早一页,锚点本身不含
    ///
    /// 「有没有下一页」用返回条数 == limit 判断,不提供总数。
    ///
    /// # 压缩消息处理
    ///
    /// compaction 消息(摘要)当作对话流里的一个普通节点正常显示,不过滤 `kind`——
    /// 全部消息(普通 + 压缩)按 seq 倒序一起分页。
    ///
    /// # 索引
    ///
    /// 走 `UNIQUE(session_id, seq)` 约束自带的隐式索引,`WHERE session_id=? AND seq < ?`
    /// 是索引范围扫描。`(?2 IS NULL OR seq < ?2)` 让「第一页」与「翻页」共用一条 SQL。
    pub async fn list_messages_before(
        &self,
        session_id: &str,
        before_seq: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Message>, SessionError> {
        let rows = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, model_id, role, content, images, tool_call_id,
                    tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                    reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind
             FROM messages
             WHERE session_id = ?1 AND (?2 IS NULL OR seq < ?2)
             ORDER BY seq DESC LIMIT ?3",
        )
        .bind(session_id)
        .bind(before_seq)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Message::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::super::SessionStore;
    use crate::store::compaction::CompressionReason;
    use fuyao_api::{Message, MessageKind, ToolCallData};

    /// 构造临时存储(隔离的临时目录)
    async fn temp_store() -> SessionStore {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        std::mem::forget(dir);
        SessionStore::new(db_path).await.expect("创建存储失败")
    }

    /// 插入一条 user 消息
    async fn insert_user(store: &SessionStore, sid: &str, content: &str) {
        let mut msg = Message::user(content.to_string());
        store.insert_message(sid, &mut msg).await.unwrap();
    }

    // ===== insert_message / count_messages 测试 =====

    #[tokio::test]
    async fn insert_message_assigns_sequential_seq() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        let mut m1 = Message::user("第一条".to_string());
        let seq1 = store.insert_message(&session.id, &mut m1).await.unwrap();
        assert_eq!(seq1, 1);
        assert_eq!(m1.seq, 1, "msg.seq 应被回填");

        let mut m2 = Message::assistant(Some("回复".to_string()));
        let seq2 = store.insert_message(&session.id, &mut m2).await.unwrap();
        assert_eq!(seq2, 2);
        assert_eq!(m2.seq, 2);
    }

    #[tokio::test]
    async fn insert_message_serializes_tool_calls() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        let mut msg = Message::assistant(None);
        msg.tool_calls = Some(vec![ToolCallData {
            id: "call_1".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
        }]);
        store.insert_message(&session.id, &mut msg).await.unwrap();

        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 1);
        let calls = full[0].tool_calls.as_ref().expect("tool_calls 应落库");
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments, "{}");
        // DB 列为 flat 数组形态：id / name / arguments 三字段平铺，无嵌套 function 层
        let flat = serde_json::to_string(calls).unwrap();
        assert_eq!(flat, r#"[{"id":"call_1","name":"bash","arguments":"{}"}]"#);
    }

    #[tokio::test]
    async fn count_messages_excludes_compaction_boundary() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        insert_user(&store, &session.id, "a").await;
        insert_user(&store, &session.id, "b").await;
        assert_eq!(store.count_messages(&session.id).await.unwrap(), 2);

        // 压缩边界消息(kind='compaction')不计入
        store
            .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        assert_eq!(
            store.count_messages(&session.id).await.unwrap(),
            2,
            "count_messages 应排除 compaction 边界"
        );
    }

    #[tokio::test]
    async fn count_user_messages_only_counts_user_role() {
        // 只数 role=user 的普通消息:assistant / tool 不计,compaction 边界不计
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        insert_user(&store, &session.id, "首个问题").await;
        let mut assistant = Message::assistant(Some("回复".to_string()));
        store
            .insert_message(&session.id, &mut assistant)
            .await
            .unwrap();
        let mut tool = Message::tool_result("c1".into(), "echo".into(), "结果".into());
        store.insert_message(&session.id, &mut tool).await.unwrap();
        assert_eq!(
            store.count_user_messages(&session.id).await.unwrap(),
            1,
            "只有 1 条 user 消息,assistant/tool 不计入"
        );

        insert_user(&store, &session.id, "追问").await;
        store
            .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap();
        assert_eq!(
            store.count_user_messages(&session.id).await.unwrap(),
            2,
            "compaction 边界不影响 user 计数"
        );
    }

    // ===== insert_messages_batch 测试(单事务批量落库) =====

    /// 构造一批混合消息:user(token 非零)/ assistant(四项 token + 费用)/ tool
    fn make_mixed_batch() -> Vec<Message> {
        vec![
            {
                let mut m = Message::user("问题".to_string());
                m.prompt_tokens = 12;
                m
            },
            {
                let mut m = Message::assistant(Some("回复".to_string()));
                m.prompt_tokens = 100;
                m.completion_tokens = 40;
                m.reasoning_tokens = 8;
                m.cached_tokens = 60;
                m.cost = 0.015;
                m
            },
            Message::tool_result("call_1".to_string(), "bash".to_string(), "结果".to_string()),
        ]
    }

    #[tokio::test]
    async fn insert_messages_batch_assigns_contiguous_seq_from_existing_max() {
        // 批量 seq 分配:从现有 MAX(seq)+1 起连续递增,并回填到每条 msg.seq
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        // 预置 2 条单条消息占住 seq 1、2
        insert_user(&store, &session.id, "已有1").await;
        insert_user(&store, &session.id, "已有2").await;

        let mut batch = vec![
            Message::user("批量1".to_string()),
            Message::assistant(Some("批量2".to_string())),
            Message::user("批量3".to_string()),
        ];
        store
            .insert_messages_batch(&session.id, &mut batch)
            .await
            .unwrap();

        // 回填值:3、4、5 连续
        assert_eq!(batch[0].seq, 3, "批量起点应为现有 MAX(seq)+1");
        assert_eq!(batch[1].seq, 4);
        assert_eq!(batch[2].seq, 5);
        for pair in batch.windows(2) {
            assert_eq!(pair[1].seq - pair[0].seq, 1, "seq 应连续递增");
        }

        // DB 侧:全量 seq 与单条路径的分配形态无差异
        let full = store.load_full_history(&session.id).await.unwrap();
        let seqs: Vec<i64> = full.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    }

    #[tokio::test]
    async fn insert_messages_batch_aggregates_same_as_single_path() {
        // 等价性:同一批数据,批量一次落库 vs 逐条 insert_message,
        // sessions 表各统计字段的终态必须完全一致
        let store = temp_store().await;
        let single = fuyao_api::Session::new(None, None, None);
        store.create(&single).await.unwrap();
        let batched = fuyao_api::Session::new(None, None, None);
        store.create(&batched).await.unwrap();

        for mut m in make_mixed_batch() {
            store.insert_message(&single.id, &mut m).await.unwrap();
        }
        let mut batch = make_mixed_batch();
        store
            .insert_messages_batch(&batched.id, &mut batch)
            .await
            .unwrap();

        let s = store.get(&single.id).await.unwrap().unwrap();
        let b = store.get(&batched.id).await.unwrap().unwrap();
        assert_eq!(b.message_count, s.message_count);
        assert_eq!(b.tool_call_count, s.tool_call_count);
        assert_eq!(b.total_prompt_tokens, s.total_prompt_tokens);
        assert_eq!(b.total_completion_tokens, s.total_completion_tokens);
        assert_eq!(b.total_reasoning_tokens, s.total_reasoning_tokens);
        assert_eq!(b.total_cached_tokens, s.total_cached_tokens);
        assert!(
            (b.total_cost - s.total_cost).abs() < 1e-9,
            "费用聚合应与逐条累加等价"
        );

        // 绝对值锚定(防止两条路径同错):user/assistant/tool 混合计数语义——
        // 3 条普通消息各 +1 message_count,tool 单独 +1 tool_call_count,
        // token 与费用按各自值求和
        assert_eq!(b.message_count, 3);
        assert_eq!(b.tool_call_count, 1);
        assert_eq!(b.total_prompt_tokens, 112);
        assert_eq!(b.total_completion_tokens, 40);
        assert_eq!(b.total_reasoning_tokens, 8);
        assert_eq!(b.total_cached_tokens, 60);
        assert!((b.total_cost - 0.015).abs() < 1e-9);
    }

    #[tokio::test]
    async fn insert_messages_batch_skips_compaction_kind_in_counters() {
        // 混入 compaction 边界消息:行照常落库(共享 seq 序列)但不进任何计数
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        insert_user(&store, &session.id, "预置").await;

        let mut batch = vec![
            Message::user("批量普通".to_string()),
            {
                let mut m = Message::assistant(Some("摘要载体".to_string()));
                m.kind = MessageKind::Compaction;
                m
            },
            Message::tool_result("c9".to_string(), "grep".to_string(), "命中".to_string()),
        ];
        store
            .insert_messages_batch(&session.id, &mut batch)
            .await
            .unwrap();

        // 4 行全部落库(compaction 行也在),count_messages 只数普通消息 = 3
        let full = store.load_full_history(&session.id).await.unwrap();
        assert_eq!(full.len(), 4);
        assert_eq!(store.count_messages(&session.id).await.unwrap(), 3);

        // 计数只认普通消息:预置 + 批量 user/tool 各计数,compaction 不计;tool 单独 +1
        let s = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(s.message_count, 3);
        assert_eq!(s.tool_call_count, 1);
    }

    #[tokio::test]
    async fn insert_messages_batch_empty_slice_is_noop() {
        // 空切片:no-op——不报错、不落行、统计保持初始值
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        let mut empty: Vec<Message> = Vec::new();
        store
            .insert_messages_batch(&session.id, &mut empty)
            .await
            .unwrap();

        assert_eq!(store.count_messages(&session.id).await.unwrap(), 0);
        let s = store.get(&session.id).await.unwrap().unwrap();
        assert_eq!(s.message_count, 0);
        assert_eq!(s.tool_call_count, 0);
        assert_eq!(s.total_cost, 0.0);
    }

    // ===== load_full_history 测试 =====

    #[tokio::test]
    async fn load_full_history_includes_compacted_messages() {
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        insert_user(&store, &session.id, "old").await;
        let mut m2 = Message::assistant(Some("reply".to_string()));
        store.insert_message(&session.id, &mut m2).await.unwrap();

        store
            .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap();

        let full = store.load_full_history(&session.id).await.unwrap();
        // 全量 = 2 条原始消息 + 1 条 compaction 边界
        assert_eq!(full.len(), 3);
        assert_eq!(full[2].kind, MessageKind::Compaction);
    }

    // ===== list_messages_before 测试(游标分页,seq 倒序) =====

    #[tokio::test]
    async fn list_messages_before_returns_latest_first_descending() {
        // 第一页(before_seq=None):从最新一条开始,seq 倒序
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        for content in ["m1", "m2", "m3"] {
            insert_user(&store, &session.id, content).await;
        }

        let page = store
            .list_messages_before(&session.id, None, 2)
            .await
            .unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].content.as_deref(), Some("m3"), "最新在前");
        assert_eq!(page[1].content.as_deref(), Some("m2"));
        assert!(page[0].seq > page[1].seq, "seq 倒序");
    }

    #[tokio::test]
    async fn list_messages_before_paging_forward_with_cursor() {
        // 向前翻:用上次最旧 seq 作锚点,锚点本身不含,继续取更早一页
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        for content in ["m1", "m2", "m3", "m4", "m5"] {
            insert_user(&store, &session.id, content).await;
        }

        // 第一页:m5, m4(每页 2 条)
        let page1 = store
            .list_messages_before(&session.id, None, 2)
            .await
            .unwrap();
        assert_eq!(page1[0].content.as_deref(), Some("m5"));
        assert_eq!(page1[1].content.as_deref(), Some("m4"));
        let cursor = page1[1].seq;

        // 第二页:以 m4.seq 为锚,m3, m2
        let page2 = store
            .list_messages_before(&session.id, Some(cursor), 2)
            .await
            .unwrap();
        assert_eq!(page2[0].content.as_deref(), Some("m3"));
        assert_eq!(page2[1].content.as_deref(), Some("m2"));

        // 第三页:以 m2.seq 为锚,只剩 m1
        let cursor = page2[1].seq;
        let page3 = store
            .list_messages_before(&session.id, Some(cursor), 2)
            .await
            .unwrap();
        assert_eq!(page3.len(), 1);
        assert_eq!(page3[0].content.as_deref(), Some("m1"));
    }

    #[tokio::test]
    async fn list_messages_before_returns_empty_for_empty_session() {
        // 空会话:返回空 Vec,不报错
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();

        let page = store
            .list_messages_before(&session.id, None, 50)
            .await
            .unwrap();
        assert!(page.is_empty(), "空会话返回空 Vec");
    }

    #[tokio::test]
    async fn list_messages_before_includes_compaction_in_descending_order() {
        // compaction 消息不过滤,当作对话流节点正常显示,在 seq 倒序中按 seq 就位
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        insert_user(&store, &session.id, "old1").await; // seq 1
        store
            .mark_compaction(&session.id, "摘要".to_string(), CompressionReason::Auto)
            .await
            .unwrap(); // seq 2
        insert_user(&store, &session.id, "new1").await; // seq 3

        let page = store
            .list_messages_before(&session.id, None, 50)
            .await
            .unwrap();
        // seq 倒序:new1(3), 摘要(2), old1(1),compaction 正常出现,顺序由 seq 决定
        assert_eq!(page.len(), 3);
        assert_eq!(page[0].content.as_deref(), Some("new1"));
        assert_eq!(page[1].kind, MessageKind::Compaction);
        assert_eq!(page[1].content.as_deref(), Some("摘要"));
        assert_eq!(page[2].content.as_deref(), Some("old1"));
    }

    #[tokio::test]
    async fn list_messages_before_returns_remainder_when_fewer_than_limit() {
        // 条数 < limit:返回剩余全部,不报错
        let store = temp_store().await;
        let session = fuyao_api::Session::new(None, None, None);
        store.create(&session).await.unwrap();
        insert_user(&store, &session.id, "only").await;

        let page = store
            .list_messages_before(&session.id, None, 50)
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].content.as_deref(), Some("only"));
    }
}
