//! SQLite DDL + 版本管理
//!
//! 当前含 sessions + messages + todos + file_snapshots 四张表。todos 表存任务列表，
//! file_snapshots 表存文件快照薄日志，两表均按 session_id 软关联会话（不加外键
//! 约束——隔离数据，session_id 仅作字符串过滤键），级联清理走显式删除。
//!
//! v2 改动（上下文压缩地基）：
//! - sessions 加 `compression_count` + `last_compacted_seq`（压缩边界元数据）
//! - messages 加 `seq`（session 内单调递增投影序号）+ `kind`（消息类型）
//! - messages 加 `UNIQUE(session_id, seq)` 约束
//! - 新增 `idx_messages_session_kind_seq` 索引（session_id+seq 查询走
//!   `UNIQUE(session_id, seq)` 约束自带的隐式索引，无需另建显式索引）
//!
//! v3 改动（会话列表排序与工作目录区分）：
//! - sessions 加 `workspace`（工作目录路径，创建时定死，供列表按项目过滤）
//! - sessions 加 `last_active_at`（最近活动时间，每次 update 刷新，供列表按最近活动倒序）
//! - 新增 `idx_sessions_last_active`（支撑按最近活动倒序的列表查询）
//!
//! v4 改动（文件回退快照账）：
//! - 新增 `file_snapshots` 表（文件快照薄日志：每个工具批执行前记一行基线树与
//!   变更文件集），列含 id 自增主键 / session_id / msg_seq / tree_hash /
//!   files（JSON 数组）/ created_at
//! - 新增 `idx_file_snapshots_session_seq`（按会话 + seq 谓词的查删路径）
//!
//! 索引设计：显式索引只为有真实查询的路径而建，每个索引可对应到具体 SQL——
//! - `idx_sessions_last_active`：list_all 的 `ORDER BY last_active_at DESC`
//! - `idx_sessions_parent`：按父定位子会话的 `WHERE parent_session_id = ?` +
//!   `ORDER BY started_at`（list_child_sessions），以及主列表排除子会话的
//!   `parent_session_id IS NULL` 过滤与 child_count 计数子查询
//! - `idx_messages_session_kind_seq`：kind 过滤类查询（消息计数 / compaction 边界定位）
//! - `idx_todos_session`：todo 列表的 `WHERE session_id` + `ORDER BY sort_order`
//! - `idx_file_snapshots_session_seq`：快照行的 `WHERE session_id` + `msg_seq`
//!   谓词路径（回退前按 `msg_seq >= target` 查行、回退 / 会话删除时删行）
//!
//! session_id+seq 类查询（全量加载 / 游标分页 / 可见窗口过滤 / seq 分配）全部走
//! `UNIQUE(session_id, seq)` 约束的隐式索引；无查询使用的列（timestamp / started_at）
//! 不建索引，也不建与约束隐式索引列完全相同的冗余显式索引——每多一个索引，
//! 每条 INSERT 都要多维护一份索引写入。
//!
//! `parent_session_id` 字段语义：**通用子任务标记**（非 fork 专属）：
//! - `NULL` = 主 session（用户对话，`create_session` / `resume_session` 产出）
//! - 非 `NULL` = 子任务 session（后台任务 / 子代理），值为父 session id
//!
//! 用途：主列表只返回顶层会话（排除子任务），子任务按父 id 定位列举。
//! 无论子任务是「全新创建」还是「fork 旧的」，只要它是子任务就带此字段。
//! `idx_sessions_parent` 支撑全部按此列的查询路径（排除过滤 / 按父列举 / 计数子查询）。

/// 当前 schema 版本（演进线见模块文档的 v2 / v3 / v4 改动段）
pub const SCHEMA_VERSION: i32 = 4;

/// 完整的 DDL 语句
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id                TEXT PRIMARY KEY,
    started_at        REAL NOT NULL,
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
    parent_session_id      TEXT,
    workspace               TEXT,
    last_active_at          REAL NOT NULL DEFAULT (unixepoch())
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

CREATE INDEX IF NOT EXISTS idx_sessions_last_active ON sessions(last_active_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_session_id, started_at);
CREATE INDEX IF NOT EXISTS idx_messages_session_kind_seq ON messages(session_id, kind, seq);

CREATE TABLE IF NOT EXISTS todos (
    id          TEXT NOT NULL,
    session_id  TEXT NOT NULL,
    content     TEXT NOT NULL,
    status      TEXT NOT NULL,
    sort_order  INTEGER NOT NULL,
    created_at  REAL NOT NULL,
    updated_at  REAL NOT NULL,
    PRIMARY KEY (session_id, id)
);

CREATE INDEX IF NOT EXISTS idx_todos_session ON todos(session_id, sort_order);

CREATE TABLE IF NOT EXISTS file_snapshots (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL,
    msg_seq     INTEGER NOT NULL,
    tree_hash   TEXT NOT NULL,
    files       TEXT NOT NULL DEFAULT '[]',
    created_at  REAL NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_file_snapshots_session_seq ON file_snapshots(session_id, msg_seq);
"#;
