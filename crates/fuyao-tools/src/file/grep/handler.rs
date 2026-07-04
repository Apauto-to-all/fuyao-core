//! 文件内容搜索工具
//!
//! 使用正则表达式搜索文件内容。
//! 基于 grep crate（ripgrep 的库形式）实现，无需外部 rg 进程。
//!
//! ## 实现
//!
//! 使用 `grep-regex` 构建正则匹配器，`grep-searcher` 逐行搜索，
//! `ignore::WalkBuilder` 遍历目录树（自动遵守 .gitignore）。
//! 支持 include 参数过滤文件类型（如 *.py、*.{ts,tsx}）。
//! 支持 context 参数显示匹配行的上下文。
//! 搜索结果自动脱敏 API Key 等敏感信息。

use crate::common::{self, resolve_path};
use crate::file::grep::types::{GrepMatch, GrepResult};
use crate::redact::redact_sensitive_text;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;
use ignore::WalkBuilder;
use serde_json::Value;

/// 脱敏搜索结果中的敏感信息
///
/// 遍历 matches 数组，对每个 match 的 content 和 context 字段执行脱敏。
fn redact_content_in_result(matches: &mut [GrepMatch]) {
    for match_item in matches.iter_mut() {
        match_item.content = redact_sensitive_text(&match_item.content);
        if let Some(ref mut context) = match_item.context {
            *context = redact_sensitive_text(context);
        }
    }
}

/// 内容搜索的核心实现
///
/// 使用 `grep-regex` 构建正则匹配器，`grep-searcher` 逐行搜索文件内容。
/// 通过 `ignore::WalkBuilder` 遍历目录树，自动遵守 .gitignore 规则。
///
/// # 参数
///
/// - `pattern`: 正则表达式
/// - `path`: 搜索根路径
/// - `include`: 文件过滤模式（如 `*.py`、`*.{ts,tsx}`）
/// - `limit`: 最大返回数量
/// - `offset`: 跳过前 N 个结果
/// - `context`: 匹配行的上下文行数
fn search_content(
    pattern: &str,
    path: &str,
    include: Option<&str>,
    limit: usize,
    offset: usize,
    context: usize,
) -> GrepResult {
    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(true)
        .line_terminator(Some(b'\n'))
        .build(pattern)
    {
        Ok(m) => m,
        Err(e) => {
            return GrepResult {
                matches: Vec::new(),
                total_count: 0,
                truncated: false,
                pattern: pattern.to_string(),
                path: path.to_string(),
                error: Some(format!("正则表达式无效: {e}")),
                _hint: None,
            };
        }
    };

    let search_path = crate::common::expand_tilde(path);
    if !search_path.exists() {
        return GrepResult {
            matches: Vec::new(),
            total_count: 0,
            truncated: false,
            pattern: pattern.to_string(),
            path: path.to_string(),
            error: Some(format!("路径不存在: {path}")),
            _hint: None,
        };
    }

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(context)
        .after_context(context)
        .build();

    let walker = WalkBuilder::new(&search_path)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut matches: Vec<GrepMatch> = Vec::new();
    let mut total_count = 0usize;

    for entry in walker.flatten() {
        let file_type = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if !file_type.is_file() {
            continue;
        }

        if let Some(inc) = include {
            let name = entry.file_name().to_string_lossy();
            if !match_glob_pattern(&name, inc) {
                continue;
            }
        }

        let file_path = entry.path();
        let file_path_str = file_path.to_string_lossy().to_string();

        let search_result = searcher.search_path(
            &matcher,
            file_path,
            UTF8(|line_num, line| {
                total_count += 1;

                if matches.len() < limit && total_count > offset {
                    matches.push(GrepMatch {
                        file: file_path_str.clone(),
                        line: line_num,
                        content: line.trim_end().to_string(),
                        context: if context > 0 {
                            Some(line.trim_end().to_string())
                        } else {
                            None
                        },
                    });
                }

                Ok(matches.len() < limit + offset)
            }),
        );

        if let Err(e) = search_result {
            // 跳过无法读取的文件（二进制、权限等）
            let _ = e;
        }
    }

    GrepResult {
        matches,
        total_count,
        truncated: total_count > offset + limit,
        pattern: pattern.to_string(),
        path: path.to_string(),
        error: None,
        _hint: None,
    }
}

