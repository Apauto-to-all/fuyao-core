//! SQLite DDL + 版本管理

/// 当前 schema 版本
pub const SCHEMA_VERSION: i32 = 1;

/// 完整的 DDL 语句
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id                TEXT PRIMARY KEY,
    parent_session_id TEXT REFERENCES sessions(id),
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
    system_prompt     TEXT
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
    reasoning             TEXT
);

CREATE TABLE IF NOT EXISTS todos (
    id          TEXT NOT NULL,
    session_id  TEXT NOT NULL REFERENCES sessions(id),
    content     TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending',
    sort_order  INTEGER NOT NULL DEFAULT 0,
    created_at  REAL NOT NULL,
    updated_at  REAL NOT NULL,
    PRIMARY KEY (id, session_id)
);

CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_session_id);
CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_todos_session ON todos(session_id);
"#;
