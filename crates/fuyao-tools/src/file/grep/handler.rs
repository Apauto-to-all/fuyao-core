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

use crate::common::resolve_path;
use crate::config::GREP_MAX_LINE_CHARS;
use crate::file::grep::types::{GrepArgs, GrepMatch, GrepResult};
use crate::redact::redact_sensitive_text;
use fuyao_api::{CancellationToken, ToolCallContext, ToolOutput, parse_args};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;
use ignore::WalkBuilder;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

/// 截断超长行并追加省略标记
///
/// limit 只限匹配条数，单行巨物（压缩 JS、单行 JSON 等）一条即可灌爆上下文，
/// 故对每条返回行再限字符数。按字符而非字节计数截断——多字节字符（如中文）
/// 从字节中间切断会产生乱码；未超上限的行原样返回，不加标记。
fn truncate_line(line: &str) -> String {
    if line.chars().count() <= GREP_MAX_LINE_CHARS {
        return line.to_string();
    }
    let mut truncated: String = line.chars().take(GREP_MAX_LINE_CHARS).collect();
    truncated.push('…');
    truncated
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
/// - `context`: 匹配行的上下文行数
fn search_content(
    pattern: &str,
    path: &str,
    include: Option<&str>,
    limit: usize,
    context: usize,
    cancel: &AtomicBool,
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
                hint: None,
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
            hint: None,
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
        // 协作式取消：超时后由调用方置位，立即退出遍历
        if cancel.load(Ordering::Acquire) {
            break;
        }
        // 已收集满且确认存在更多匹配，无需继续遍历
        if total_count > limit {
            break;
        }
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

                if matches.len() < limit {
                    matches.push(GrepMatch {
                        file: file_path_str.clone(),
                        line: line_num,
                        // 超长行截断：content 与 context 共用同一条截断逻辑
                        content: truncate_line(line.trim_end()),
                        context: if context > 0 {
                            Some(truncate_line(line.trim_end()))
                        } else {
                            None
                        },
                    });
                }

                // 收集满后再统计一条额外匹配即可确认截断，随后停止当前文件扫描
                Ok(total_count <= limit)
            }),
        );

        if let Err(e) = search_result {
            // 跳过无法读取的文件（二进制、权限等）
            let _ = e;
        }
    }

    // 截断判定：跳过收集后仍存在更多匹配（总数超过已收集数）
    let truncated = total_count > matches.len();
    GrepResult {
        matches,
        total_count,
        truncated,
        pattern: pattern.to_string(),
        path: path.to_string(),
        error: None,
        hint: None,
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
pub async fn grep_impl(
    args: Value,
    ctx: ToolCallContext,
    _cancel: CancellationToken,
) -> ToolOutput {
    let GrepArgs {
        pattern,
        path,
        include,
        limit,
        context,
    } = match parse_args(args) {
        Ok(a) => a,
        Err(e) => return ToolOutput::Err(e),
    };
    // limit 硬上限由配置 search_max_results 驱动（usize → i64 防御性转换，防溢出；
    // 上限钳到至少 1，避免配置为 0 时 clamp 区间非法 panic）
    let config_limit =
        i64::try_from(fuyao_api::get_config().tools.limits.search_max_results).unwrap_or(i64::MAX);
    let limit = limit.clamp(1, config_limit.max(1)) as usize;
    let context_lines = context.max(0) as usize;

    if pattern.is_empty() {
        return ToolOutput::error("搜索模式不能为空");
    }

    let workspace = ctx.workspace().map(std::path::Path::to_path_buf);
    let resolved_path_obj = resolve_path(&path, workspace.as_deref());
    let resolved_path = resolved_path_obj.to_string_lossy().to_string();

    let timeout_secs = fuyao_api::get_config().tools.limits.search_timeout_secs;
    // 协作式取消令牌：超时后通知阻塞任务在下一文件处退出（spawn_blocking 无法强制中断线程）
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = cancel.clone();
    let join = tokio::task::spawn_blocking({
        let pattern = pattern.clone();
        let resolved_path = resolved_path.clone();
        move || {
            search_content(
                &pattern,
                &resolved_path,
                include.as_deref(),
                limit,
                context_lines,
                &cancel_clone,
            )
        }
    });
    let result = match tokio::time::timeout(Duration::from_secs(timeout_secs), join).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => GrepResult {
            matches: Vec::new(),
            total_count: 0,
            truncated: false,
            pattern: pattern.to_string(),
            path: resolved_path.clone(),
            error: Some(format!("搜索任务失败: {e}")),
            hint: None,
        },
        Err(_elapsed) => {
            // 通知阻塞任务取消；它会在下一文件迭代处观察到并 break
            cancel.store(true, Ordering::Release);
            GrepResult {
                matches: Vec::new(),
                total_count: 0,
                truncated: false,
                pattern: pattern.to_string(),
                path: resolved_path.clone(),
                error: Some(format!(
                    "搜索超时（超过 {timeout_secs} 秒），请缩小搜索范围、使用更具体的 pattern，或通过 include 参数限定文件类型"
                )),
                hint: None,
            }
        }
    };

    if let Some(err) = &result.error {
        return ToolOutput::error(err);
    }

    let mut result = result;
    redact_content_in_result(&mut result.matches);

    if result.truncated {
        result.hint =
            Some("结果已截断。请使用更具体的 pattern 或 include 参数缩小搜索范围。".to_string());
    }

    ToolOutput::ok(serde_json::to_value(result).unwrap_or_default())
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
            hint: None,
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
            hint: None,
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

    /// limit 超过配置上限（tools.limits.search_max_results）时被钳制截断
    #[tokio::test]
    async fn grep_limit_clamped_to_config_max() {
        let dir = std::env::temp_dir().join("fuyao_test_grep_limit_clamp");
        std::fs::create_dir_all(&dir).unwrap();
        // 写入超过配置默认上限（500）的匹配行，用远超上限的 limit 验证钳制生效
        let content = (0..600)
            .map(|i| format!("needle line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let file_path = dir.join("big.txt");
        std::fs::write(&file_path, content).unwrap();

        let args = serde_json::json!({
            "pattern": "needle",
            "path": file_path.to_string_lossy().to_string(),
            "limit": 100000
        });
        let output = grep_impl(args, ToolCallContext::default(), CancellationToken::new()).await;
        let json = match output {
            ToolOutput::Value(v) => v,
            other => panic!("期望 Value 结果: {other:?}"),
        };

        let max = fuyao_api::get_config().tools.limits.search_max_results;
        assert_eq!(json["matches"].as_array().map(Vec::len), Some(max));
        assert_eq!(json["truncated"], serde_json::json!(true));
        assert_eq!(json["total_count"], serde_json::json!(max + 1));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncate_line_keeps_short_line_intact() {
        assert_eq!(truncate_line("fn main() {}"), "fn main() {}");
        assert_eq!(truncate_line("中文短行"), "中文短行");
    }

    #[test]
    fn truncate_line_truncates_by_chars_not_bytes() {
        // 1000 字符长行，混入中文验证按字符（而非字节）截断——按字节切会切断
        // 多字节字符产生乱码
        let long_line = "中文内容x".repeat(200);
        assert_eq!(long_line.chars().count(), 1000);

        let truncated = truncate_line(&long_line);
        assert_eq!(truncated.chars().count(), GREP_MAX_LINE_CHARS + 1);
        assert!(truncated.ends_with('…'));
        // 截断后仍是合法 UTF-8（字符边界安全，无 panic）
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    /// 超长匹配行经完整搜索链路后被截断：content 与 context 均截到上限 + 省略标记
    #[tokio::test]
    async fn grep_truncates_overlong_match_line_end_to_end() {
        let dir = std::env::temp_dir().join("fuyao_test_grep_line_truncate");
        std::fs::create_dir_all(&dir).unwrap();
        // 1007 字符长行（含中文），单条即可灌爆上下文，验证逐行截断生效
        let long_line = format!("needle {}", "中文内容x".repeat(200));
        assert!(long_line.chars().count() > GREP_MAX_LINE_CHARS);
        let file_path = dir.join("long.txt");
        std::fs::write(&file_path, format!("{long_line}\n")).unwrap();

        let args = serde_json::json!({
            "pattern": "needle",
            "path": file_path.to_string_lossy().to_string(),
            "limit": 10,
            "context": 1
        });
        let output = grep_impl(args, ToolCallContext::default(), CancellationToken::new()).await;
        let json = match output {
            ToolOutput::Value(v) => v,
            other => panic!("期望 Value 结果: {other:?}"),
        };

        let content = json["matches"][0]["content"].as_str().unwrap();
        assert!(content.ends_with('…'), "实际：{content:?}");
        assert_eq!(content.chars().count(), GREP_MAX_LINE_CHARS + 1);

        let context_line = json["matches"][0]["context"].as_str().unwrap();
        assert!(context_line.ends_with('…'), "实际：{context_line:?}");
        assert_eq!(context_line.chars().count(), GREP_MAX_LINE_CHARS + 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
