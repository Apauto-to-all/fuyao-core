//! 会话管理门面：会话与消息的查询接口
//!
//! 与 [`crate::App`]（运行时交互门面）平级正交，专司「会话的检索与浏览」：
//! - [`crate::App`] 管对话的进行（create / send / recv）
//! - [`SessionManager`] 管会话的检索与浏览（列会话 / 查历史）
//!
//! 两者共享同一份 [`fuyao_session::SessionStore`]（由装配层 [`crate::start`] 注入
//! `Arc` 克隆），各取所需：Engine 写（LLM 流程落库）、SessionManager 读（查询 / 浏览）。
//! 通讯方式为直接异步方法调用——查询是纯存储读、不涉及 LLM、不需要流式产出，
//! 与 `App::create_session` 返 `SessionId` 同属「管理型同步方法」，不走消息总线。
//!
//! # 设计依据
//! 详见 `docs/开发/设计文档/01-会话查询接口设计.md`。

use std::sync::Arc;

use fuyao_session::SessionStore;

use crate::history_replay;

/// 会话管理器：持有会话存储句柄，对外提供会话 / 消息的查询接口
///
/// 与 [`App`](crate::App) 平级正交：
/// - [`App`](crate::App) 管「对话的进行」（create / send / recv）
/// - `SessionManager` 管「会话的检索与浏览」（列会话 / 查历史）
///
/// 两者共享同一份 `SessionStore`（Arc 克隆，零拷贝共享连接池）。
pub struct SessionManager {
    /// 会话存储句柄（与 Engine 共享同一份，Arc 克隆）
    store: Arc<SessionStore>,
}

impl SessionManager {
    /// 由装配层（[`crate::start`]）注入 store 句柄构造
    ///
    /// 与 Engine 共享同一份 `SessionStore`——传入的是 `Arc` 克隆，仅增引用计数、零拷贝，
    /// 两者指向同一份内存、同一个 `SqlitePool` 连接池。
    pub fn new(store: Arc<SessionStore>) -> Self {
        Self { store }
    }

    // ── 会话查询 ───────────────────────────────────────────────

