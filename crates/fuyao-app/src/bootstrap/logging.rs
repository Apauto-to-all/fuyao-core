//! 日志初始化 —— tracing subscriber 装配
//!
//! 在 `init_engine` 加载配置后调用一次，构建「文件 + stderr」双层 subscriber。
//! 文件层失败时（目录不可写等）自动降级为纯 stderr，不阻断引擎启动。
//!
//! 日志地基仅负责 subscriber 装配与 guard 生命周期；具体日志点位（哪些地方记录）
//! 见日志模块实现计划步骤 6（开发内部文档），后续单独细化。

use std::io;

use fuyao_api::{AgentPaths, LogRotation, LoggingConfig};
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::RollingFileAppender;
use tracing_subscriber::{
    EnvFilter, Layer, Registry, fmt, fmt::time::ChronoLocal, layer::SubscriberExt,
};

/// core 引擎专属日志文件名前缀（滚动 appender 用）
///
/// 与应用壳 fuyao-code 的 `fuyao-code` 前缀对称：core 独立运行/测试时用本前缀，
/// 集成到 fuyao-code 时全局 subscriber 由 code 接管，日志统一落 `fuyao-code.log`。
const LOG_FILE_PREFIX: &str = "fuyao-core";

/// 日志 guard —— 持有 non-blocking 文件写入器的工作线程句柄
///
/// drop 时 flush 缓冲区，保证进程退出前日志全部落盘。必须存活到引擎结束。
/// 存放于 [`crate::AppContext::log_guard`]，随 `AppContext` 一起 drop。
///
/// 文件层初始化失败（降级为纯 stderr）时 `file_guard` 为 `None`，guard 仍可安全持有/丢弃。
#[derive(Default)]
pub struct LogGuard {
    /// 文件 non-blocking writer 的工作 guard；stderr 层同步写入无需 guard。
    ///
    /// 仅靠 drop 副作用（flush 缓冲）生效，从不直接读取。
    #[allow(dead_code)]
    file_guard: Option<WorkerGuard>,
}

/// 初始化日志 —— 构建文件 + stderr 双层 subscriber 并设为全局默认
///
/// **全局一次性**：`set_global_default` 全进程只能成功一次，重复调用静默忽略
/// （返回的 `LogGuard` 仍有效，但 subscriber 不变）。
///
/// - 文件层：写入 `agent_paths.logs_dir()`，按 `rotation` 滚动，非阻塞（不阻塞引擎）
/// - stderr 层：受 `config.console` 开关，彩色
/// - 级别过滤：`EnvFilter`，`RUST_LOG` 优先，否则用 `config.level`（每层各自挂同一 filter）
///
/// 文件层失败（目录创建/打开失败）时打印一行 stderr 提示并降级为纯 stderr，不返回错误：
/// 日志是辅助设施，绝不应让引擎启动失败。
pub fn init_logging(config: &LoggingConfig, agent_paths: &AgentPaths) -> LogGuard {
    // RUST_LOG 优先；未设或解析失败则用配置的 level
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));

    // 本地时区计时器：用用户当地时区显示日志时间（替代默认的 UTC SystemTime）
    let timer = ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f".to_string());

    let mut file_guard: Option<WorkerGuard> = None;
    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();

    // 文件层（失败降级为纯 stderr）
    match make_file_writer(agent_paths, config.rotation) {
        Ok((writer, guard)) => {
            file_guard = Some(guard);
            layers.push(Box::new(
                fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false) // 文件不要 ANSI 颜色码
                    .with_timer(timer.clone())
                    .with_filter(filter.clone()),
            ));
        }
        Err(e) => {
            // subscriber 尚未就绪，用 stderr 直接提示
            eprintln!("[fuyao] 日志文件初始化失败，仅输出到 stderr: {e}");
        }
    }

    // stderr 层
    if config.console {
        layers.push(Box::new(
            fmt::layer()
                .with_writer(io::stderr)
                .with_ansi(true)
                .with_timer(timer)
                .with_filter(filter),
        ));
    }

    let subscriber = Registry::default().with(layers);
    let _ = tracing::subscriber::set_global_default(subscriber);

    LogGuard { file_guard }
}

/// 构建文件 non-blocking writer 及其 guard
///
/// 按需创建 `logs_dir` 目录，构建滚动 appender，包装为 non-blocking。
fn make_file_writer(
    agent_paths: &AgentPaths,
    rotation: LogRotation,
) -> io::Result<(NonBlocking, WorkerGuard)> {
    let dir = agent_paths.logs_dir();
    std::fs::create_dir_all(&dir)?;

    let appender = RollingFileAppender::builder()
        .rotation(to_appender_rotation(rotation))
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix("log")
        .build(&dir)
        .map_err(io::Error::other)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    Ok((writer, guard))
}

/// 配置轮转枚举 → tracing-appender 轮转枚举
fn to_appender_rotation(rotation: LogRotation) -> tracing_appender::rolling::Rotation {
    match rotation {
        LogRotation::Daily => tracing_appender::rolling::Rotation::DAILY,
        LogRotation::Hourly => tracing_appender::rolling::Rotation::HOURLY,
        LogRotation::Never => tracing_appender::rolling::Rotation::NEVER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_api::AgentPaths;
    use std::path::PathBuf;

    /// make_file_writer 在临时目录应创建 logs 子目录并返回可用 writer/guard。
    #[test]
    fn make_file_writer_creates_dir_and_returns_guard() {
        let temp = std::env::temp_dir().join("fuyao_test_logging_make_writer");
        // 清理可能的残留
        let _ = std::fs::remove_dir_all(&temp);

        let paths = AgentPaths {
            fuyao_home: temp.clone(),
            ..Default::default()
        };
        let (writer, guard) = make_file_writer(&paths, LogRotation::Never).unwrap();
        // logs 目录已创建
        assert!(temp.join("logs").is_dir(), "logs 目录应被创建");

        // guard drop 不 panic（flush non-blocking 缓冲）
        drop(writer);
        drop(guard);

        std::fs::remove_dir_all(&temp).ok();
    }

    /// to_appender_rotation 各变体映射正确。
    #[test]
    fn to_appender_rotation_maps_variants() {
        use tracing_appender::rolling::Rotation;
        assert_eq!(to_appender_rotation(LogRotation::Daily), Rotation::DAILY);
        assert_eq!(to_appender_rotation(LogRotation::Hourly), Rotation::HOURLY);
        assert_eq!(to_appender_rotation(LogRotation::Never), Rotation::NEVER);
    }

    /// LogGuard::default() 为空 guard，可安全 drop。
    #[test]
    fn log_guard_default_is_empty() {
        let g = LogGuard::default();
        drop(g);
    }

    /// make_file_writer 对不可写路径返回错误而非 panic。
    #[test]
    fn make_file_writer_fails_on_unwritable_path() {
        // 路径含 NUL 字节在多数平台视为非法，create_dir_all 必失败
        let bad = PathBuf::from("/nonexistent_drive_xyz_root/fuyao_test\0/invalid");
        let paths = AgentPaths {
            fuyao_home: bad,
            ..Default::default()
        };
        let result = make_file_writer(&paths, LogRotation::Daily);
        assert!(result.is_err(), "含 NUL 的非法路径应返回错误");
    }
}
