//! 文件名搜索处理逻辑
//!
//! 按 gitignore 语义的 glob 模式搜索文件名。
//! 基于 ignore crate（ripgrep 的目录遍历组件）实现，自动遵守 .gitignore 规则。
//!
//! ## 实现
//!
//! 使用 `ignore::WalkBuilder` 遍历目录树，`ignore::overrides::OverrideBuilder`
//! 编译模式并对遍历结果逐项判定：Whitelist / None 收录，Ignore 跳过。
//! 自动跳过隐藏文件和 .gitignore 排除的文件。
//! 搜索结果按修改时间排序（最新优先），结果数受配置硬上限约束，超出自动截断。

use crate::common::{parse_tool_args, resolve_ctx_path, run_search_with_timeout, to_ok_output};
use crate::file::glob::types::{GlobArgs, GlobMatch, GlobResult};
use fuyao_api::{CancellationToken, ToolCallContext, ToolOutput};
use ignore::Match;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// 搜索文件的核心实现
///
/// 使用 `ignore::WalkBuilder` 遍历目录树，`ignore::overrides` 覆盖规则逐项判定。
/// 结果按修改时间排序（最新优先），超过 limit 的部分自动截断。
///
/// 模式为 gitignore 语义：无分隔符匹配任意层级的文件名，含 `/` 锚定搜索根
/// （如 `src/*.rs`），`!` 前缀排除（如 `!*.log`），`{a,b}` 花括号展开
/// （如 `*.{md,txt}`）——与 grep 工具的 glob 参数同一套模式语言。
///
/// # 参数
///
/// - `pattern`: glob 模式
/// - `path`: 搜索根路径（已由调用方解析归一）
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
    let search_path = PathBuf::from(path);

    if !search_path.exists() {
        return Err(format!("路径不存在: {path}"));
    }

    // 模式编译为覆盖规则，编译失败直接报错（替代静默失配——
    // 无效模式对调用方可见，可据此修正）
    let mut override_builder = OverrideBuilder::new(&search_path);
    if let Err(e) = override_builder.add(pattern) {
        return Err(format!("glob 模式无效: {e}"));
    }
    let overrides = match override_builder.build() {
        Ok(o) => o,
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

        // 匹配目标：相对搜索根的路径；搜索根本身是文件时退回文件名
        let match_target = match entry.path().strip_prefix(&search_path) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel,
            _ => Path::new(entry.file_name()),
        };
        // Whitelist 命中收录；纯排除模式下无规则命中（None）同样收录，
        // Ignore 才跳过——白名单模式下未命中会直接判为 Ignore
        if matches!(overrides.matched(match_target, false), Match::Ignore(_)) {
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

    /// 含分隔符模式相对搜索根匹配：`sub/*.md` 直接命中，无需 `**/` 对齐
    /// 盘符等根前缀；结果路径已归一——无 `\.` 残段，匹配项位于回显根之下
    #[tokio::test]
    async fn glob_separator_pattern_matches_relative_to_root() {
        let dir = std::env::temp_dir().join("fuyao_test_glob_rel_pattern");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("笔记.md"), "内容").unwrap();
        std::fs::write(dir.join("sub").join("草稿.txt"), "内容").unwrap();

        // workspace 以正斜杠书写，复现混合分隔符输入场景
        let ctx_for = || ToolCallContext {
            agent_paths: Some(fuyao_api::AgentPaths {
                workspace: Some(PathBuf::from(dir.to_string_lossy().replace('\\', "/"))),
                ..fuyao_api::AgentPaths::default()
            }),
            ..ToolCallContext::default()
        };

        let args = serde_json::json!({ "pattern": "sub/*.md", "path": "." });
        let output = glob_impl(args, ctx_for(), CancellationToken::new()).await;
        let json = match output {
            ToolOutput::Value(v) => v,
            other => panic!("期望 Value 结果: {other:?}"),
        };

        assert_eq!(json["total_count"], serde_json::json!(1));
        let root = json["path"].as_str().unwrap().to_string();
        assert!(!root.contains(r"\."), "实际：{root}");
        let matched = json["matches"][0]["path"].as_str().unwrap().to_string();
        assert!(
            matched.starts_with(&root),
            "匹配项应位于搜索根之下：{matched} vs {root}"
        );
        assert!(
            matched.replace('/', "\\").ends_with(r"sub\笔记.md"),
            "实际：{matched}"
        );

        // `**/` 前缀跨目录匹配保持可用
        let args = serde_json::json!({ "pattern": "**/草稿.txt", "path": "." });
        let output = glob_impl(args, ctx_for(), CancellationToken::new()).await;
        let json = match output {
            ToolOutput::Value(v) => v,
            other => panic!("期望 Value 结果: {other:?}"),
        };
        assert_eq!(json["total_count"], serde_json::json!(1));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `!` 前缀排除：命中排除规则的文件不出现在结果中，其余照常收录
    #[tokio::test]
    async fn glob_bang_prefix_excludes_matches() {
        let dir = std::env::temp_dir().join("fuyao_test_glob_bang_exclude");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("笔记.md"), "内容").unwrap();
        std::fs::write(dir.join("调试.log"), "内容").unwrap();

        let args = serde_json::json!({
            "pattern": "!*.log",
            "path": dir.to_string_lossy().to_string(),
        });
        let output = glob_impl(args, ToolCallContext::default(), CancellationToken::new()).await;
        let json = match output {
            ToolOutput::Value(v) => v,
            other => panic!("期望 Value 结果: {other:?}"),
        };

        assert_eq!(json["total_count"], serde_json::json!(1));
        let matched = json["matches"][0]["path"].as_str().unwrap();
        assert!(matched.ends_with("笔记.md"), "实际：{matched}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 花括号展开：`*.{md,txt}` 同时命中两种扩展名
    #[tokio::test]
    async fn glob_brace_expansion_matches_alternatives() {
        let dir = std::env::temp_dir().join("fuyao_test_glob_brace");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.md"), "内容").unwrap();
        std::fs::write(dir.join("b.txt"), "内容").unwrap();
        std::fs::write(dir.join("c.py"), "内容").unwrap();

        let args = serde_json::json!({
            "pattern": "*.{md,txt}",
            "path": dir.to_string_lossy().to_string(),
        });
        let output = glob_impl(args, ToolCallContext::default(), CancellationToken::new()).await;
        let json = match output {
            ToolOutput::Value(v) => v,
            other => panic!("期望 Value 结果: {other:?}"),
        };

        assert_eq!(json["total_count"], serde_json::json!(2));
        let matches_str = json["matches"].to_string();
        assert!(matches_str.contains("a.md"), "实际：{matches_str}");
        assert!(matches_str.contains("b.txt"), "实际：{matches_str}");
        assert!(!matches_str.contains("c.py"), "实际：{matches_str}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 搜索根是文件时：按文件名匹配，模式不命中则零结果
    #[tokio::test]
    async fn glob_file_root_matches_by_file_name() {
        let dir = std::env::temp_dir().join("fuyao_test_glob_file_root");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("笔记.md");
        std::fs::write(&file_path, "内容").unwrap();

        // 模式命中文件名
        let args = serde_json::json!({
            "pattern": "*.md",
            "path": file_path.to_string_lossy().to_string(),
        });
        let output = glob_impl(args, ToolCallContext::default(), CancellationToken::new()).await;
        let json = match output {
            ToolOutput::Value(v) => v,
            other => panic!("期望 Value 结果: {other:?}"),
        };
        assert_eq!(json["total_count"], serde_json::json!(1));

        // 模式不命中文件名：零结果而非误收录
        let args = serde_json::json!({
            "pattern": "*.txt",
            "path": file_path.to_string_lossy().to_string(),
        });
        let output = glob_impl(args, ToolCallContext::default(), CancellationToken::new()).await;
        let json = match output {
            ToolOutput::Value(v) => v,
            other => panic!("期望 Value 结果: {other:?}"),
        };
        assert_eq!(json["total_count"], serde_json::json!(0));

        std::fs::remove_dir_all(&dir).ok();
    }
}