    /// 列举历史会话（分页，按最近活动时间倒序）
    ///
    /// 返回的会话按 `last_active_at` 倒序——用户刚交互的会话排最前。
    ///
    /// # 参数
    /// - `workspace_filter`：传 `Some(path)` 只列该工作目录的会话；`None` 列全部（含无 workspace 的）
    /// - `limit` / `offset`：分页，单页条数与偏移量
    pub async fn list_sessions(
        &self,
        workspace_filter: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<fuyao_api::SessionPage, fuyao_session::SessionError> {
        // items 与 total 是两条独立查询（list + count），同一 workspace_filter。
        // 极端情况下两次查询之间有新会话插入会致二者差一两条——对「列表分页给 UI 算页数」
        // 是可接受的弱一致（过时的 1-2 条不影响翻页体验）。
        let items = self.store.list_all(workspace_filter, limit, offset).await?;
        let total = self.store.count_with_filter(workspace_filter).await?;
        Ok(fuyao_api::SessionPage {
            items,
            total,
            limit,
            offset,
        })
    }

    // ── 会话删除 ───────────────────────────────────────────────

    /// 删除会话（cascade 删消息 + 任务列表 + 会话行）
    ///
    /// 透传 [`SessionStore::delete`](fuyao_session::SessionStore::delete)：单事务内删
    /// todos + messages + sessions，三者要么全删要么全留。
    ///
    /// # 返回
    /// - `Ok(true)`：会话存在并已删除
    /// - `Ok(false)`：会话不存在（无 session 行被删，但该 id 的残留 todos / messages 仍被清理）
    pub async fn delete_session(
        &self,
        session_id: &str,
    ) -> Result<bool, fuyao_session::SessionError> {
        self.store.delete(session_id).await
    }

    // ── 消息查询 ───────────────────────────────────────────────

    /// 默认分页大小（每页消息条数）
    ///
    /// 向上滚动加载历史的常见档位：既不因每页过少而频繁请求，也不因过多撑爆渲染。
    /// 调用方可经 `limit` 参数覆盖此默认值。
    const DEFAULT_MESSAGE_PAGE_SIZE: i64 = 50;

    /// 游标分页加载历史消息（给人看的历史浏览，seq 倒序）
    ///
    /// 打开会话先看最新一页，向上滚动加载更早消息。与给 LLM 构造请求的可见窗口
    /// （`SessionStore::load_visible_messages`）是正交两条路径，互不影响。
    ///
    /// # 游标分页（不用 OFFSET）
    ///
    /// 消息是持续追加的流，OFFSET 基于「位置」分页，新消息插入会让整页内容向后漂移、
    /// 重复或遗漏。本接口基于消息的稳定标识 `seq`（单调递增、插入后永不改）做游标分页：
    ///
    /// - `before_seq = None`：从最新一条开始（第一页）
    /// - `before_seq = Some(N)`：取 `seq < N` 的更早一页，锚点本身不含
    ///
    /// 返回 [`MessagePage`](fuyao_api::MessagePage) 信封：`has_more` 判是否还有更早页，
    /// `next_cursor` 给翻页锚点（本页最旧消息的 seq，直接回传作下次 `before_seq`）。
    /// 游标分页不提供总数——消息是持续追加的流，total 会在新消息到达时过时、误导前端。
    ///
    /// # 参数
    ///
    /// - `session_id`：会话 ID
    /// - `before_seq`：游标锚点。`None` 取第一页（最新），`Some(N)` 向前翻（取 seq < N）
    /// - `limit`：每页条数。`None` 用 [`DEFAULT_MESSAGE_PAGE_SIZE`](Self::DEFAULT_MESSAGE_PAGE_SIZE)
    ///
    /// # 压缩消息处理
    ///
    /// compaction 消息（摘要）当作对话流里的一个普通节点正常显示，不过滤——
    /// 全部消息（普通 + 压缩）按 seq 倒序一起分页。
    pub async fn list_messages(
        &self,
        session_id: &str,
        before_seq: Option<i64>,
        limit: Option<i64>,
    ) -> Result<fuyao_api::MessagePage, fuyao_session::SessionError> {
        let limit = limit.unwrap_or(Self::DEFAULT_MESSAGE_PAGE_SIZE).max(1);
        let messages = self
            .store
            .list_messages_before(session_id, before_seq, limit)
            .await?;

        // has_more 由「本页条数 == limit」推导（满页才可能还有更多）。
        // next_cursor 取 messages（seq 倒序）末条——即本页最旧一条的 seq，作下次 before_seq。
        let has_more = messages.len() as i64 == limit;
        let next_cursor = if has_more {
            messages.last().map(|m| m.seq)
        } else {
            None
        };
        Ok(fuyao_api::MessagePage {
            items: messages,
            has_more,
            next_cursor,
        })
    }

    /// 历史消息投影成事件流（历史回放，seq 正序）
    ///
    /// 与 [`list_messages`](Self::list_messages) 同源取数（同游标、同分页），但把存储
    /// [`Message`] 投影成与实时流同构的 [`OutputEvent`]——前端历史回放与实时流共用一套
    /// 渲染逻辑，无需区分数据来源。转换由 [`history_replay`] 承担（含 tool_calls 嵌套 →
    /// 扁平的逆向、seq 倒序翻正序），详见该模块。
    ///
    /// 返回 [`EventPage`](fuyao_api::EventPage) 信封：`has_more` / `next_cursor` 直接复用
    /// [`list_messages`](Self::list_messages) 的推导（同源同游标），仅把 `items` 投影成
    /// `events`。游标分页不提供总数——消息是持续追加的流，total 会在新消息到达时过时。
    ///
    /// # 参数
    /// 同 [`list_messages`](Self::list_messages)。
    pub async fn list_events(
        &self,
        session_id: &str,
        before_seq: Option<i64>,
        limit: Option<i64>,
    ) -> Result<fuyao_api::EventPage, fuyao_session::SessionError> {
        // 复用 list_messages 的取数 + 游标推导，避免两处重复实现 limit/has_more/cursor 逻辑。
        let page = self.list_messages(session_id, before_seq, limit).await?;
        let events = history_replay::messages_to_events(page.items);
        Ok(fuyao_api::EventPage {
            events,
            has_more: page.has_more,
            next_cursor: page.next_cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::{Message, Session};

    /// 构造临时 SessionManager（隔离的临时目录 + 临时 store）
    async fn temp_manager() -> SessionManager {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db_path = dir.path().join("test.db");
        // forget 让目录留到进程结束（SessionManager 跨 await 持有 store，dir 必须存活）
        std::mem::forget(dir);
        let store = Arc::new(SessionStore::new(db_path).await.expect("创建存储失败"));
        SessionManager::new(store)
    }

    /// 创建一个带 workspace 的 session 并落库
    async fn seed_session(manager: &SessionManager, workspace: Option<&str>) -> String {
        let mut session = Session::new(workspace.map(str::to_string), None, None);
        manager.store.create_with_retry(&mut session).await.unwrap();
        session.id
    }

    /// 给 session 追加一条 user 消息并落库
    async fn seed_user_message(manager: &SessionManager, session_id: &str, content: &str) {
        let mut msg = Message::user(content.to_string());
        manager
            .store
            .insert_message(session_id, &mut msg)
            .await
            .unwrap();
    }

    // ===== SessionPage：OFFSET 分页 + total =====

    #[tokio::test]
    async fn list_sessions_returns_page_with_total_and_echo_params() {
        let manager = temp_manager().await;
        for i in 0..3 {
            seed_session(&manager, Some("/proj-a")).await;
            let _ = i;
        }
        seed_session(&manager, Some("/proj-b")).await;

        // 第一页：limit=2, offset=0 → 2 条，total=4（全部）
        let page = manager.list_sessions(None, 2, 0).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total, 4, "total 应为过滤前的全部会话数");
        assert_eq!(page.limit, 2);
        assert_eq!(page.offset, 0);
    }

    #[tokio::test]
    async fn list_sessions_total_respects_workspace_filter() {
        let manager = temp_manager().await;
        for _ in 0..2 {
            seed_session(&manager, Some("/proj-a")).await;
        }
        seed_session(&manager, Some("/proj-b")).await;
        seed_session(&manager, None).await;

        // workspace 过滤：/proj-a 有 2 条，total=2
        let page = manager.list_sessions(Some("/proj-a"), 10, 0).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.total, 2, "total 应与 workspace 过滤后的条数一致");
        // 无 workspace 的会话不在 /proj-a 过滤范围内
        assert!(
            page.items
                .iter()
                .all(|s| s.workspace.as_deref() == Some("/proj-a"))
        );
    }

    #[tokio::test]
    async fn list_sessions_offset_advances_pages() {
        let manager = temp_manager().await;
        for _ in 0..3 {
            seed_session(&manager, None).await;
        }

        let p1 = manager.list_sessions(None, 2, 0).await.unwrap();
        let p2 = manager.list_sessions(None, 2, 2).await.unwrap();
        assert_eq!(p1.items.len(), 2);
        assert_eq!(p2.items.len(), 1, "第三页只剩 1 条");
        // 两页 id 不重叠
        let p1_ids: Vec<&str> = p1.items.iter().map(|s| s.id.as_str()).collect();
        let p2_ids: Vec<&str> = p2.items.iter().map(|s| s.id.as_str()).collect();
        assert!(p1_ids.iter().all(|id| !p2_ids.contains(id)));
    }

    // ===== MessagePage：游标分页 + has_more / next_cursor =====

    #[tokio::test]
    async fn list_messages_first_page_has_more_and_cursor() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        // 插 3 条消息，limit=2 → 首页 2 条，has_more=true
        for i in 1..=3 {
            seed_user_message(&manager, &sid, &format!("msg{i}")).await;
        }

        let page = manager.list_messages(&sid, None, Some(2)).await.unwrap();
        assert_eq!(page.items.len(), 2, "首页应满 2 条");
        assert!(page.has_more, "还有更早消息，has_more 应为 true");
        // items 按 seq 倒序（新→旧）：取最新 2 条 = seq3、seq2
        assert_eq!(page.items[0].seq, 3);
        assert_eq!(page.items[1].seq, 2);
        // next_cursor = 本页最旧条 seq（末条）= 2，作下次 before_seq
        assert_eq!(page.next_cursor, Some(2));
    }

    #[tokio::test]
    async fn list_messages_next_cursor_drives_next_page() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        for i in 1..=3 {
            seed_user_message(&manager, &sid, &format!("msg{i}")).await;
        }

        // 第一页取最新 2 条（seq3, seq2），next_cursor=2
        let p1 = manager.list_messages(&sid, None, Some(2)).await.unwrap();
        assert_eq!(p1.next_cursor, Some(2));

        // 用 next_cursor 翻第二页（seq<2 → seq1）
        let p2 = manager
            .list_messages(&sid, p1.next_cursor, Some(2))
            .await
            .unwrap();
        assert_eq!(p2.items.len(), 1, "第二页只剩 seq1 一条");
        assert_eq!(p2.items[0].seq, 1);
        assert!(!p2.has_more, "到底了，has_more 应为 false");
        assert_eq!(p2.next_cursor, None);
    }

