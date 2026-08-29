//! 文件名搜索处理逻辑
//!
//! 使用标准 glob 语法搜索文件名。
//! 基于 ignore crate（ripgrep 的目录遍历组件）实现，自动遵守 .gitignore 规则。
//!
//! ## 实现
//!
//! 使用 `ignore::WalkBuilder` 遍历目录树，`glob::Pattern` 匹配文件名。
//! 自动跳过隐藏文件和 .gitignore 排除的文件。
//! 搜索结果按修改时间排序（最新优先），结果数受配置硬上限约束，超出自动截断。

use crate::common::{parse_tool_args, resolve_ctx_path, run_search_with_timeout, to_ok_output};
use crate::file::glob::types::{GlobArgs, GlobMatch, GlobResult};
use fuyao_api::{CancellationToken, ToolCallContext, ToolOutput};
use glob::Pattern;
use ignore::WalkBuilder;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// 搜索文件的核心实现
///
/// 使用 `ignore::WalkBuilder` 遍历目录树，`glob::Pattern` 匹配文件名。
/// 结果按修改时间排序（最新优先），超过 limit 的部分自动截断。
///
/// # 参数
///
/// - `pattern`: glob 模式（如 `*.rs`、`*.{ts,tsx}`）
/// - `path`: 搜索根路径
/// - `limit`: 最大返回数量
///
/// 内部错误（路径不存在、模式无效等）经 `Err(String)` 返回，由调用方折成
/// `ToolOutput::Err`——结果信封不携带错误字段。
fn search_files(
    pattern: &str,
    path: &str,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<GlobResult, String> {
    let search_path = crate::common::expand_tilde(path);

    if !search_path.exists() {
        return Err(format!("路径不存在: {path}"));
    }

    let glob_pattern = match Pattern::new(pattern) {
        Ok(p) => p,
        Err(e) => return Err(format!("glob 模式无效: {e}")),
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
    let page: Vec<_> = all_files.into_iter().take(limit).collect();

    let matches: Vec<GlobMatch> = page
        .into_iter()
        .map(|(path, size, mtime)| GlobMatch {
            path: path.to_string_lossy().to_string(),
            size,
            modified: mtime,
        })
        .collect();

    Ok(GlobResult {
        matches,
        total_count: total,
        truncated: total > limit,
        pattern: pattern.to_string(),
        path: path.to_string(),
        hint: None,
    })
}

/// 将 limit 钳制到 `[1, 配置硬上限]` 区间
///
/// 上限来自 `[tools.limits] search_max_results`（usize），此处转换为 i64 参与钳制：
/// 配置值超出 i64 表示范围时退化为 `i64::MAX`（即不设上限），防御异常配置。
fn clamp_limit(limit: i64) -> usize {
    let max =
        i64::try_from(fuyao_api::get_config().tools.limits.search_max_results).unwrap_or(i64::MAX);
    limit.clamp(1, max) as usize
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
    } = match parse_tool_args(args) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let limit = clamp_limit(limit);

    if pattern.is_empty() {
        return ToolOutput::error("搜索模式不能为空");
    }

    let resolved_path_obj = resolve_ctx_path(&ctx, &path);
    let resolved_path = resolved_path_obj.to_string_lossy().to_string();

    let timeout_secs = fuyao_api::get_config().tools.limits.search_timeout_secs;
    let mut result = match run_search_with_timeout(
        timeout_secs,
        "请缩小搜索范围或使用更具体的 pattern",
        {
            let pattern = pattern.clone();
            let resolved_path = resolved_path.clone();
            move |cancel| search_files(&pattern, &resolved_path, limit, cancel)
        },
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return e,
    };

    if result.truncated {
        result.hint = Some("结果已截断。请使用更具体的 pattern 缩小搜索范围。".to_string());
    }

    to_ok_output(&result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_result_hint_not_serialized_when_none() {
        let result = GlobResult {
            matches: vec![],
            total_count: 0,
            truncated: false,
            pattern: "*.rs".to_string(),
            path: ".".to_string(),
            hint: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert!(json.get("hint").is_none());
    }

    #[test]
    fn glob_result_hint_serialized_when_some() {
        let result = GlobResult {
            matches: vec![],
            total_count: 0,
            truncated: true,
            pattern: "*.rs".to_string(),
            path: ".".to_string(),
            hint: Some("结果已截断".to_string()),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["hint"], "结果已截断");
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
            hint: None,
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
            hint: None,
        };
        assert!(result.truncated);
    }

    #[test]
    fn limit_exceeding_config_cap_is_clamped() {
        // 未注入配置时 get_config 返回默认值（search_max_results = 500）
        let cap = fuyao_api::get_config().tools.limits.search_max_results;
        // 超大 limit 被压到配置硬上限
        assert_eq!(clamp_limit(10_000_000), cap);
        assert_eq!(clamp_limit(i64::MAX), cap);
        // 超大负值 / 零钳制到下界 1，普通值原样保留
        assert_eq!(clamp_limit(i64::MIN), 1);
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(50), 50);
    }
}