/// 文件名通配符匹配
///
/// 支持两种模式：
/// - `*.rs` → 匹配以 .rs 结尾的文件名
/// - `*.{ts,tsx}` → 匹配以 .ts 或 .tsx 结尾的文件名
fn match_glob_pattern(name: &str, pattern: &str) -> bool {
    // 支持 *.rs
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return name.ends_with(&format!(".{suffix}"));
    }
    // 支持 *.{ts,tsx}
    if pattern.starts_with("*.{")
        && let Some(inner) = pattern
            .strip_prefix("*.{")
            .and_then(|s| s.strip_suffix('}'))
    {
        return inner
            .split(',')
            .any(|ext| name.ends_with(&format!(".{}", ext.trim())));
    }
    name == pattern
}

/// grep 工具的异步入口
///
/// 解析参数后，在 `spawn_blocking` 中执行搜索（避免阻塞异步运行时）。
pub async fn grep_impl(args: Value, ctx: &fuyao_api::ToolCallContext) -> String {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let include = args.get("include").and_then(|v| v.as_str());
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(50)
        .clamp(1, 100) as usize;
    let offset = args
        .get("offset")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0) as usize;
    let context_lines = args
        .get("context")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0) as usize;

    if pattern.is_empty() {
        return common::tool_error("搜索模式不能为空");
    }

    let workspace = ctx.workspace().map(std::path::Path::to_path_buf);
    let resolved_path_obj = resolve_path(path, workspace.as_deref());
    let resolved_path = resolved_path_obj.to_string_lossy().to_string();

    let result = tokio::task::spawn_blocking({
        let pattern = pattern.to_string();
        let resolved_path = resolved_path.clone();
        let include = include.map(|s| s.to_string());
        move || {
            search_content(
                &pattern,
                &resolved_path,
                include.as_deref(),
                limit,
                offset,
                context_lines,
            )
        }
    })
    .await
    .unwrap_or_else(|e| GrepResult {
        matches: Vec::new(),
        total_count: 0,
        truncated: false,
        pattern: pattern.to_string(),
        path: resolved_path.clone(),
        error: Some(format!("搜索任务失败: {e}")),
        _hint: None,
    });

    if let Some(err) = &result.error {
        return common::tool_error(err);
    }

    let mut result = result;
    redact_content_in_result(&mut result.matches);

    if result.truncated {
        let next_offset = offset + limit;
        result._hint = Some(format!(
            "结果已截断。使用 offset={next_offset} 查看更多，或使用更具体的 pattern 或 include 缩小范围。"
        ));
    }

    common::tool_result(serde_json::to_value(result).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_result_error_field_not_serialized_when_none() {
        let result = GrepResult {
            matches: vec![],
            total_count: 0,
            truncated: false,
            pattern: "test".to_string(),
            path: ".".to_string(),
            error: None,
            _hint: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert!(json.get("error").is_none());
    }

    #[test]
    fn grep_result_error_field_serialized_when_some() {
        let result = GrepResult {
            matches: vec![],
            total_count: 0,
            truncated: false,
            pattern: "test".to_string(),
            path: ".".to_string(),
            error: Some("正则表达式无效".to_string()),
            _hint: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["error"], "正则表达式无效");
    }

    #[test]
    fn grep_match_serializes_all_fields() {
        let m = GrepMatch {
            file: "src/main.rs".to_string(),
            line: 42,
            content: "fn main() {}".to_string(),
            context: None,
        };
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["file"], "src/main.rs");
        assert_eq!(json["line"], 42);
        assert_eq!(json["content"], "fn main() {}");
        assert!(json.get("context").is_none());
    }

    #[test]
    fn grep_match_serializes_context_when_some() {
        let m = GrepMatch {
            file: "src/main.rs".to_string(),
            line: 42,
            content: "fn main() {}".to_string(),
            context: Some("上下文".to_string()),
        };
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["context"], "上下文");
    }

    #[test]
    fn redact_content_in_result_preserves_normal_text() {
        let mut matches = vec![GrepMatch {
            file: "test.rs".to_string(),
            line: 1,
            content: "fn main() {}".to_string(),
            context: None,
        }];
        redact_content_in_result(&mut matches);
        assert_eq!(matches[0].content, "fn main() {}");
    }
}
