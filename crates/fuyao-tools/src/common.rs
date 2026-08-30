//! 工具公共函数
//!
//! 提供所有工具共享的辅助函数：参数解析前奏、路径解析、结果信封构造、
//! 阻塞搜索的超时-取消执行。
//!
//! ## 路径解析规则
//!
//! - 无 path 且有 workspace → 返回 workspace
//! - 绝对路径 → 直接返回
//! - 相对路径 → 基于 workspace 解析（无 workspace 则基于 cwd）
//! - `~` 前缀 → 展开为用户主目录
//! - 解析结果统一词法归一：统一为平台分隔符、消除 `.` 段（如 `E:/a/.` → `E:\a`）

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use fuyao_api::{ToolCallContext, ToolError, ToolOutput, parse_args};

/// 类型化解析工具参数，失败折为错误信封
///
/// handler 入口的统一前奏：解析失败即刻得到可直返的 `ToolOutput::Err`，
/// 错误信封（消息 + suggestion 附加字段）与 [`parse_args`] 产出完全一致。
pub(crate) fn parse_tool_args<T: DeserializeOwned>(args: Value) -> Result<T, ToolOutput> {
    parse_args(args).map_err(ToolOutput::Err)
}

/// 从调用上下文解析路径
///
/// 提取 ctx 的 workspace 后按 [`resolve_path`] 规则得到绝对路径：
/// 相对路径基于 workspace 解析（无 workspace 则基于 cwd）。
pub(crate) fn resolve_ctx_path(ctx: &ToolCallContext, path: &str) -> PathBuf {
    let workspace = ctx.workspace().map(Path::to_path_buf);
    resolve_path(path, workspace.as_deref())
}

/// 空路径参数的错误信封
///
/// message 指明约束（不能为空，区别于缺字段——缺字段在参数解析阶段即报错），
/// suggestion 给出修正方向。
pub(crate) fn empty_path_error() -> ToolOutput {
    ToolOutput::Err(ToolError::new("路径参数不能为空").with("suggestion", "请提供有效的文件路径"))
}

/// 序列化结果结构体为 JSON 并包成成功信封
///
/// 序列化失败兜底为空 JSON 对象（`unwrap_or_default`），保证工具恒有可回喂的输出。
pub(crate) fn to_ok_output<T: Serialize>(result: &T) -> ToolOutput {
    ToolOutput::ok(serde_json::to_value(result).unwrap_or_default())
}

/// 在 `spawn_blocking` 中执行阻塞搜索，外层套超时与协作式取消
///
/// 执行模型（glob / grep 共用）：
/// - 阻塞搜索在独立线程运行，不阻塞异步运行时
/// - `timeout_secs` 内未完成即置位 cancel 令牌并返回超时错误——
///   `spawn_blocking` 无法强制中断线程，搜索闭包须轮询令牌在下一迭代处退出
/// - 搜索内部错误（路径不存在、模式无效等）与任务 panic 各自折为独立文案
///
/// # 参数
///
/// - `timeout_secs`: 超时秒数（0 语义为立即超时，由 [`tokio::time::timeout`] 决定）
/// - `timeout_hint`: 超时错误中附加的修正建议文案（调用方各自的指引）
/// - `search`: 阻塞搜索闭包，收 cancel 令牌引用，成功返回 `T`、失败返回错误消息
///
/// # 返回
///
/// `Ok(T)` 为搜索结果；`Err(ToolOutput)` 为三类失败（内部错误 / 任务失败 / 超时）
/// 对应的错误信封，调用方直接返回给上层。
pub(crate) async fn run_search_with_timeout<T, F>(
    timeout_secs: u64,
    timeout_hint: &str,
    search: F,
) -> Result<T, ToolOutput>
where
    T: Send + 'static,
    F: FnOnce(&AtomicBool) -> Result<T, String> + Send + 'static,
{
    // 协作式取消令牌：超时后通知阻塞任务在下一文件处退出
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_task = cancel.clone();
    let join = tokio::task::spawn_blocking(move || search(&cancel_for_task));
    match tokio::time::timeout(Duration::from_secs(timeout_secs), join).await {
        Ok(Ok(r)) => r.map_err(ToolOutput::error),
        Ok(Err(e)) => Err(ToolOutput::error(format!("搜索任务失败: {e}"))),
        Err(_elapsed) => {
            // 通知阻塞任务取消；它会在下一文件迭代处观察到并 break
            cancel.store(true, Ordering::Release);
            Err(ToolOutput::error(format!(
                "搜索超时（超过 {timeout_secs} 秒），{timeout_hint}"
            )))
        }
    }
}

