//! 文件名搜索处理逻辑
//!
//! 使用标准 glob 语法搜索文件名。
//! 基于 ignore crate（ripgrep 的目录遍历组件）实现，自动遵守 .gitignore 规则。
//!
//! ## 实现
//!
//! 使用 `ignore::WalkBuilder` 遍历目录树，`glob::Pattern` 匹配文件名。
//! 自动跳过隐藏文件和 .gitignore 排除的文件。
//! 搜索结果按修改时间排序（最新优先），支持 offset/limit 分页。

use crate::common::resolve_path;
use crate::file::glob::types::{GlobArgs, GlobMatch, GlobResult, MAX_LIMIT};
use fuyao_api::{CancellationToken, ToolCallContext, ToolOutput, parse_args};
use glob::Pattern;
use ignore::WalkBuilder;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// 搜索文件的核心实现
///
/// 使用 `ignore::WalkBuilder` 遍历目录树，`glob::Pattern` 匹配文件名。
/// 结果按修改时间排序（最新优先），支持 offset/limit 分页。
///
/// # 参数
///
/// - `pattern`: glob 模式（如 `*.rs`、`*.{ts,tsx}`）
/// - `path`: 搜索根路径
/// - `limit`: 最大返回数量
/// - `offset`: 跳过前 N 个结果
fn search_files(
    pattern: &str,
    path: &str,
    limit: usize,
    offset: usize,
    cancel: &AtomicBool,
) -> GlobResult {
    let search_path = crate::common::expand_tilde(path);

    if !search_path.exists() {
        return GlobResult {
            matches: Vec::new(),
            total_count: 0,
            truncated: false,
            pattern: pattern.to_string(),
            path: path.to_string(),
            error: Some(format!("路径不存在: {path}")),
            _hint: None,
        };
    }

    let glob_pattern = match Pattern::new(pattern) {
        Ok(p) => p,
        Err(e) => {
            return GlobResult {
                matches: Vec::new(),
                total_count: 0,
                truncated: false,
                pattern: pattern.to_string(),
                path: path.to_string(),
                error: Some(format!("glob 模式无效: {e}")),
                _hint: None,
            };
        }
    };

    let walker = WalkBuilder::new(&search_path)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut all_files: Vec<(PathBuf, u64, u64)> = Vec::new();

    for entry in walker.flatten() {
        // 协作式取消：超时后由调用方置位，立即退出遍历
        if cancel.load(Ordering::Acquire) {
            break;
        }
        let file_type = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if !file_type.is_file() {
            continue;
        }

        let match_target = if pattern.contains('/') || pattern.contains('\\') {
            // 模式包含路径分隔符，匹配完整路径
            entry.path().to_string_lossy().to_string()
        } else {
            // 模式只有文件名，只匹配文件名
            entry.file_name().to_string_lossy().to_string()
        };
        if !glob_pattern.matches(&match_target) {
            continue;
        }

        if let Ok(meta) = entry.metadata() {
            // 获取修改时间戳（秒），用于排序和返回
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            all_files.push((entry.into_path(), meta.len(), mtime));
        }
    }

    // 按修改时间排序（最新优先）
    all_files.sort_by_key(|b| std::cmp::Reverse(b.2));

    let total = all_files.len();
    let page: Vec<_> = all_files.into_iter().skip(offset).take(limit).collect();

    let matches: Vec<GlobMatch> = page
        .into_iter()
        .map(|(path, size, mtime)| GlobMatch {
            path: path.to_string_lossy().to_string(),
            size,
            modified: mtime,
        })
        .collect();

    GlobResult {
        matches,
        total_count: total,
        truncated: total > offset + limit,
        pattern: pattern.to_string(),
        path: path.to_string(),
        error: None,
        _hint: None,
    }
}

