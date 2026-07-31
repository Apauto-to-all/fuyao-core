//! SQLite DDL + 版本管理
//!
//! 当前含 sessions + messages 两张表。
//! todos 表属工具层（fuyao-tools）职责，后续由该 crate 自带 schema，不在此处维护。
//!
//! v2 改动（上下文压缩地基）：
//! - sessions 加 `compression_count` + `last_compacted_seq`（压缩边界元数据）
//! - messages 加 `seq`（session 内单调递增投影序号）+ `kind`（消息类型）
//! - messages 加 `UNIQUE(session_id, seq)` 约束
//! - 新增 `idx_messages_session_seq` + `idx_messages_session_kind_seq` 索引
//!
//! `parent_session_id` 字段历史：
//! - 最初为「链式分裂压缩方案」引入，后随方案废弃而删除（一并删除 `idx_sessions_parent`）
//! - 现重新加回，语义为**通用子任务标记**（非 fork 专属）：
//!   - `NULL` = 主 session（用户对话，`create_session` / `resume_session` 产出）
//!   - 非 `NULL` = 子任务 session（后台任务 / 子代理），值为父 session id
//! - 用途：前端区分主对话 vs 子任务，按父 id 分组/过滤
//! - 与链式分裂完全无关。无论子任务是「全新创建」还是「fork 旧的」，只要它是子任务就带此字段。
//!   当前不建派生索引——级联查找（如「列出某父 session 的所有子任务」）属未来调度层职责，到时再加。

/// 当前 schema 版本（开发阶段 db 每次重建，保持 1）
pub const SCHEMA_VERSION: i32 = 1;

/// 完整的 DDL 语句
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id                TEXT PRIMARY KEY,
    started_at        REAL NOT NULL,
    ended_at          REAL,
    end_reason        TEXT,
    message_count     INTEGER DEFAULT 0,
    tool_call_count   INTEGER DEFAULT 0,
    total_prompt_tokens      INTEGER DEFAULT 0,
    total_completion_tokens  INTEGER DEFAULT 0,
    total_reasoning_tokens   INTEGER DEFAULT 0,
    total_cached_tokens      INTEGER DEFAULT 0,
    total_cost        REAL DEFAULT 0,
    title             TEXT,
    system_prompt     TEXT,
    compression_count      INTEGER NOT NULL DEFAULT 0,
    last_compacted_seq     INTEGER,
    parent_session_id      TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id            TEXT NOT NULL REFERENCES sessions(id),
    model_id              TEXT,
    role                  TEXT NOT NULL,
    content               TEXT,
    images                TEXT,
    tool_call_id          TEXT,
    tool_calls            TEXT,
    tool_name             TEXT,
    timestamp             REAL NOT NULL,
    prompt_tokens         INTEGER DEFAULT 0,
    completion_tokens     INTEGER DEFAULT 0,
    reasoning_tokens      INTEGER DEFAULT 0,
    cached_tokens         INTEGER DEFAULT 0,
    cost                  REAL DEFAULT 0,
    finish_reason         TEXT,
    reasoning             TEXT,
    seq                   INTEGER NOT NULL,
    kind                  TEXT NOT NULL DEFAULT 'message',
    UNIQUE(session_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_messages_session_seq ON messages(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_messages_session_kind_seq ON messages(session_id, kind, seq);
"#;
