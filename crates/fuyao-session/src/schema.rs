//! SQLite DDL + 版本管理
//!
//! 当前含 sessions + messages 两张表。
//! todos 表属工具层（fuyao-tools）职责，后续由该 crate 自带 schema，不在此处维护。
//!
//! v2 改动（上下文压缩地基）：
//! - sessions 删 `parent_session_id`（链式分裂方案废弃）
//! - sessions 加 `compression_count` + `last_compacted_seq`（压缩边界元数据）
//! - messages 加 `seq`（session 内单调递增投影序号）+ `kind`（消息类型）
//! - messages 加 `UNIQUE(session_id, seq)` 约束
//! - 新增 `idx_messages_session_seq` + `idx_messages_session_kind_seq` 索引
//! - 删除 `idx_sessions_parent` 索引（随字段删除）

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
    last_compacted_seq     INTEGER
);

CREATE TABLE IF NOT EXISTS messages (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id            TEXT NOT NULL REFERENCES sessions(id),
    model_id              TEXT,
    role                  TEXT NOT NULL,
    content               TEXT,
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
