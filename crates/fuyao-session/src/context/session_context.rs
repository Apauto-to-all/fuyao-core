//! SessionContext — 纯会话管理
//!
//! 持有消息列表 + session_id + 持久化。
//! 引擎负责构建完整 Message，此模块只做直接存储。

use crate::SessionManager;
use fuyao_api::message::OutputEvent;
use fuyao_api::message::output;
use fuyao_api::{AgentPaths, Message, Session};
use std::sync::Arc;

/// 会话上下文（内部状态）
pub struct SessionContext {
    /// 当前会话 ID
    pub(crate) session_id: Option<String>,
    /// 内存消息列表
    messages: Vec<Message>,
    /// Agent 路径配置
    agent_paths: AgentPaths,
    /// Session 管理器
    pub(crate) session_manager: Option<Arc<SessionManager>>,
    /// 模型 ID（用于消息标记）
    model_id: Option<String>,
}

impl SessionContext {
    /// 创建新的会话上下文
    pub fn new(agent_paths: AgentPaths) -> Self {
        Self {
            session_id: None,
            messages: Vec::new(),
            agent_paths,
            session_manager: None,
            model_id: None,
        }
    }

    /// 设置模型 ID
    pub fn set_model_id(&mut self, id: String) {
        self.model_id = Some(id);
    }

    /// 确保 SessionManager 已初始化（不创建 session）
    ///
    /// 仅初始化 SessionManager，用于列出/查询会话等不需要当前 session 的场景。
    pub async fn ensure_manager(&mut self) -> Result<(), crate::SessionError> {
        if self.session_manager.is_some() {
            return Ok(());
        }

        let db_path = self.agent_paths.sessions_db_path();
        let session_manager = crate::get_session_manager(db_path).await?;
        self.session_manager = Some(Arc::clone(&session_manager));

        Ok(())
    }

    /// 确保会话存在（懒初始化）
    ///
    /// 决策优先级：
    /// 1. session_id 已有 → 直接返回
    /// 2. initial_session_id 有值 → 加载已有会话
    /// 3. 都没有 → 创建全新空会话
    pub async fn ensure_session(
        &mut self,
        initial_session_id: Option<&str>,
    ) -> Result<(), crate::SessionError> {
        if self.session_id.is_some() {
            return Ok(());
        }

        // 确保 SessionManager 已初始化
        self.ensure_manager().await?;
        let mgr = self.session_manager.as_ref().unwrap();

        // 优先级2：加载已有会话
        if let Some(eid) = initial_session_id
            && let Some(session) = mgr.get(eid).await?
        {
            self.session_id = Some(session.id.clone());
            if let Some(ref system_prompt) = session.system_prompt {
                self.inject_system_message(system_prompt.clone());
            }
            self.messages.extend(session.messages);
            return Ok(());
        }

        // 优先级3：创建新 session
        let system_prompt = fuyao_prompt::build_system_prompt(&self.agent_paths);
        let session = mgr
            .create(
                None,
                if system_prompt.is_empty() {
                    None
                } else {
                    Some(system_prompt.clone())
                },
            )
            .await?;
        self.session_id = Some(session.id.clone());

        // 注入系统提示词
        if !system_prompt.is_empty() {
            self.inject_system_message(system_prompt);
        }

        Ok(())
    }

    /// 重置为无 session 状态
    ///
    /// 结束当前会话（标记 end_reason），清空消息和 session_id。
    /// 下次 ensure_session() 时会创建新 session。
    pub async fn reset_session(&mut self) -> Result<(), crate::SessionError> {
        // 结束当前会话
        if let (Some(current_id), Some(mgr)) = (&self.session_id, &self.session_manager) {
            let _ = mgr.end_session(current_id, "user_switch").await;
        }

        // 清空状态
        self.messages.clear();
        self.session_id = None;

        Ok(())
    }

    /// 将系统提示词插入消息列表头部
    fn inject_system_message(&mut self, system_prompt: String) {
        let system_msg = Message::system(system_prompt);
        // 避免重复注入（确保头部只有一个 system message）
        if !self.messages.is_empty() && self.messages[0].role == "system" {
            self.messages[0] = system_msg;
        } else {
            self.messages.insert(0, system_msg);
        }
    }

    /// 获取消息列表（给 LLM 调用用）
    pub fn get_messages(&self) -> Vec<Message> {
        self.messages.clone()
    }

    /// 输出事件到达（引擎已构建完整 Message，直接存储）
    ///
    /// 返回 true 表示实际持久化了消息（用于触发统计发射）。
    pub async fn on_output(&mut self, event: OutputEvent) -> bool {
        match event {
            OutputEvent::User(data) => self.handle_user_message(&data).await,
            OutputEvent::Assistant(data) => {
                // Assistant 仅在 LLM 调用后产生，before_llm 已确保 session 初始化
                if self.session_id.is_none() {
                    return false;
                }
                self.handle_assistant_message(data).await
            }
            OutputEvent::ToolResult(data) => {
                // ToolResult 仅在 LLM 调用后产生，before_llm 已确保 session 初始化
                if self.session_id.is_none() {
                    return false;
                }
                self.handle_tool_result(data).await
            }
            _ => false,
        }
    }

    /// 存储用户消息（通过 OutputEvent::User 触发）
    ///
    /// 用户消息是 session 创建的触发点：首次收到用户消息时自动创建 session。
    /// 这确保"发送第一条消息后才创建 session"，避免启动即创建空 session。
    async fn handle_user_message(&mut self, data: &output::UserMessage) -> bool {
        // 首次用户消息触发 session 懒初始化
        let initial_id = None; // 新建场景无初始 id
        if self.ensure_session(initial_id).await.is_err() {
            return false;
        }

        // 内存
        self.messages
            .push(Message::user(data.payload.content.clone()));

        // DB 持久化
        if let (Some(session_id), Some(mgr)) = (&self.session_id, &self.session_manager) {
            let msg = Message::user(data.payload.content.clone());
            mgr.add_message(session_id, msg, &self.agent_paths)
                .await
                .is_ok()
        } else {
            false
        }
    }

