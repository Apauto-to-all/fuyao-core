//! 存储层性能基准（A/B 对比，显式运行）
//!
//! 验证两类存储优化的实际收益，正常 `cargo test` 不执行（#[ignore]），
//! 需显式运行并显示输出：
//!
//! ```text
//! cargo test -p fuyao-session --test storage_perf_bench_test -- --ignored --nocapture
//! ```
//!
//! # 基准一：fork 复制路径（逐条 vs 批量）
//!
//! 同一批混合消息，一条 session 走逐条 `insert_message`（每条一个事务：
//! seq 分配 + INSERT + 统计累加），另一条走一次 `insert_messages_batch`
//! （单事务）。这是 fork 派生会话复制可见消息改动前后的真实调用形态对比。
//!
//! # 基准二：索引写放大（旧 4 索引 vs 新 2 索引）
//!
//! 两套仅索引定义不同的 DDL（表结构、连接 PRAGMA、插入数据完全一致），
//! 单事务批量插入 N 条——单事务内无 commit 噪声，耗时差即纯索引维护成本。
//! 旧 schema 含三个已删除的索引（与 UNIQUE 约束隐式索引重复的显式索引、
//! 两个无查询使用的死索引），作为基线保留在本基准中。

use fuyao_session::SessionStore;
use std::time::Instant;

/// 基准轮次：多轮取稳定区间，避免单次抖动误导结论
const ROUNDS: usize = 3;

/// 构造一批贴近真实形态的混合消息（fork 会搬运的可见消息形态）：
/// 短 user 提问 / 带工具调用的 assistant / 长工具输出（数 KB）
fn make_mixed_batch(count: usize) -> Vec<fuyao_api::Message> {
    let mut msgs = Vec::with_capacity(count);
    for i in 0..count {
        match i % 3 {
            0 => {
                let mut m =
                    fuyao_api::Message::user(format!("第 {i} 个问题：帮我看看这段代码的问题"));
                m.prompt_tokens = 50;
                msgs.push(m);
            }
            1 => {
                let mut m = fuyao_api::Message::assistant(Some(format!(
                    "第 {i} 轮分析：需要先读取文件确认结构，再定位问题根源。"
                )));
                m.tool_calls = Some(vec![fuyao_api::ToolCallData {
                    id: format!("call_{i}"),
                    name: "read_file".into(),
                    arguments: format!("{{\"path\": \"src/main_{i}.rs\"}}"),
                }]);
                m.prompt_tokens = 800;
                m.completion_tokens = 120;
                m.cost = 0.002;
                msgs.push(m);
            }
            _ => {
                let mut m = fuyao_api::Message::tool_result(
                    format!("call_{i}"),
                    "read_file".into(),
                    format!(
                        "文件内容第 {i} 份：{}",
                        "fn handler() -> Result<(), Error> { ... }\n".repeat(80)
                    ),
                );
                m.prompt_tokens = 1500;
                m.cost = 0.001;
                msgs.push(m);
            }
        }
    }
    msgs
}

/// 构造临时存储（隔离的临时目录）
async fn temp_store() -> SessionStore {
    let dir = tempfile::tempdir().expect("创建临时目录失败");
    let db_path = dir.path().join("bench.db");
    std::mem::forget(dir);
    SessionStore::new(db_path).await.expect("创建存储失败")
}

// ── 基准一：fork 复制路径（逐条 insert_message vs 一次 insert_messages_batch）──

/// 逐条路径耗时（ms）：每条一个事务，与 fork 改造前的调用形态一致
async fn bench_single_inserts(
    store: &SessionStore,
    session_id: &str,
    msgs: Vec<fuyao_api::Message>,
) -> u128 {
    let start = Instant::now();
    for mut msg in msgs {
        store
            .insert_message(session_id, &mut msg)
            .await
            .expect("逐条插入失败");
    }
    start.elapsed().as_millis()
}

/// 批量路径耗时（ms）：单事务一次搬运，与 fork 改造后的调用形态一致
async fn bench_batch_inserts(
    store: &SessionStore,
    session_id: &str,
    msgs: Vec<fuyao_api::Message>,
) -> u128 {
    let mut batch = msgs;
    let start = Instant::now();
    store
        .insert_messages_batch(session_id, &mut batch)
        .await
        .expect("批量插入失败");
    start.elapsed().as_millis()
}

#[tokio::test]
#[ignore = "性能基准：显式运行，见文件头注释"]
async fn fork_copy_single_vs_batch() {
    const N: usize = 500;
    let store = temp_store().await;

    for round in 1..=ROUNDS {
        let single_session = fuyao_api::Session::new(None, None, None);
        store.create(&single_session).await.unwrap();
        let batch_session = fuyao_api::Session::new(None, None, None);
        store.create(&batch_session).await.unwrap();

        let single_ms = bench_single_inserts(&store, &single_session.id, make_mixed_batch(N)).await;
        let batch_ms = bench_batch_inserts(&store, &batch_session.id, make_mixed_batch(N)).await;
        let speedup = single_ms as f64 / batch_ms.max(1) as f64;
        println!(
            "[fork 路径] 轮次 {round}: 逐条 ×{N} = {single_ms}ms | 批量 ×{N} = {batch_ms}ms | {speedup:.1}x"
        );
    }
}

