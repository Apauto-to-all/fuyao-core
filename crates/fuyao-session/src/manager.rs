//! 会话管理器（高层 API）
//!
//! 封装 SQLiteStore，提供缓存和便捷方法。

use crate::cost::calculate_cost;
use crate::error::SessionError;
use crate::store::SQLiteStore;
use crate::todo_store::TodoStore;
use fuyao_api::{AgentPaths, Message, Session};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 全局 SessionManager 工厂缓存
static SESSION_MANAGER_CACHE: std::sync::LazyLock<Mutex<HashMap<String, Arc<SessionManager>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// 获取或创建 SessionManager（按 db_path 缓存）
pub async fn get_session_manager(db_path: PathBuf) -> Result<Arc<SessionManager>, SessionError> {
    let key = db_path.to_string_lossy().to_string();

    // 先尝试读缓存（块作用域确保释放锁后再 await，避免持锁跨 await 点）
    {
        let mgr = SESSION_MANAGER_CACHE
            .lock()
            .map_err(|e| SessionError::InvalidState(format!("缓存锁中毒: {e}")))?
            .get(&key)
            .map(Arc::clone);
        if let Some(mgr) = mgr {
            return Ok(mgr);
        }
    }

    let store = SQLiteStore::new(db_path).await?;
    let mgr = Arc::new(SessionManager::new(store));
    SESSION_MANAGER_CACHE
        .lock()
        .map_err(|e| SessionError::InvalidState(format!("缓存锁中毒: {e}")))?
        .insert(key, Arc::clone(&mgr));
    Ok(mgr)
}

/// 清空全局缓存
pub fn clear_session_manager_cache() {
    if let Ok(mut cache) = SESSION_MANAGER_CACHE.lock() {
        cache.clear();
    }
}

/// 会话管理器
pub struct SessionManager {
    store: SQLiteStore,
    cached_session_id: Mutex<Option<String>>,
    cached_session: Mutex<Option<Session>>,
}

impl SessionManager {
    pub fn new(store: SQLiteStore) -> Self {
        Self {
            store,
            cached_session_id: Mutex::new(None),
            cached_session: Mutex::new(None),
        }
    }

    /// 创建新会话
    pub async fn create(
        &self,
        title: Option<String>,
        system_prompt: Option<String>,
    ) -> Result<Session, SessionError> {
        let session = Session::new(title, system_prompt);
        self.store.create(&session).await?;
        self.update_cache(&session);
        tracing::info!(session_id = %session.id, "会话创建");
        Ok(session)
    }

    /// 创建新会话（指定 ID）
    pub async fn create_with_id(
        &self,
        session_id: String,
        title: Option<String>,
        system_prompt: Option<String>,
        parent_session_id: Option<String>,
    ) -> Result<Session, SessionError> {
        let mut session = Session::new(title, system_prompt);
        session.id = session_id;
        session.parent_session_id = parent_session_id;
        self.store.create(&session).await?;
        self.update_cache(&session);
        tracing::info!(session_id = %session.id, "会话创建");
        Ok(session)
    }

    /// 获取会话（缓存优先）
    pub async fn get(&self, session_id: &str) -> Result<Option<Session>, SessionError> {
        {
            let cached_id = self
                .cached_session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref id) = *cached_id
                && id == session_id
            {
                let cached = self
                    .cached_session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if cached.is_some() {
                    return Ok(cached.clone());
                }
            }
        }