/// glob 工具的异步入口
///
/// 解析参数后，在 `spawn_blocking` 中执行目录遍历（避免阻塞异步运行时）。
pub async fn glob_impl(
    args: Value,
    ctx: ToolCallContext,
    _cancel: CancellationToken,
) -> ToolOutput {
    let GlobArgs {
        pattern,
        path,
        limit,
        offset,
    } = match parse_args(args) {
        Ok(a) => a,
        Err(e) => return ToolOutput::Err(e),
    };
    let limit = limit.clamp(1, MAX_LIMIT) as usize;
    let offset = offset.max(0) as usize;

    if pattern.is_empty() {
        return ToolOutput::error("搜索模式不能为空");
    }

    let workspace = ctx.workspace().map(Path::to_path_buf);
    let resolved_path_obj = resolve_path(&path, workspace.as_deref());
    let resolved_path = resolved_path_obj.to_string_lossy().to_string();

    let pattern_owned = pattern.to_string();
    let resolved_path_clone = resolved_path.clone();

    let timeout_secs = fuyao_api::get_config().tools.limits.search_timeout_secs;
    // 协作式取消令牌：超时后通知阻塞任务在下一文件处退出（spawn_blocking 无法强制中断线程）
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = cancel.clone();
    let join = tokio::task::spawn_blocking(move || {
        search_files(
            &pattern_owned,
            &resolved_path_clone,
            limit,
            offset,
            &cancel_clone,
        )
    });
    let result = match tokio::time::timeout(Duration::from_secs(timeout_secs), join).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => GlobResult {
            matches: Vec::new(),
            total_count: 0,
            truncated: false,
            pattern: pattern.to_string(),
            path: resolved_path.clone(),
            error: Some(format!("搜索任务失败: {e}")),
            _hint: None,
        },
        Err(_elapsed) => {
            // 通知阻塞任务取消；它会在下一文件迭代处观察到并 break
            cancel.store(true, Ordering::Release);
            GlobResult {
                matches: Vec::new(),
                total_count: 0,
                truncated: false,
                pattern: pattern.to_string(),
                path: resolved_path.clone(),
                error: Some(format!(
                    "搜索超时（超过 {timeout_secs} 秒），请缩小搜索范围或使用更具体的 pattern"
                )),
                _hint: None,
            }
        }
    };

    if let Some(err) = &result.error {
        return ToolOutput::error(err);
    }

    let mut result = result;
    if result.truncated {
        let next_offset = offset + limit;
        result._hint = Some(format!(
            "结果已截断。使用 offset={next_offset} 查看更多，或使用更具体的 pattern 缩小范围。"
        ));
    }

    ToolOutput::ok(serde_json::to_value(result).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_result_error_field_not_serialized_when_none() {
        let result = GlobResult {
            matches: vec![],
            total_count: 0,
            truncated: false,
            pattern: "*.rs".to_string(),
            path: ".".to_string(),
            error: None,
            _hint: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert!(json.get("error").is_none());
    }

    #[test]
    fn glob_result_error_field_serialized_when_some() {
        let result = GlobResult {
            matches: vec![],
            total_count: 0,
            truncated: false,
            pattern: "*.rs".to_string(),
            path: ".".to_string(),
            error: Some("路径不存在".to_string()),
            _hint: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["error"], "路径不存在");
    }

    #[test]
    fn glob_result_hint_not_serialized_when_none() {
        let result = GlobResult {
            matches: vec![],
            total_count: 0,
            truncated: false,
            pattern: "*.rs".to_string(),
            path: ".".to_string(),
            error: None,
            _hint: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert!(json.get("_hint").is_none());
    }

    #[test]
    fn glob_result_hint_serialized_when_some() {
        let result = GlobResult {
            matches: vec![],
            total_count: 0,
            truncated: true,
            pattern: "*.rs".to_string(),
            path: ".".to_string(),
            error: None,
            _hint: Some("结果已截断".to_string()),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["_hint"], "结果已截断");
    }

    #[test]
    fn glob_match_serializes_all_fields() {
        let m = GlobMatch {
            path: "src/main.rs".to_string(),
            size: 1024,
            modified: 1234567890,
        };
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["path"], "src/main.rs");
        assert_eq!(json["size"], 1024);
        assert_eq!(json["modified"], 1234567890);
    }

    #[test]
    fn glob_result_truncated_false_when_total_within_limit() {
        let result = GlobResult {
            matches: vec![],
            total_count: 5,
            truncated: false,
            pattern: "*.rs".to_string(),
            path: ".".to_string(),
            error: None,
            _hint: None,
        };
        assert!(!result.truncated);
    }

    #[test]
    fn glob_result_truncated_true_when_total_exceeds_limit() {
        let result = GlobResult {
            matches: vec![],
            total_count: 100,
            truncated: true,
            pattern: "*.rs".to_string(),
            path: ".".to_string(),
            error: None,
            _hint: None,
        };
        assert!(result.truncated);
    }
}
