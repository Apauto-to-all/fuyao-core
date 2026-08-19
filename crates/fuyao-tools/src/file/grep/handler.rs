//! 文件内容搜索工具
//!
//! 使用正则表达式搜索文件内容。
//! 基于 grep crate（ripgrep 的库形式）实现，无需外部 rg 进程。
//!
//! ## 实现
//!
//! 使用 `grep-regex` 构建正则匹配器，`grep-searcher` 逐行搜索，
//! `ignore::WalkBuilder` 遍历目录树（自动遵守 .gitignore）。
//! 支持 glob 参数以 glob 语法过滤文件（如 *.py、*.{ts,tsx}，`!` 前缀排除）。
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
use ignore::overrides::OverrideBuilder;
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
/// glob 参数经 `OverrideBuilder` 编译后挂载到遍历器——无斜杠模式自动跨
/// 目录匹配（`*.rs` 匹配任意层级下的 .rs 文件），`!` 前缀表示排除；
/// 遍历时目录级剪枝，不匹配的目录直接跳过。
///
/// # 参数
///
/// - `pattern`: 正则表达式
/// - `path`: 搜索根路径
/// - `glob`: glob 过滤模式（如 `*.py`、`*.{ts,tsx}`，`!` 前缀排除）
/// - `limit`: 最大返回数量
/// - `context`: 匹配行的上下文行数
///
/// 内部错误（正则/路径/glob 无效等）经 `Err(String)` 返回，由调用方折成
/// `ToolOutput::Err`——结果信封不携带错误字段。
fn search_content(
    pattern: &str,
    path: &str,
    glob: Option<&str>,
    limit: usize,
    context: usize,
    cancel: &AtomicBool,
) -> Result<GrepResult, String> {
    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(true)
        .line_terminator(Some(b'\n'))
        .build(pattern)
    {
        Ok(m) => m,
        Err(e) => return Err(format!("正则表达式无效: {e}")),
    };

    let search_path = crate::common::expand_tilde(path);
    if !search_path.exists() {
        return Err(format!("路径不存在: {path}"));
    }

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(context)
        .after_context(context)
        .build();

    let mut walker = WalkBuilder::new(&search_path);
    walker
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);

    // glob 过滤：OverrideBuilder 编译为覆盖规则挂载到遍历器，编译失败直接报错
    // （替代静默失配——无效模式对调用方可见，可据此修正）
    if let Some(g) = glob {
        let mut override_builder = OverrideBuilder::new(&search_path);
        if let Err(e) = override_builder.add(g) {
            return Err(format!("glob 模式无效: {e}"));
        }
        match override_builder.build() {
            Ok(overrides) => {
                walker.overrides(overrides);
            }
            Err(e) => return Err(format!("glob 模式无效: {e}")),
        }
    }

    let mut matches: Vec<GrepMatch> = Vec::new();
    // 收集满后是否仍发现更多匹配——截断判定的唯一依据
    let mut found_extra = false;

    for entry in walker.build().flatten() {
        // 协作式取消：超时后由调用方置位，立即退出遍历
        if cancel.load(Ordering::Acquire) {
            break;
        }
        // 已确认存在更多匹配，无需继续遍历
        if found_extra {
            break;
        }
        let file_type = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if !file_type.is_file() {
            continue;
        }

        let file_path = entry.path();
        let file_path_str = file_path.to_string_lossy().to_string();

        let search_result = searcher.search_path(
            &matcher,
            file_path,
            UTF8(|line_num, line| {
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
                    Ok(true)
                } else {
                    // 收集满后再确认一个额外匹配即可判定截断，随后停止当前文件扫描
                    found_extra = true;
                    Ok(false)
                }
            }),
        );

        if let Err(e) = search_result {
            // 跳过无法读取的文件（二进制、权限等）
            let _ = e;
        }
    }

    Ok(GrepResult {
        matches,
        truncated: found_extra,
        pattern: pattern.to_string(),
        path: path.to_string(),
        hint: None,
    })
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
        glob,
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
                glob.as_deref(),
                limit,
                context_lines,
                &cancel_clone,
            )
        }
    });
    let result = match tokio::time::timeout(Duration::from_secs(timeout_secs), join).await {
        Ok(Ok(r)) => match r {
            Ok(g) => g,
            Err(e) => return ToolOutput::error(e),
        },
        Ok(Err(e)) => return ToolOutput::error(format!("搜索任务失败: {e}")),
        Err(_elapsed) => {
            // 通知阻塞任务取消；它会在下一文件迭代处观察到并 break
            cancel.store(true, Ordering::Release);
            return ToolOutput::error(format!(
                "搜索超时（超过 {timeout_secs} 秒），请缩小搜索范围、使用更具体的 pattern，或通过 glob 参数限定文件类型"
            ));
        }
    };

    let mut result = result;
    redact_content_in_result(&mut result.matches);

    if result.truncated {
        result.hint =
            Some("结果已截断。请使用更具体的 pattern 或 glob 参数缩小搜索范围。".to_string());
    }

    ToolOutput::ok(serde_json::to_value(result).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

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

        std::fs::remove_dir_all(&dir).ok();
    }

    /// glob 参数按扩展名过滤搜索范围：不匹配的文件不参与内容匹配
    #[tokio::test]
    async fn grep_glob_filters_files_by_extension() {
        let dir = std::env::temp_dir().join("fuyao_test_grep_glob_filter");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "needle in rust\n").unwrap();
        std::fs::write(dir.join("b.md"), "needle in markdown\n").unwrap();

        let args = serde_json::json!({
            "pattern": "needle",
            "path": dir.to_string_lossy().to_string(),
            "glob": "*.rs"
        });
        let output = grep_impl(args, ToolCallContext::default(), CancellationToken::new()).await;
        let json = match output {
            ToolOutput::Value(v) => v,
            other => panic!("期望 Value 结果: {other:?}"),
        };

        let matches = json["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert!(
            matches[0]["file"].as_str().unwrap().ends_with("a.rs"),
            "实际：{matches:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// glob 参数 `!` 前缀排除匹配文件，其余文件照常搜索
    #[tokio::test]
    async fn grep_glob_excludes_with_bang_prefix() {
        let dir = std::env::temp_dir().join("fuyao_test_grep_glob_exclude");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "needle in rust\n").unwrap();
        std::fs::write(dir.join("b.md"), "needle in markdown\n").unwrap();

        let args = serde_json::json!({
            "pattern": "needle",
            "path": dir.to_string_lossy().to_string(),
            "glob": "!*.md"
        });
        let output = grep_impl(args, ToolCallContext::default(), CancellationToken::new()).await;
        let json = match output {
            ToolOutput::Value(v) => v,
            other => panic!("期望 Value 结果: {other:?}"),
        };

        let matches = json["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert!(
            matches[0]["file"].as_str().unwrap().ends_with("a.rs"),
            "实际：{matches:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 无效 glob 模式返回明确错误，替代静默零结果
    #[tokio::test]
    async fn grep_glob_invalid_pattern_returns_error() {
        let dir = std::env::temp_dir().join("fuyao_test_grep_glob_invalid");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "needle\n").unwrap();

        let args = serde_json::json!({
            "pattern": "needle",
            "path": dir.to_string_lossy().to_string(),
            "glob": "[invalid"
        });
        let output = grep_impl(args, ToolCallContext::default(), CancellationToken::new()).await;
        // 无效 glob 经 ToolOutput::Err 通道返回（带明确消息），而非 Value 内的 error 字段
        let message = match output {
            ToolOutput::Err(e) => e.message,
            other => panic!("期望 Err 结果: {other:?}"),
        };
        assert!(message.contains("glob 模式无效"), "实际：{message}");

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
