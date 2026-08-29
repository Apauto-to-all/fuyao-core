//! store 层共享 SQL 片段的单点定义
//!
//! messages 表列清单、fork 复制投影、fork 聚合重算列段、可见窗口谓词——这些被
//! 多条 SQL 共同引用的片段在此集中定义，各引用点同源拼接：schema 变更只改一处，
//! 不同路径的口径物理上无法分叉。
//!
//! # 动态拼接的注入审计
//!
//! 引用点以 `format!` 将本模块片段拼进语句文本，交给 sqlx 时须经
//! `AssertSqlSafe` 显式放行。审计结论：拼接素材全部是本模块的编译期常量
//! （固定列名数组与固定表达式片段），语句中的数据一律经 `?N` 绑定参数传递，
//! 无任何外部输入进入 SQL 文本，不存在注入面。

/// messages 表全部 18 列的列名，INSERT 写入顺序
///
/// 自增主键 id 由 SQLite 引擎分配，不在清单内。列顺序是 INSERT 目标列与
/// VALUES 占位符 / SELECT 投影的对位基准，调整顺序须同步对位两侧。
pub(super) const MESSAGE_COLUMNS: [&str; 18] = [
    "session_id",
    "model_id",
    "role",
    "content",
    "images",
    "tool_call_id",
    "tool_calls",
    "tool_name",
    "timestamp",
    "prompt_tokens",
    "completion_tokens",
    "reasoning_tokens",
    "cached_tokens",
    "cost",
    "finish_reason",
    "reasoning",
    "seq",
    "kind",
];

/// 逗号连接的完整列清单：INSERT 目标列与全列投影的通用形态
pub(super) fn message_columns_sql() -> String {
    MESSAGE_COLUMNS.join(", ")
}