/// 解析路径
///
/// 规则：
/// - 无 path 且有 workspace → 返回 workspace
/// - 绝对路径 → 直接返回
/// - 相对路径 → 基于 workspace 解析（无 workspace 则基于 cwd）
/// - 结果经 [`normalize_lexical`] 词法归一后返回
pub fn resolve_path(path: &str, workspace: Option<&Path>) -> PathBuf {
    let resolved = if path.is_empty() {
        workspace
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
    } else {
        let expanded = expand_tilde(path);
        if expanded.is_absolute() {
            expanded
        } else if let Some(ws) = workspace {
            ws.join(&expanded)
        } else {
            std::env::current_dir().unwrap_or_default().join(&expanded)
        }
    };
    normalize_lexical(resolved)
}

/// 词法归一路径：统一为平台分隔符、消除 `.` 段、相对路径补全为绝对路径
///
/// `std::path::absolute` 的纯词法语义：不访问文件系统、不解析符号链接、
/// 不产生 Windows verbatim 前缀（`\\?\`）、保留 `..` 段。
/// join 只追加不清洗，解析出口统一归一，避免 `E:/a\.` 这类混合形态外泄。
/// 归一失败（如空路径）时原样返回。
fn normalize_lexical(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

/// 展开 ~ 为用户主目录
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
        && let Some(home) = dirs_home()
    {
        return if rest.is_empty() {
            home
        } else {
            let rest_trimmed = rest.trim_start_matches(['/', '\\']);
            home.join(rest_trimmed)
        };
    }
    PathBuf::from(path)
}