    /// 存储引擎构建的完整助手消息（cost 由 SessionManager 在存储时自动计算）
    ///
    /// 返回 true 表示持久化成功。
    async fn handle_assistant_message(&mut self, data: output::AssistantMessage) -> bool {
        // 构建 Message
        let mut msg = Message::assistant(data.payload.content.clone());
        msg.model_id = self.model_id.clone();
        msg.reasoning = data.payload.reasoning.clone();
        msg.finish_reason = data.payload.finish_reason.clone();
        msg.completion_tokens = data.payload.completion_tokens;
        msg.prompt_tokens = data.payload.prompt_tokens;
        msg.reasoning_tokens = data.payload.reasoning_tokens;
        msg.cached_tokens = data.payload.cached_tokens;

        // 工具调用
        if let Some(tool_calls) = data.payload.tool_calls {
            msg.tool_calls = Some(
                serde_json::to_value(
                    tool_calls
                        .iter()
                        .map(|tc| {
                            serde_json::json!({
                                "id": tc.tool_call_id,
                                "type": "function",
                                "function": {
                                    "name": tc.tool_name,
                                    "arguments": tc.tool_args.to_string(),
                                }
                            })
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap_or(serde_json::Value::Null),
            );
        }

        // 内存
        self.messages.push(msg.clone());

        // DB 持久化（cost 自动计算并累加到 session 级别）
        if let (Some(session_id), Some(mgr)) = (&self.session_id, &self.session_manager) {
            mgr.add_message(session_id, msg, &self.agent_paths)
                .await
                .is_ok()
        } else {
            false
        }
    }

    /// 存储工具结果消息
    ///
    /// 返回 true 表示持久化成功。
    async fn handle_tool_result(&mut self, data: output::ToolResultMessage) -> bool {
        // 构建 tool Message
        let msg = Message::tool_result(
            data.payload.tool_call_id.clone(),
            data.payload.content.clone(),
        );

        // 内存
        self.messages.push(msg.clone());

        // DB 持久化（非 assistant 消息，cost 保持 0）
        if let (Some(session_id), Some(mgr)) = (&self.session_id, &self.session_manager) {
            mgr.add_message(session_id, msg, &self.agent_paths)
                .await
                .is_ok()
        } else {
            false
        }
    }

    /// 切换到指定会话
    ///
    /// 加载目标会话的消息历史。
    /// 仅在 Idle 状态下调用，避免与 Engine 处理冲突。
    pub async fn switch_session(
        &mut self,
        session_id: String,
    ) -> Result<Session, crate::SessionError> {
        let mgr = self
            .session_manager
            .as_ref()
            .ok_or_else(|| crate::SessionError::InvalidState("SessionManager 未初始化".into()))?;

        let session = mgr
            .get(&session_id)
            .await?
            .ok_or_else(|| crate::SessionError::NotFound(session_id.clone()))?;

        // 清空当前消息
        self.messages.clear();
        self.session_id = Some(session.id.clone());

        // 注入系统提示词
        if let Some(ref system_prompt) = session.system_prompt {
            self.inject_system_message(system_prompt.clone());
        }

        // 加载历史消息
        self.messages.extend(session.messages.clone());

        Ok(session)
    }

    /// 创建新会话
    ///
    /// 结束当前会话，创建新会话并切换过去。
    pub async fn new_session(&mut self) -> Result<String, crate::SessionError> {
        let mgr = self
            .session_manager
            .as_ref()
            .ok_or_else(|| crate::SessionError::InvalidState("SessionManager 未初始化".into()))?;

        // 结束当前会话
        if let Some(ref current_id) = self.session_id {
            let _ = mgr.end_session(current_id, "user_switch").await;
        }

        // 构建系统提示词
        let system_prompt = fuyao_prompt::build_system_prompt(&self.agent_paths);

        // 创建新 session
        let session = mgr
            .create(
                None,
                if system_prompt.is_empty() {
                    None
                } else {
                    Some(system_prompt.clone())
                },
            )
            .await?;

        // 清空当前消息
        self.messages.clear();
        self.session_id = Some(session.id.clone());

        // 注入系统提示词
        if !system_prompt.is_empty() {
            self.inject_system_message(system_prompt);
        }

        Ok(session.id)
    }

    /// 列出历史会话（分页）
    pub async fn list_sessions(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Session>, crate::SessionError> {
        let mgr = self
            .session_manager
            .as_ref()
            .ok_or_else(|| crate::SessionError::InvalidState("SessionManager 未初始化".into()))?;
        mgr.list(limit, offset).await
    }

    /// 获取会话总数
    pub async fn count_sessions(&self) -> Result<i64, crate::SessionError> {
        let mgr = self
            .session_manager
            .as_ref()
            .ok_or_else(|| crate::SessionError::InvalidState("SessionManager 未初始化".into()))?;
        mgr.count().await
    }

    /// 获取当前 session_id（借用）
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// 获取当前 session_id（owned，编排层回写用）
    pub fn session_id_cloned(&self) -> Option<String> {
        self.session_id.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::AgentPaths;

    #[test]
    fn session_context_new() {
        let ctx = SessionContext::new(AgentPaths::default());
        assert!(ctx.session_id.is_none());
        assert!(ctx.messages.is_empty());
    }
}