// ── 基准二：索引写放大（单事务批量插入，唯一变量是索引数量）──

/// 建库用的连接选项：与生产 SessionStore 完全一致的 PRAGMA 集
fn bench_connect_options(db_path: &std::path::Path) -> sqlx::sqlite::SqliteConnectOptions {
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
    SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5))
        .pragma("cache_size", "-64000")
        .pragma("mmap_size", "1073741824")
}

/// 表结构（与生产 schema 相同的列定义）
const TABLES_DDL: &str = r#"
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
    parent_session_id TEXT,
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
"#;

/// 旧 schema 基线：4 个显式索引（含与 UNIQUE 隐式索引重复的一个 + 无查询使用的两个）
const OLD_INDEX_DDL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_last_active ON sessions(last_active_at DESC);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_messages_session_seq ON messages(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_messages_session_kind_seq ON messages(session_id, kind, seq);
"#;

/// 新 schema：只保留有真实查询的两个显式索引
const NEW_INDEX_DDL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_sessions_last_active ON sessions(last_active_at DESC);
CREATE INDEX IF NOT EXISTS idx_messages_session_kind_seq ON messages(session_id, kind, seq);
"#;

/// 单事务批量插入 N 条消息，返回耗时（ms）。只做 INSERT 不做统计 UPDATE，
/// 使索引维护成为事务内唯一变量。索引 DDL 传常量（raw_sql 多语句接口要求 'static）
async fn bench_index_insert(db_path: &std::path::Path, index_ddl: &'static str, n: usize) -> u128 {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(bench_connect_options(db_path))
        .await
        .expect("建库失败");
    sqlx::raw_sql(TABLES_DDL)
        .execute(&pool)
        .await
        .expect("建表失败");
    sqlx::raw_sql(index_ddl)
        .execute(&pool)
        .await
        .expect("建索引失败");
    sqlx::query("INSERT INTO sessions (id, started_at) VALUES ('s1', 0.0)")
        .execute(&pool)
        .await
        .expect("建 session 行失败");

    let content_2kb = format!(
        "工具输出：{}",
        "fn handler() -> Result<(), Error> { ... }\n".repeat(80)
    );
    let start = Instant::now();
    let mut tx = pool.begin().await.expect("开事务失败");
    for seq in 1..=(n as i64) {
        // 与生产 INSERT 同列集（基准专用副本：列集若变需同步此语句）
        sqlx::query(
            "INSERT INTO messages (session_id, model_id, role, content, images, tool_call_id,
                tool_calls, tool_name, timestamp, prompt_tokens, completion_tokens,
                reasoning_tokens, cached_tokens, cost, finish_reason, reasoning, seq, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        )
        .bind("s1")
        .bind(None::<String>)
        .bind(if seq % 3 == 0 { "tool" } else { "user" })
        .bind(content_2kb.as_str())
        .bind(None::<String>)
        .bind(None::<String>)
        .bind(None::<String>)
        .bind(None::<String>)
        .bind(0.0_f64)
        .bind(100_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0.001_f64)
        .bind(None::<String>)
        .bind(None::<String>)
        .bind(seq)
        .bind("message")
        .execute(&mut *tx)
        .await
        .expect("插入失败");
    }
    tx.commit().await.expect("提交失败");
    let elapsed = start.elapsed().as_millis();
    pool.close().await;
    elapsed
}

#[tokio::test]
#[ignore = "性能基准：显式运行，见文件头注释"]
async fn index_write_amplification_old_vs_new() {
    const N: usize = 2000;
    for round in 1..=ROUNDS {
        let dir_old = tempfile::tempdir().expect("创建临时目录失败");
        let db_old = dir_old.path().join("old.db");
        let dir_new = tempfile::tempdir().expect("创建临时目录失败");
        let db_new = dir_new.path().join("new.db");

        let old_ms = bench_index_insert(&db_old, OLD_INDEX_DDL, N).await;
        let new_ms = bench_index_insert(&db_new, NEW_INDEX_DDL, N).await;
        let saved = (old_ms as f64 - new_ms as f64) / old_ms.max(1) as f64 * 100.0;
        println!(
            "[索引写放大] 轮次 {round}: 旧 4 索引 ×{N} = {old_ms}ms | 新 2 索引 ×{N} = {new_ms}ms | 写入耗时 -{saved:.0}%"
        );
        std::mem::forget(dir_old);
        std::mem::forget(dir_new);
    }
}

// ── 基准三：读取路径（每次 LLM 调用前的可见窗口重读成本）──
//
// 真实调用形态：每个 ReAct 迭代调一次 load_visible_messages + 一次 store.get
// （system_prompt 现读）。本基准量化单次调用的绝对成本，以及三个变量：
// 纯文本 vs 带图（图片 base64 JSON 反序列化）、未压缩全量路径 vs 已压缩三段路径
// （后者读入区间全部行但只保留尾部预算内的——读放大直接可见）。

/// 构造读取路径基准会话：n 条混合消息（复用 fork 基准的消息形态，含数 KB 工具输出），
/// 可选每 k 条附 1 张 150KB 级 base64 图（模拟多模态消息的大字段）
async fn setup_read_session(store: &SessionStore, n: usize, image_every: Option<usize>) -> String {
    let session = fuyao_api::Session::new(None, None, Some("系统提示词段落。".repeat(2_000)));
    store.create(&session).await.unwrap();
    let mut batch = make_mixed_batch(n);
    if let Some(k) = image_every {
        for (i, m) in batch.iter_mut().enumerate() {
            if i % k == 0 {
                m.images = vec![fuyao_api::ImageContent {
                    mime_type: "image/jpeg".into(),
                    data: "x".repeat(150_000),
                }];
            }
        }
    }
    store
        .insert_messages_batch(&session.id, &mut batch)
        .await
        .unwrap();
    session.id
}

/// 连续调用采样，返回 (min, median, mean) 微秒；闭包返回条数（black_box 防优化）
async fn sample_calls<F, Fut>(rounds: usize, mut f: F) -> (u64, u64, u64)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = usize>,
{
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        let count = f().await;
        std::hint::black_box(count);
        samples.push(start.elapsed().as_micros() as u64);
    }
    samples.sort_unstable();
    let median = samples[rounds / 2];
    let mean = samples.iter().sum::<u64>() / rounds as u64;
    (samples[0], median, mean)
}