/// 用户主目录（优先 HOME，回退 Windows 的 USERPROFILE）
pub(crate) fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_empty_path_returns_workspace() {
        let ws = PathBuf::from("/tmp/project");
        // Windows 下无盘符的根路径按当前盘符补全为绝对路径
        let expected = std::path::absolute(&ws).unwrap();
        let result = resolve_path("", Some(&ws));
        assert_eq!(result, expected);
    }

    #[test]
    fn resolve_absolute_path_returns_as_is() {
        let abs_path = if cfg!(windows) {
            "C:\\usr\\local\\bin"
        } else {
            "/usr/local/bin"
        };
        let result = resolve_path(abs_path, None);
        assert_eq!(result, PathBuf::from(abs_path));
    }

    #[test]
    fn resolve_relative_path_with_workspace() {
        let ws = PathBuf::from("/tmp/project");
        let result = resolve_path("src/main.rs", Some(&ws));
        let expected = std::path::absolute("/tmp/project/src/main.rs").unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn resolve_relative_path_without_workspace() {
        let result = resolve_path("src/main.rs", None);
        let expected = std::path::absolute(
            std::env::current_dir()
                .unwrap_or_default()
                .join("src/main.rs"),
        )
        .unwrap();
        assert_eq!(result, expected);
    }

    /// 归一化消除 join 产生的 `\.` 残段与混合分隔符——默认 path "." 配正斜杠
    /// workspace 的场景，解析出口得到统一平台分隔符的干净路径
    #[test]
    fn resolve_path_normalizes_dot_segment_and_mixed_separators() {
        if cfg!(windows) {
            let ws = PathBuf::from("E:/novel/饵城");
            assert_eq!(
                resolve_path(".", Some(&ws)),
                PathBuf::from(r"E:\novel\饵城")
            );
            assert_eq!(
                resolve_path("E:/novel/饵城/.", None),
                PathBuf::from(r"E:\novel\饵城")
            );
        } else {
            assert_eq!(
                resolve_path("./a/./b", Some(Path::new("/tmp/p"))),
                PathBuf::from("/tmp/p/a/b")
            );
        }
    }

    #[test]
    fn expand_tilde_basic() {
        let result = expand_tilde("~/test");
        assert!(result.is_absolute());
        assert!(result.to_string_lossy().ends_with("test"));
    }

    // ── parse_tool_args ──────────────────────────────────────────

    #[derive(Debug, serde::Deserialize)]
    struct SampleArgs {
        path: String,
    }

    /// 解析成功交出结构体；失败信封与 parse_args 直出逐字节一致
    #[test]
    fn parse_tool_args_passes_envelope_through() {
        let ok: SampleArgs = parse_tool_args(serde_json::json!({ "path": "a.txt" })).unwrap();
        assert_eq!(ok.path, "a.txt");

        let out = parse_tool_args::<SampleArgs>(serde_json::json!({})).unwrap_err();
        let direct = parse_args::<SampleArgs>(serde_json::json!({})).unwrap_err();
        assert_eq!(out.to_wire(), ToolOutput::Err(direct).to_wire());
        assert!(out.to_wire().contains("缺少必填参数"));
    }

    // ── resolve_ctx_path ─────────────────────────────────────────

    /// ctx 注入 workspace 时相对路径基于 workspace 解析；默认 ctx 回退 cwd
    #[test]
    fn resolve_ctx_path_uses_workspace_from_ctx() {
        let ctx = ToolCallContext {
            agent_paths: Some(fuyao_api::AgentPaths {
                workspace: Some(PathBuf::from("/tmp/project")),
                ..fuyao_api::AgentPaths::default()
            }),
            ..ToolCallContext::default()
        };
        assert_eq!(
            resolve_ctx_path(&ctx, "src/main.rs"),
            std::path::absolute("/tmp/project/src/main.rs").unwrap()
        );

        let fallback = resolve_ctx_path(&ToolCallContext::default(), "src/main.rs");
        assert_eq!(
            fallback,
            std::path::absolute(
                std::env::current_dir()
                    .unwrap_or_default()
                    .join("src/main.rs")
            )
            .unwrap()
        );
    }

    // ── empty_path_error ─────────────────────────────────────────

    /// 信封含统一文案与 suggestion 附加字段
    #[test]
    fn empty_path_error_envelope_shape() {
        let wire = empty_path_error().to_wire();
        let parsed: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed["error"], "路径参数不能为空");
        assert_eq!(parsed["suggestion"], "请提供有效的文件路径");
    }

    // ── to_ok_output ─────────────────────────────────────────────

    #[derive(Serialize)]
    struct SampleResult {
        path: String,
        bytes: usize,
    }

    /// 结果结构体序列化为 JSON 成功信封
    #[test]
    fn to_ok_output_serializes_struct() {
        let out = to_ok_output(&SampleResult {
            path: "a.txt".into(),
            bytes: 3,
        });
        let wire = out.to_wire();
        let parsed: Value = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed["path"], "a.txt");
        assert_eq!(parsed["bytes"], 3);
    }

    // ── run_search_with_timeout ──────────────────────────────────

    /// 搜索成功：闭包结果原样透传
    #[tokio::test]
    async fn run_search_returns_closure_result() {
        let out =
            run_search_with_timeout(5, "提示", |_cancel| -> Result<usize, String> { Ok(42) })
                .await
                .unwrap();
        assert_eq!(out, 42);
    }

    /// 搜索内部错误：折为 error 信封，消息原样保留
    #[tokio::test]
    async fn run_search_wraps_internal_error() {
        let err = run_search_with_timeout(5, "提示", |_cancel| -> Result<usize, String> {
            Err("路径不存在: x".to_string())
        })
        .await
        .unwrap_err();
        let wire = err.to_wire();
        assert!(wire.contains("路径不存在: x"), "实际：{wire}");
    }

    /// 搜索任务 panic：折为「搜索任务失败」信封
    #[tokio::test]
    async fn run_search_wraps_join_panic() {
        let err = run_search_with_timeout(5, "提示", |_cancel| -> Result<usize, String> {
            panic!("搜索线程崩溃");
        })
        .await
        .unwrap_err();
        assert!(err.to_wire().contains("搜索任务失败"));
    }

    /// 超时：返回含秒数与修正建议的超时信封，且 cancel 令牌对闭包可见
    #[tokio::test]
    async fn run_search_timeout_signals_cancel() {
        // 闭包轮询 cancel 令牌，观察到即记录并退出
        let observed = Arc::new(AtomicBool::new(false));
        let observed_clone = observed.clone();
        let err = run_search_with_timeout(
            1,
            "请缩小范围",
            move |cancel| -> Result<usize, String> {
                loop {
                    if cancel.load(Ordering::Acquire) {
                        observed_clone.store(true, Ordering::Release);
                        return Err("被取消".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            },
        )
        .await
        .unwrap_err();

        let wire = err.to_wire();
        assert!(wire.contains("搜索超时（超过 1 秒）"), "实际：{wire}");
        assert!(wire.contains("请缩小范围"), "实际：{wire}");

        // 阻塞线程异步观察到令牌（至多再等一个轮询周期）
        for _ in 0..100 {
            if observed.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(observed.load(Ordering::Acquire), "闭包应观察到取消令牌");
    }
}