        let session = self.store.get(session_id).await?;
        if let Some(ref s) = session {
            self.update_cache(s);
        }
        Ok(session)
    }

    /// 保存会话
    pub async fn save(&self, session: &Session) -> Result<(), SessionError> {
        self.store.update(session).await?;
        self.update_cache(session);
        Ok(())
    }

    /// 更新会话标题（轻量，只改 title 列 + 同步缓存）
    pub async fn set_session_title(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<bool, SessionError> {
        let updated = self.store.set_session_title(session_id, title).await?;
        if updated {
            // 同步缓存（若命中的是该 session）
            let cached_id = self
                .cached_session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if cached_id.as_deref() == Some(session_id) {
                let mut cached = self
                    .cached_session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(ref mut s) = *cached {
                    s.title = Some(title.to_string());
                }
            }
        }
        Ok(updated)
    }

    /// 删除会话
    pub async fn delete(&self, session_id: &str) -> Result<bool, SessionError> {
        let deleted = self.store.delete(session_id).await?;
        if deleted {
            let mut cached_id = self
                .cached_session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if cached_id.as_deref() == Some(session_id) {
                *cached_id = None;
                let mut cached = self
                    .cached_session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *cached = None;
            }
        }
        Ok(deleted)
    }

    /// 列出会话（分页）
    pub async fn list(&self, limit: i64, offset: i64) -> Result<Vec<Session>, SessionError> {
        self.store.list_all(limit, offset).await
    }

    /// 会话总数
    pub async fn count(&self) -> Result<i64, SessionError> {
        self.store.count().await
    }

    /// 追加消息（自动计算 assistant 消息的 cost，并累加 session 级别统计）
    pub async fn add_message(
        &self,
        session_id: &str,
        mut message: Message,
        agent_paths: &AgentPaths,
    ) -> Result<Option<Session>, SessionError> {
        let mut session = match self.get(session_id).await? {
            Some(s) => s,
            None => return Ok(None),
        };

        // 自动计算 assistant 消息的费用
        if message.role == "assistant"
            && let Some(ref model_id) = message.model_id
        {
            let cost_decimal = calculate_cost(
                model_id,
                message.prompt_tokens,
                message.completion_tokens,
                message.reasoning_tokens,
                message.cached_tokens,
                agent_paths,
            );
            message.cost = cost_decimal.to_f64().unwrap_or(0.0);
        }

        // 累加 session 级别统计
        session.total_prompt_tokens += message.prompt_tokens;
        session.total_completion_tokens += message.completion_tokens;
        session.total_reasoning_tokens += message.reasoning_tokens;
        session.total_cached_tokens += message.cached_tokens;
        // 使用 Decimal 精确累加后转 f64
        let total_cost_decimal = Decimal::from_f64(session.total_cost).unwrap_or(Decimal::ZERO)
            + Decimal::from_f64(message.cost).unwrap_or(Decimal::ZERO);
        session.total_cost = total_cost_decimal.to_f64().unwrap_or(0.0);

        if message.role == "tool" {
            session.tool_call_count += 1;
        }
        session.message_count += 1;
        session.messages.push(message);
        self.save(&session).await?;
        Ok(Some(session))
    }

    /// 获取消息历史
    pub async fn get_messages(&self, session_id: &str) -> Result<Vec<Message>, SessionError> {
        let session = self.get(session_id).await?;
        Ok(session.map(|s| s.messages).unwrap_or_default())
    }

    /// 结束会话
    pub async fn end_session(
        &self,
        session_id: &str,
        end_reason: &str,
    ) -> Result<Option<Session>, SessionError> {
        let mut session = match self.get(session_id).await? {
            Some(s) => s,
            None => return Ok(None),
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        session.ended_at = Some(now);
        session.end_reason = Some(end_reason.to_string());
        self.save(&session).await?;
        tracing::info!(session_id = %session_id, end_reason = %end_reason, "会话结束");
        Ok(Some(session))
    }

    /// 压缩分裂：结束旧会话 → 创建新会话 → 写入压缩消息
    pub async fn split_session(
        &self,
        old_session_id: &str,
        new_system_prompt: String,
        compressed_messages: Vec<Message>,
        title: Option<String>,
    ) -> Result<Session, SessionError> {
        // 结束旧会话
        self.end_session(old_session_id, "compression").await?;

        // 创建新会话
        let mut new_session = Session::new(title, Some(new_system_prompt));
        new_session.parent_session_id = Some(old_session_id.to_string());
        new_session.messages = compressed_messages;
        new_session.message_count = new_session.messages.len() as i64;

        // 自动从消息中累加 token 和 cost 到 session 级别
        let mut total_cost_decimal = Decimal::ZERO;
        for msg in &new_session.messages {
            new_session.total_prompt_tokens += msg.prompt_tokens;
            new_session.total_completion_tokens += msg.completion_tokens;
            new_session.total_reasoning_tokens += msg.reasoning_tokens;
            new_session.total_cached_tokens += msg.cached_tokens;
            total_cost_decimal += Decimal::from_f64(msg.cost).unwrap_or(Decimal::ZERO);
        }
        new_session.total_cost = total_cost_decimal.to_f64().unwrap_or(0.0);

        self.store.create(&new_session).await?;
        self.update_cache(&new_session);
        Ok(new_session)
    }

    /// Fork 源 session 的历史消息到目标 session
    ///
    /// 将源 session 的所有消息和统计字段完整复制到目标 session（纯 DB 操作），
    /// 目标 session 的 system_prompt 保持创建时的值不变。用于编排层节点间上下文继承。
    pub async fn fork_messages(
        &self,
        source_session_id: &str,
        target_session_id: &str,
    ) -> Result<Session, SessionError> {
        let source = self
            .get(source_session_id)
            .await?
            .ok_or_else(|| SessionError::NotFound(source_session_id.to_string()))?;

        let mut target = self
            .get(target_session_id)
            .await?
            .ok_or_else(|| SessionError::NotFound(target_session_id.to_string()))?;

        // 全量复制消息（不过滤）
        target.messages = source.messages;

        // 直接复制统计字段
        target.message_count = source.message_count;
        target.tool_call_count = source.tool_call_count;
        target.total_prompt_tokens = source.total_prompt_tokens;
        target.total_completion_tokens = source.total_completion_tokens;
        target.total_reasoning_tokens = source.total_reasoning_tokens;
        target.total_cached_tokens = source.total_cached_tokens;
        target.total_cost = source.total_cost;

        // 记录 fork 来源
        target.parent_session_id = Some(source_session_id.to_string());

        self.save(&target).await?;
        Ok(target)
    }

    /// 获取 TodoStore（共享底层连接池）
    pub fn get_todo_store(&self) -> TodoStore {
        TodoStore::new(self.store.pool().clone())
    }

    fn update_cache(&self, session: &Session) {
        if let Ok(mut id) = self.cached_session_id.lock() {
            *id = Some(session.id.clone());
        }
        if let Ok(mut cached) = self.cached_session.lock() {
            *cached = Some(session.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::Message;

    async fn temp_manager() -> Arc<SessionManager> {
        let dir = std::env::temp_dir()
            .join("fuyao_mgr_test")
            .join(uuid::Uuid::new_v4().to_string());
        let store = SQLiteStore::new(dir.join("test.db")).await.unwrap();
        Arc::new(SessionManager::new(store))
    }

    #[tokio::test]
    async fn manager_create_and_get() {
        let mgr = temp_manager().await;
        let session = mgr.create(Some("测试".to_string()), None).await.unwrap();
        assert_eq!(session.title, Some("测试".to_string()));

        let loaded = mgr.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, session.id);
    }

    #[tokio::test]
    async fn manager_get_returns_none_for_missing() {
        let mgr = temp_manager().await;
        assert!(mgr.get("nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn manager_add_message() {
        let mgr = temp_manager().await;
        let session = mgr.create(None, None).await.unwrap();

        let agent_paths = AgentPaths::default();
        let result = mgr
            .add_message(&session.id, Message::user("你好".to_string()), &agent_paths)
            .await
            .unwrap();
        assert!(result.is_some());
        let s = result.unwrap();
        assert_eq!(s.message_count, 1);
        assert_eq!(s.messages.len(), 1);
    }

    #[tokio::test]
    async fn manager_add_message_accumulates_tokens_and_cost() {
        let mgr = temp_manager().await;
        let agent_paths = AgentPaths::default();
        let session = mgr.create(None, None).await.unwrap();

        // 添加 assistant 消息，自动计算 cost 并累加 session 统计
        let mut msg = Message::assistant(Some("回复".to_string()));
        msg.model_id = Some("test/model".to_string());
        msg.prompt_tokens = 100;
        msg.completion_tokens = 50;
        msg.reasoning_tokens = 10;
        msg.cached_tokens = 20;
        mgr.add_message(&session.id, msg, &agent_paths)
            .await
            .unwrap();

        let loaded = mgr.get(&session.id).await.unwrap().unwrap();
        assert_eq!(loaded.total_prompt_tokens, 100);
        assert_eq!(loaded.total_completion_tokens, 50);
        assert_eq!(loaded.total_reasoning_tokens, 10);
        assert_eq!(loaded.total_cached_tokens, 20);
        // cost 应已自动计算（虽然 test/model 找不到配置，返回 0.0）
        assert_eq!(loaded.total_cost, 0.0);
        assert_eq!(loaded.messages[0].cost, 0.0);
    }

    #[tokio::test]
    async fn manager_end_session() {
        let mgr = temp_manager().await;
        let session = mgr.create(None, None).await.unwrap();

        let result = mgr.end_session(&session.id, "completed").await.unwrap();
        assert!(result.is_some());
        let s = result.unwrap();
        assert_eq!(s.end_reason, Some("completed".to_string()));
        assert!(s.ended_at.is_some());
    }

    #[tokio::test]
    async fn manager_split_session() {
        let mgr = temp_manager().await;
        let agent_paths = AgentPaths::default();
        let session = mgr.create(None, None).await.unwrap();
        let session_id = session.id.clone();
        mgr.add_message(&session_id, Message::user("你好".to_string()), &agent_paths)
            .await
            .unwrap();

        let new_session = mgr
            .split_session(
                &session_id,
                "新系统提示".to_string(),
                vec![Message::system("压缩摘要".to_string())],
                Some("压缩会话".to_string()),
            )
            .await
            .unwrap();

        assert_eq!(new_session.parent_session_id, Some(session_id.clone()));
        assert_eq!(new_session.system_prompt, Some("新系统提示".to_string()));
        assert_eq!(new_session.messages.len(), 1);
        assert_eq!(new_session.title, Some("压缩会话".to_string()));

        // 旧会话应已结束
        let old = mgr.get(&session_id).await.unwrap().unwrap();
        assert_eq!(old.end_reason, Some("compression".to_string()));
    }

    #[tokio::test]
    async fn manager_delete_clears_cache() {
        let mgr = temp_manager().await;
        let session = mgr.create(None, None).await.unwrap();
        let sid = session.id.clone();

        mgr.delete(&sid).await.unwrap();
        assert!(mgr.get(&sid).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn manager_list_and_count() {
        let mgr = temp_manager().await;
        mgr.create(Some("s1".to_string()), None).await.unwrap();
        mgr.create(Some("s2".to_string()), None).await.unwrap();

        assert_eq!(mgr.count().await.unwrap(), 2);
        let list = mgr.list(10, 0).await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn manager_get_messages() {
        let mgr = temp_manager().await;
        let agent_paths = AgentPaths::default();
        let session = mgr.create(None, None).await.unwrap();
        mgr.add_message(&session.id, Message::user("msg1".to_string()), &agent_paths)
            .await
            .unwrap();
        mgr.add_message(
            &session.id,
            Message::assistant(Some("msg2".to_string())),
            &agent_paths,
        )
        .await
        .unwrap();

        let messages = mgr.get_messages(&session.id).await.unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
    }

    #[tokio::test]
    async fn manager_fork_messages_copies_all() {
        let mgr = temp_manager().await;
        let agent_paths = AgentPaths::default();

        // 创建源 session，含 user + assistant 消息
        let source = mgr.create(None, None).await.unwrap();
        mgr.add_message(&source.id, Message::user("你好".to_string()), &agent_paths)
            .await
            .unwrap();
        let mut assistant_msg = Message::assistant(Some("回复".to_string()));
        assistant_msg.model_id = Some("test/model".to_string());
        assistant_msg.prompt_tokens = 100;
        assistant_msg.completion_tokens = 50;
        mgr.add_message(&source.id, assistant_msg, &agent_paths)
            .await
            .unwrap();

        // 创建目标 session（有自己的 system prompt）
        let target = mgr
            .create(None, Some("新系统提示".to_string()))
            .await
            .unwrap();

        // Fork
        let result = mgr.fork_messages(&source.id, &target.id).await.unwrap();

        // 全量复制消息
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.messages[0].role, "user");
        assert_eq!(result.messages[1].role, "assistant");

        // 统计字段直接从源 session 复制
        assert_eq!(result.total_prompt_tokens, 100);
        assert_eq!(result.total_completion_tokens, 50);

        // parent_session_id 记录 fork 来源
        assert_eq!(result.parent_session_id, Some(source.id.clone()));

        // 源 session 不受影响
        let source_loaded = mgr.get(&source.id).await.unwrap().unwrap();
        assert_eq!(source_loaded.messages.len(), 2);
    }

    #[tokio::test]
    async fn manager_fork_messages_source_not_found() {
        let mgr = temp_manager().await;
        let target = mgr.create(None, None).await.unwrap();

        let result = mgr.fork_messages("nonexistent", &target.id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn manager_fork_messages_target_not_found() {
        let mgr = temp_manager().await;
        let source = mgr.create(None, None).await.unwrap();

        let result = mgr.fork_messages(&source.id, "nonexistent").await;
        assert!(result.is_err());
    }
}