/// fork 复制路径的 SELECT 投影：列清单逐列映射
///
/// `session_id` 换成新会话 id 的绑定占位符（占位符编号由调用路径的绑定顺序决定），
/// `seq` 换成 `ROW_NUMBER() OVER (ORDER BY seq)` 连续重分配表达式，其余列原样
/// 搬运。投影列序与 [`MESSAGE_COLUMNS`] 一致，与 INSERT 目标列清单一一对位。
pub(super) fn fork_copy_projection_sql(new_session_placeholder: &str) -> String {
    MESSAGE_COLUMNS
        .iter()
        .map(|col| match *col {
            "session_id" => new_session_placeholder.to_string(),
            "seq" => "ROW_NUMBER() OVER (ORDER BY seq)".to_string(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// fork 聚合重算 SELECT 的公共七列：count 类两列 + 消费类五列
///
/// message_count 只数 kind='message' 的普通消息，tool_call_count 数其中
/// role='tool' 的 tool 结果；消费类（token 四项 / cost）只计普通消息各自携带的值
/// （user/tool 消息恒为 0，SUM 空集经 COALESCE 归零）。
pub(super) const FORK_RECOUNT_AGGREGATES: &str = "COUNT(*) FILTER (WHERE kind = 'message'), \
     COUNT(*) FILTER (WHERE role = 'tool'), \
     COALESCE(SUM(prompt_tokens) FILTER (WHERE kind = 'message'), 0), \
     COALESCE(SUM(completion_tokens) FILTER (WHERE kind = 'message'), 0), \
     COALESCE(SUM(reasoning_tokens) FILTER (WHERE kind = 'message'), 0), \
     COALESCE(SUM(cached_tokens) FILTER (WHERE kind = 'message'), 0), \
     COALESCE(SUM(cost) FILTER (WHERE kind = 'message'), 0.0)";

/// fork 聚合重算 UPDATE 的公共 SET 段：count 类与消费类七字段的赋值占位
///
/// 占位符以「?1 = 会话 id」为基准顺排至 ?8；各路径在其后追加自己的附加字段时
/// 须延续编号。
pub(super) const FORK_RECOUNT_SET: &str = "message_count = ?2, tool_call_count = ?3, \
     total_prompt_tokens = ?4, total_completion_tokens = ?5, \
     total_reasoning_tokens = ?6, total_cached_tokens = ?7, total_cost = ?8";

/// 可见窗口下界谓词：seq 不早于最新 compaction 边界
///
/// 无 compaction 边界时 COALESCE 退化为 0，即全量。谓词引用外层查询的 ?1 绑定
/// （目标会话 id），嵌入处的 ?1 必须正是该会话 id。
pub(super) const VISIBLE_WINDOW_PREDICATE: &str = "AND seq >= COALESCE((SELECT MAX(seq) FROM messages \
     WHERE session_id = ?1 AND kind = 'compaction'), 0)";

#[cfg(test)]
mod tests {
    use super::*;

    /// 折叠连续空白为单个空格：SQL 片段比对只看词法序列，不看排版
    fn normalize(sql: &str) -> String {
        sql.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// 列清单展开 = messages 表 18 列（INSERT 写入顺序，不含自增主键 id）
    #[test]
    fn message_columns_sql_expands_to_eighteen_columns() {
        assert_eq!(MESSAGE_COLUMNS.len(), 18);
        assert_eq!(
            normalize(&message_columns_sql()),
            normalize(
                "session_id, model_id, role, content, images, tool_call_id, tool_calls, \
                 tool_name, timestamp, prompt_tokens, completion_tokens, reasoning_tokens, \
                 cached_tokens, cost, finish_reason, reasoning, seq, kind"
            )
        );
    }

    /// fork 投影仅 session_id（换占位符）与 seq（换 ROW_NUMBER 重算）两列变形，
    /// 其余列原样、列序与列清单一致
    #[test]
    fn fork_copy_projection_rewrites_session_and_seq_only() {
        assert_eq!(
            normalize(&fork_copy_projection_sql("?3")),
            normalize(
                "?3, model_id, role, content, images, tool_call_id, tool_calls, \
                 tool_name, timestamp, prompt_tokens, completion_tokens, reasoning_tokens, \
                 cached_tokens, cost, finish_reason, reasoning, \
                 ROW_NUMBER() OVER (ORDER BY seq), kind"
            )
        );
        // 投影列数与列清单一致：18 列逗号分隔
        assert_eq!(
            fork_copy_projection_sql("?2").split(',').count(),
            MESSAGE_COLUMNS.len()
        );
    }

    /// 聚合重算公共段：count 类两列 + 消费类五列，共七个 FILTER 表达式
    #[test]
    fn fork_recount_aggregates_expand_to_seven_expressions() {
        assert_eq!(FORK_RECOUNT_AGGREGATES.matches("FILTER").count(), 7);
        assert_eq!(
            normalize(FORK_RECOUNT_AGGREGATES),
            normalize(
                "COUNT(*) FILTER (WHERE kind = 'message'), \
                 COUNT(*) FILTER (WHERE role = 'tool'), \
                 COALESCE(SUM(prompt_tokens) FILTER (WHERE kind = 'message'), 0), \
                 COALESCE(SUM(completion_tokens) FILTER (WHERE kind = 'message'), 0), \
                 COALESCE(SUM(reasoning_tokens) FILTER (WHERE kind = 'message'), 0), \
                 COALESCE(SUM(cached_tokens) FILTER (WHERE kind = 'message'), 0), \
                 COALESCE(SUM(cost) FILTER (WHERE kind = 'message'), 0.0)"
            )
        );
    }

    /// SET 公共段：七字段赋值，占位符 ?2..?8 以 ?1 = 会话 id 为基准顺排
    #[test]
    fn fork_recount_set_expands_to_seven_assignments() {
        assert_eq!(
            normalize(FORK_RECOUNT_SET),
            normalize(
                "message_count = ?2, tool_call_count = ?3, total_prompt_tokens = ?4, \
                 total_completion_tokens = ?5, total_reasoning_tokens = ?6, \
                 total_cached_tokens = ?7, total_cost = ?8"
            )
        );
    }

    /// 窗口谓词：下界 = 最新 compaction 边界 seq，无边界 COALESCE 退化全量
    #[test]
    fn visible_window_predicate_bounds_at_latest_compaction() {
        assert_eq!(
            normalize(VISIBLE_WINDOW_PREDICATE),
            normalize(
                "AND seq >= COALESCE((SELECT MAX(seq) FROM messages \
                 WHERE session_id = ?1 AND kind = 'compaction'), 0)"
            )
        );
    }
}