fn report(label: &str, stats: (u64, u64, u64), rounds: usize, extra: &str) {
    let (min, median, mean) = stats;
    println!(
        "[读取路径] {label}: min={min}µs median={median}µs mean={mean}µs（{rounds} 次）{extra}"
    );
}

#[tokio::test]
#[ignore = "性能基准：显式运行，见文件头注释"]
async fn read_path_visible_window_cost() {
    const N: usize = 400;
    const READ_ROUNDS: usize = 10;
    const NEWER: usize = 20;
    let store = temp_store().await;
    let keep_tokens = fuyao_api::get_config().session.compression.keep_tokens_max;

    // ① 纯文本 · 未压缩（全量路径）——LLM 全部都要看，读满 400 条
    let sid_text = setup_read_session(&store, N, None).await;
    let stats = sample_calls(READ_ROUNDS, || async {
        store
            .load_visible_messages(&sid_text, keep_tokens)
            .await
            .expect("加载失败")
            .len()
    })
    .await;
    report(
        &format!("纯文本·未压缩 ×{N}"),
        stats,
        READ_ROUNDS,
        "（全量路径，400 条全部进 LLM）",
    );

    // ② 纯文本 · 已压缩（三段路径）——读入 400+20 条，只保留尾部预算内 + 新消息
    store
        .mark_compaction(
            &sid_text,
            "会话摘要正文。".repeat(200),
            fuyao_session::CompressionReason::Auto,
        )
        .await
        .unwrap();
    let mut newer = make_mixed_batch(NEWER);
    store
        .insert_messages_batch(&sid_text, &mut newer)
        .await
        .unwrap();
    let stats = sample_calls(READ_ROUNDS, || async {
        store
            .load_visible_messages(&sid_text, keep_tokens)
            .await
            .expect("加载失败")
            .len()
    })
    .await;
    let total_rows = N + 1 + NEWER; // 原始 400 + 1 条摘要边界 + 20 条新消息
    report(
        &format!("纯文本·已压缩 读{total_rows}条"),
        stats,
        READ_ROUNDS,
        &format!(
            "（三段路径：读入 {total_rows} 条，仅保留尾部 {} token 预算 + 摘要 + 新消息）",
            keep_tokens
        ),
    );

    // ③ 带图 · 未压缩——每 8 条 1 张 150KB base64，量化图片反序列化成本
    let sid_img = setup_read_session(&store, N, Some(8)).await;
    let stats = sample_calls(READ_ROUNDS, || async {
        store
            .load_visible_messages(&sid_img, keep_tokens)
            .await
            .expect("加载失败")
            .len()
    })
    .await;
    report(
        &format!("带图·未压缩 ×{N}（每8条1图）"),
        stats,
        READ_ROUNDS,
        "（全量路径，50 张 150KB base64 图）",
    );

    // ④ store.get（每次 LLM 调用还伴随一次 session 整行现读，含 system_prompt）
    let stats = sample_calls(READ_ROUNDS, || async {
        store
            .get(&sid_text)
            .await
            .expect("读取失败")
            .map_or(0, |s| s.system_prompt.as_deref().map_or(0, str::len))
    })
    .await;
    report(
        "store.get 整行(含 system_prompt)",
        stats,
        READ_ROUNDS,
        "（system_prompt 约 42KB）",
    );
}