    #[tokio::test]
    async fn list_messages_last_page_has_no_more() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        for i in 1..=3 {
            seed_user_message(&manager, &sid, &format!("msg{i}")).await;
        }

        // 第一页满 2 条（seq3、seq2），has_more=true，cursor=2
        let p1 = manager.list_messages(&sid, None, Some(2)).await.unwrap();
        assert_eq!(p1.items.len(), 2);
        assert!(p1.has_more);
        assert_eq!(p1.next_cursor, Some(2));

        // 翻第二页（seq<2 → seq1，仅 1 条 < limit）→ 真正到底，has_more=false
        let p2 = manager
            .list_messages(&sid, p1.next_cursor, Some(2))
            .await
            .unwrap();
        assert_eq!(p2.items.len(), 1, "第二页只剩 seq1");
        assert!(!p2.has_more, "不足一页，确无更多");
        assert_eq!(p2.next_cursor, None);
    }

    // ===== EventPage：游标分页 + has_more / next_cursor =====

    #[tokio::test]
    async fn list_events_first_page_has_more_and_cursor() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        // 插 3 条消息，limit=2 → 首页 2 条，has_more=true
        for i in 1..=3 {
            seed_user_message(&manager, &sid, &format!("msg{i}")).await;
        }

        let page = manager.list_events(&sid, None, Some(2)).await.unwrap();
        assert_eq!(page.events.len(), 2, "首页应满 2 条");
        assert!(page.has_more, "还有更早消息，has_more 应为 true");
        // next_cursor = 本页最旧消息的 seq（首页取 seq3、seq2，最旧为 seq2）
        assert_eq!(page.next_cursor, Some(2));
    }

    #[tokio::test]
    async fn list_events_next_cursor_drives_next_page() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        for i in 1..=3 {
            seed_user_message(&manager, &sid, &format!("msg{i}")).await;
        }

        // 第一页取最新 2 条（seq3、seq2），next_cursor=2
        let p1 = manager.list_events(&sid, None, Some(2)).await.unwrap();
        assert_eq!(p1.next_cursor, Some(2));

        // 用 next_cursor 翻第二页（seq<2 → seq1）
        let p2 = manager
            .list_events(&sid, p1.next_cursor, Some(2))
            .await
            .unwrap();
        assert_eq!(p2.events.len(), 1, "第二页只剩 seq1 一条");
        assert!(!p2.has_more, "到底了，has_more 应为 false");
        assert_eq!(p2.next_cursor, None);
    }

    #[tokio::test]
    async fn list_events_last_page_has_no_more() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        for i in 1..=3 {
            seed_user_message(&manager, &sid, &format!("msg{i}")).await;
        }

        // 第一页满 2 条（seq3、seq2），has_more=true，cursor=2
        let p1 = manager.list_events(&sid, None, Some(2)).await.unwrap();
        assert_eq!(p1.events.len(), 2);
        assert!(p1.has_more);
        assert_eq!(p1.next_cursor, Some(2));

        // 翻第二页（seq<2 → seq1，仅 1 条 < limit）→ 真正到底，has_more=false
        let p2 = manager
            .list_events(&sid, p1.next_cursor, Some(2))
            .await
            .unwrap();
        assert_eq!(p2.events.len(), 1, "第二页只剩 seq1");
        assert!(!p2.has_more, "不足一页，确无更多");
        assert_eq!(p2.next_cursor, None);
    }

    #[tokio::test]
    async fn list_events_empty_session_returns_empty_page() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;

        let page = manager.list_events(&sid, None, Some(10)).await.unwrap();
        assert!(page.events.is_empty());
        assert!(!page.has_more);
        assert_eq!(page.next_cursor, None);
    }

    // ===== delete_session：透传 store.delete（cascade 删 todos + messages + sessions）=====

    #[tokio::test]
    async fn delete_session_returns_true_and_removes_session() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;
        seed_user_message(&manager, &sid, "对话").await;

        // 删除前会话存在且有消息
        let before = manager.list_messages(&sid, None, Some(10)).await.unwrap();
        assert_eq!(before.items.len(), 1);

        let deleted = manager.delete_session(&sid).await.unwrap();
        assert!(deleted, "存在的会话应返回 true");

        // 删除后会话在列表中消失
        let after = manager.list_sessions(None, 100, 0).await.unwrap();
        assert!(
            after.items.iter().all(|s| s.id != sid),
            "删除后列表不应再包含该会话"
        );
    }

    #[tokio::test]
    async fn delete_session_returns_false_for_nonexistent() {
        let manager = temp_manager().await;

        // 不存在的会话 id：返回 false（不报错）
        let deleted = manager.delete_session("nonexistent").await.unwrap();
        assert!(!deleted, "不存在的会话应返回 false");
    }

    #[tokio::test]
    async fn delete_session_cascades_todos_no_orphans() {
        let manager = temp_manager().await;
        let sid = seed_session(&manager, None).await;

        // 写一条任务（经 store 的 todo 能力，验证删除级联无残留）
        manager
            .store
            .write_todos(
                &sid,
                vec![fuyao_api::TodoItem {
                    id: "1".to_string(),
                    content: "任务".to_string(),
                    status: "pending".to_string(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(
            manager.store.read_todos(&sid).await.unwrap().len(),
            1,
            "删除前应有 1 条任务"
        );

        // 删会话 → 任务列表随之清空，无孤儿
        assert!(manager.delete_session(&sid).await.unwrap());
        assert!(
            manager.store.read_todos(&sid).await.unwrap().is_empty(),
            "删会话后任务列表应清空"
        );
    }
}
