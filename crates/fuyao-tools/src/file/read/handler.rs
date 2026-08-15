//! 文件读取处理逻辑
//!
//! 提供文件读取功能，支持分页、行号显示、设备文件保护、相似文件名建议、
//! 外部编辑检测、循环检测、敏感信息脱敏。
//!
//! ## 文件读取
//!
//! 返回带行号的内容，使用 offset/limit 分页。行数无上限，字符预算在收集循环内
//! 增量执行（内存天然有界）。自动遵守以下安全规则：
//! - 设备文件（/dev/zero 等）→ 拒绝
//! - 框架内部路径（.fuyao/.env）→ 拒绝
//! - 二进制文件（.exe、.png 等）→ 拒绝
//! - 文件不存在 → 建议相似文件名
//! - 内容超出字符预算（> MAX_READ_CHARS 字节）→ 收集期自动截断并提示分段读取
//! - 读取结果自动脱敏 API Key 等敏感信息
//!
//! ## 目录读取
//!
//! 列出目录下的文件和子目录，按修改时间排序（目录优先）。
//! 自动跳过排除目录（.venv、node_modules、__pycache__ 等）。

use crate::common::resolve_path;
use crate::config::{MAX_READ_CHARS, SEARCH_EXCLUDE_DIRS};
use crate::file::helpers::suggest_similar_files;
use crate::file::read::types::{DirectoryEntry, DirectoryResult, ReadArgs, ReadResult};
use crate::file::safety::{has_binary_extension, is_blocked_device, is_internal_path};
use crate::file::tracker::record_read;
use crate::redact::redact_sensitive_text;
use fuyao_api::{CancellationToken, ToolCallContext, ToolError, ToolOutput, parse_args};
use serde_json::Value;
use std::io::BufRead;
use std::path::Path;

/// 列出目录内容
///
/// 遍历目录下的文件和子目录，返回 JSON 格式的结果。
/// 自动跳过排除目录（.venv、node_modules 等），按目录优先 + 名称排序。
///
/// # 参数
///
/// - `dir_path`: 目录的绝对路径
/// - `original_path`: 用户传入的原始路径（用于显示）
/// - `offset`: 起始索引（从 1 开始）
/// - `limit`: 最大返回条目数
///
/// # 返回
///
/// JSON 字符串，包含 entries 数组、total_count、truncated 等字段。
fn list_directory(dir_path: &Path, original_path: &str, offset: usize, limit: usize) -> ToolOutput {
    let mut entries: Vec<DirectoryEntry> = Vec::new();

    let read_dir = match std::fs::read_dir(dir_path) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return ToolOutput::error(format!("无权限访问目录: {original_path}"));
        }
        Err(e) => {
            return ToolOutput::error(format!(
                "读取目录失败: {}: {e}",
                std::any::type_name_of_val(&e)
                    .split("::")
                    .last()
                    .unwrap_or("Error")
            ));
        }
    };

    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();

        if SEARCH_EXCLUDE_DIRS.iter().any(|d| *d == name) {
            continue;
        }

        let file_type = entry.file_type();
        let is_dir = file_type.map(|t| t.is_dir()).unwrap_or(false);

        let size = if !is_dir {
            entry.metadata().ok().map(|m| m.len())
        } else {
            None
        };

        entries.push(DirectoryEntry {
            name,
            entry_type: if is_dir {
                "dir".to_string()
            } else {
                "file".to_string()
            },
            size,
        });
    }

    entries.sort_by(|a, b| {
        let a_is_file = a.entry_type == "file";
        let b_is_file = b.entry_type == "file";
        b_is_file
            .cmp(&a_is_file)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let total_count = entries.len();
    let start_idx = offset - 1;
    let end_idx = std::cmp::min(start_idx + limit, total_count);
    let selected: Vec<_> = entries
        .into_iter()
        .skip(start_idx)
        .take(end_idx - start_idx)
        .collect();

    let truncated = end_idx < total_count;
    let result = DirectoryResult {
        result: selected,
        path: original_path.to_string(),
        total_count,
        truncated: if truncated { Some(true) } else { None },
        hint: if truncated {
            Some(format!(
                "结果已截断（共 {total_count} 个条目）。建议使用 glob 按模式缩小范围"
            ))
        } else {
            None
        },
    };

    ToolOutput::ok(serde_json::to_value(result).unwrap_or_default())
}

/// 读取文件内容的核心实现
///
/// 处理完整的读取流程：路径解析 → 安全检查 → 读取 → 行号格式化 → 脱敏 → 返回。
///
/// # 安全检查顺序
///
/// 1. 设备文件检查（/dev/zero 等）
/// 2. 框架内部路径检查（.fuyao/.env）
/// 3. 文件存在性检查（不存在则建议相似文件名）
/// 4. 二进制文件检查
/// 5. 字符预算增量执行（收集期达到 MAX_READ_CHARS 即截断，不报错）
pub async fn read_file_impl(
    args: Value,
    ctx: ToolCallContext,
    _cancel: CancellationToken,
) -> ToolOutput {
    let ReadArgs {
        path,
        offset,
        limit,
    } = match parse_args(args) {
        Ok(a) => a,
        Err(e) => return ToolOutput::Err(e),
    };
    let offset = offset.max(1) as usize;
    // 行数不再设上限：内存与上下文均由收集期的字符预算兜底，此处只防零/负数
    let limit = limit.max(1) as usize;
    let task_id = ctx.task_id().to_string();
    let workspace = ctx.workspace().map(Path::to_path_buf);

    let resolved_path_obj = resolve_path(&path, workspace.as_deref());
    let resolved_path = resolved_path_obj.to_string_lossy().to_string();

    if is_blocked_device(&path) {
        return ToolOutput::error(format!(
            "无法读取设备文件: {path}。该文件会产生无限输出或阻塞输入。"
        ));
    }

    if is_internal_path(&path) {
        return ToolOutput::error(format!(
            "拒绝读取框架内部路径: {path}。此路径包含框架内部数据，不允许直接访问。"
        ));
    }

    if !resolved_path_obj.exists() {
        let suggestions = suggest_similar_files(&path, 5);
        let mut error_msg = format!("文件不存在: {path}");
        if !suggestions.is_empty() {
            error_msg.push_str("\n\n您是否想要以下文件之一？\n");
            for s in &suggestions {
                error_msg.push_str(&format!("  • {s}\n"));
            }
        }
        let mut err = ToolError::new(error_msg).with("path", path.as_str());
        if !suggestions.is_empty() {
            err = err.with("suggestions", serde_json::json!(suggestions));
        }
        return ToolOutput::Err(err);
    }

    if resolved_path_obj.is_dir() {
        return list_directory(&resolved_path_obj, &path, offset, limit);
    }

    if !resolved_path_obj.is_file() {
        return ToolOutput::error(format!("路径不是文件或目录: {path}"));
    }

    if has_binary_extension(&resolved_path) {
        let ext = resolved_path_obj
            .extension()
            .unwrap_or_default()
            .to_string_lossy();
        return ToolOutput::error(format!("无法读取二进制文件: {path} ({ext})"));
    }

    record_read(&resolved_path, &task_id);

    let file_size = match resolved_path_obj.metadata() {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return ToolOutput::error(format!("无权限读取文件: {path}"));
        }
        Err(_) => 0,
    };

    // 流式逐行读取：只为请求窗口内的行解码分配，窗口外的行仅计数（total_lines 需要
    // 全量行数）。旧实现整文件 read_to_string + 全行收集后再切片，读大日志文件的
    // 几百行也会把整个文件搬进内存
    let file = match std::fs::File::open(&resolved_path_obj) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return ToolOutput::error(format!("无权限读取文件: {path}"));
        }
        Err(e) => {
            return ToolOutput::error(format!("读取文件失败: {e}"));
        }
    };
    let mut reader = std::io::BufReader::new(file);

    let mut raw_line: Vec<u8> = Vec::new();
    let mut line_no: usize = 0;
    // 预分配有界：limit 已无上限，不能按 limit 预分配，按小窗口起步即可
    let mut content_lines: Vec<String> = Vec::with_capacity(limit.min(1024));
    // 已收集内容的字节累计（含换行分隔符）。字符预算的执行变量：
    // 计量口径与 String::len 一致（UTF-8 字节数），作为上下文成本的代理而非精确字符数
    let mut collected_bytes: usize = 0;
    // 是否因字符预算提前停止收集（区别于行窗口收满 / EOF）
    let mut char_capped = false;
    loop {
        raw_line.clear();
        match reader.read_until(b'\n', &mut raw_line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                return ToolOutput::error(format!("读取文件失败: {e}"));
            }
        }
        line_no += 1;
        // 只解码并格式化窗口内的行；窗口外（或已达任一边界后）仅推进行号计数，不解码不存储
        if char_capped || line_no < offset || content_lines.len() >= limit {
            continue;
        }
        let line = match std::str::from_utf8(&raw_line) {
            Ok(s) => s.trim_end_matches(['\n', '\r']),
            // 区分 UTF-8 编码错误（InvalidData）和其他 IO 错误：编码错误给用户明确的修复提示
            Err(_) => {
                return ToolOutput::error(format!(
                    "文件编码无法解析: {path}。文件可能包含非 UTF-8 字节，请用二进制编辑器查看。"
                ));
            }
        };
        let formatted = format!("{:>6}\t{line}", line_no);
        // 行成本 = 格式化行字节长 + 1（换行分隔符），先到先停的第二个边界
        let line_cost = formatted.len() + 1;
        if collected_bytes + line_cost > MAX_READ_CHARS {
            if content_lines.is_empty() {
                // 窗口内首行即单独超预算（如压缩产物的单行文件）：
                // 截断到预算内的 UTF-8 安全边界并加省略标记，保证至少有内容可看
                let budget = MAX_READ_CHARS.saturating_sub('…'.len_utf8());
                content_lines.push(format!(
                    "{}…",
                    truncate_at_char_boundary(&formatted, budget)
                ));
            }
            // 触达字符预算不报错：保留已收集内容，后续行只计数不解码
            char_capped = true;
            continue;
        }
        collected_bytes += line_cost;
        content_lines.push(formatted);
    }
    let total_lines = line_no;
    // 实际收集窗口的末行行号（窗口从 offset 起逐行连续收集，故等于 offset-1+收集数）
    let last_collected = offset.saturating_sub(1) + content_lines.len();

    let output = content_lines.join("\n");
    let output = redact_sensitive_text(&output);
    // 截断判定：实际收集窗口末行未到文件末尾，或字符预算在中途/行内触发
    let truncated = char_capped || last_collected < total_lines;

    let result = ReadResult {
        result: output,
        path: path.to_string(),
        total_lines,
        file_size,
        offset,
        limit: content_lines.len(),
        truncated: if truncated { Some(true) } else { None },
        hint: if truncated {
            if char_capped {
                Some(format!(
                    "已达字符上限（{} 字节），内容已截断。建议减小 limit 分段读取，或使用 offset={} 继续读取",
                    MAX_READ_CHARS,
                    last_collected + 1
                ))
            } else {
                Some(format!(
                    "使用 offset={} 继续读取（显示第 {offset}-{last_collected} 行，共 {total_lines} 行）",
                    last_collected + 1
                ))
            }
        } else {
            None
        },
    };

    ToolOutput::ok(serde_json::to_value(result).unwrap_or_default())
}

/// 将字符串按字节预算截断到 UTF-8 字符边界
///
/// 从预算位置向后回退（至多 3 字节，UTF-8 最长序列的续字节长度）直到落在
/// 合法字符边界，保证不切断多字节字符。预算不小于字符串长度时原样返回。
fn truncate_at_char_boundary(s: &str, budget: usize) -> &str {
    if budget >= s.len() {
        return s;
    }
    let mut end = budget;
    // UTF-8 续字节形如 10xxxxxx，回退到非续字节位置即字符边界
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 执行读取并把 wire JSON 解析为 Value，便于按字段断言
    async fn run_read(args: serde_json::Value) -> serde_json::Value {
        let wire = read_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        serde_json::from_str(&wire).expect("wire 应为合法 JSON")
    }

    /// 截断辅助：字节预算落在多字节字符内部时回退到字符边界
    #[test]
    fn truncate_at_char_boundary_cuts_safely() {
        let s = "abc汉def"; // 汉占 3 字节，位于字节 3..6
        assert_eq!(truncate_at_char_boundary(s, 4), "abc");
        assert_eq!(truncate_at_char_boundary(s, 6), "abc汉");
        assert_eq!(truncate_at_char_boundary(s, 0), "");
        assert_eq!(truncate_at_char_boundary(s, 100), s);
        assert_eq!(truncate_at_char_boundary("", 5), "");
    }

    /// 超大 limit 读多行长文件：不报错，内容不超字符预算，截断并提示续读起点
    #[tokio::test]
    async fn read_huge_limit_multiline_file() {
        let dir = std::env::temp_dir().join("fuyao_test_read_huge_limit");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("big.txt");
        // 6000 行 × 每行 30 字节内容（格式化后每行成本 38 字节），总量远超预算。
        // 行内容含空格分隔的词组，贴近真实文本形状
        let content: String = (1..=6000)
            .map(|i| format!("data row {i:06} alpha beta gamma delta\n", i = i))
            .collect();
        std::fs::write(&file_path, content).unwrap();

        let parsed = run_read(serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "limit": 1_000_000
        }))
        .await;

        assert!(
            parsed.get("error").is_none(),
            "超大 limit 不应报错：{parsed}"
        );
        let result = parsed["result"].as_str().unwrap();
        // 每行成本 46 字节（38 字节内容 + 7 字节行号前缀 + 1 换行）：
        // 100000/46 = 2173 行（余 42 字节），第 2174 行触顶停止
        assert!(result.contains("data row 000001"), "首行应被收集");
        assert!(result.contains("data row 002173"), "预算内末行应被收集");
        assert!(
            !result.contains("data row 002174"),
            "触顶后的行不应进入结果"
        );
        assert!(
            result.len() <= MAX_READ_CHARS,
            "结果不应超过字符预算：{}",
            result.len()
        );
        assert_eq!(parsed["total_lines"], 6000);
        assert_eq!(parsed["limit"], 2173, "limit 字段应为实际收集行数");
        assert_eq!(parsed["truncated"], true);
        let hint = parsed["hint"].as_str().unwrap();
        assert!(hint.contains("字符上限"), "提示应说明字符上限：{hint}");
        assert!(hint.contains("offset=2174"), "提示应给出续读起点：{hint}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 长行文件中途触达字符预算：前几行完整收集，到预算即停、不含触发行
    #[tokio::test]
    async fn read_long_lines_stop_at_char_budget() {
        let dir = std::env::temp_dir().join("fuyao_test_read_long_lines");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("long.txt");
        // 5 行 × 30000 字节/行（格式化后每行成本 30007）：
        // 3 行累计 90021，第 4 行达 120028 超预算 → 收满 3 行停止
        let long_line = "x".repeat(29_999);
        let content = format!("{long_line}\n{long_line}\n{long_line}\n{long_line}\n{long_line}\n");
        std::fs::write(&file_path, content).unwrap();

        let parsed = run_read(serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "limit": 10
        }))
        .await;

        assert!(parsed.get("error").is_none(), "触达预算不应报错：{parsed}");
        let result = parsed["result"].as_str().unwrap();
        // 前三行完整收集（含完整长行内容），第四行整体不进入结果
        assert!(result.contains(&long_line), "已收集的长行内容应完整保留");
        assert!(result.contains("\n     2\t"));
        assert!(result.contains("\n     3\t"));
        assert!(!result.contains("\n     4\t"), "触发行的下一行不应进入结果");
        assert!(result.len() <= MAX_READ_CHARS);
        assert_eq!(parsed["total_lines"], 5);
        assert_eq!(parsed["limit"], 3);
        assert_eq!(parsed["truncated"], true);
        let hint = parsed["hint"].as_str().unwrap();
        assert!(
            hint.contains("offset=4"),
            "续读起点应为实际收集末行 + 1：{hint}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 单行超预算（含中文，验证 UTF-8 截断边界安全）：截断加省略标记、无 panic
    #[tokio::test]
    async fn read_single_line_over_budget_truncated_safely() {
        let dir = std::env::temp_dir().join("fuyao_test_read_single_huge_line");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("minified.txt");
        // 构造 >100k 字节的单行：ASCII 段 + 中文段（3 字节/字），
        // 使预算切点恰好落在某个汉字内部，验证回退到字符边界不切断多字节字符
        let line = format!("{}{}", "a".repeat(33_329), "汉".repeat(22_300));
        assert!(line.len() > MAX_READ_CHARS, "测试前提：单行自身超预算");
        let content = format!("{line}\ntail\n");
        std::fs::write(&file_path, content).unwrap();

        let parsed = run_read(serde_json::json!({
            "path": file_path.to_string_lossy().to_string()
        }))
        .await;

        assert!(
            parsed.get("error").is_none(),
            "单行超预算不应报错：{parsed}"
        );
        let result = parsed["result"].as_str().unwrap();
        assert!(
            result.len() <= MAX_READ_CHARS,
            "截断后不应超预算：{}",
            result.len()
        );
        assert!(result.starts_with("     1\t"), "截断行应保留行号前缀");
        assert!(result.ends_with('…'), "截断行应以省略标记结尾");
        // 省略号前应是完整的汉字（而非被切断的半个字符）
        assert_eq!(
            result.chars().rev().nth(1),
            Some('汉'),
            "截断点应落在字符边界"
        );
        assert!(!result.contains("tail"), "后续行不应进入结果");
        assert_eq!(parsed["total_lines"], 2);
        assert_eq!(parsed["limit"], 1);
        assert_eq!(parsed["truncated"], true);
        let hint = parsed["hint"].as_str().unwrap();
        assert!(hint.contains("字符上限"), "提示应说明字符上限：{hint}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_existing_file() {
        let dir = std::env::temp_dir().join("fuyao_test_read_full");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string()
        });
        let result = read_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
        assert!(result.contains("line3"));
        assert!(result.contains("total_lines"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let dir = std::env::temp_dir().join("fuyao_test_read_offset_full");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "line1\nline2\nline3\nline4\nline5\n").unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "offset": 2,
            "limit": 2
        });
        let result = read_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("line2"));
        assert!(result.contains("line3"));
        assert!(!result.contains("line1"));
        assert!(!result.contains("line4"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 末行无换行符：仍是完整一行，计入 total_lines
    #[tokio::test]
    async fn read_file_without_trailing_newline() {
        let dir = std::env::temp_dir().join("fuyao_test_read_no_trailing_nl");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "line1\nline2").unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string()
        });
        let result = read_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("line1"));
        assert!(result.contains("line2"));
        assert!(result.contains("\"total_lines\":2"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// CRLF 行尾：\r\n 不带入行内容
    #[tokio::test]
    async fn read_file_crlf_lines() {
        let dir = std::env::temp_dir().join("fuyao_test_read_crlf");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "line1\r\nline2\r\n").unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "limit": 1
        });
        let result = read_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("line1"));
        assert!(!result.contains("line2"));
        assert!(result.contains("\"total_lines\":2"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// offset 超出文件末尾：空结果、无截断提示
    #[tokio::test]
    async fn read_offset_beyond_eof() {
        let dir = std::env::temp_dir().join("fuyao_test_read_offset_eof");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "line1\nline2\n").unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "offset": 10,
            "limit": 5
        });
        let result = read_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("\"total_lines\":2"));
        assert!(!result.contains("truncated"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 大文件小窗口：行号正确，窗口外不进入结果
    #[tokio::test]
    async fn read_large_file_small_window() {
        let dir = std::env::temp_dir().join("fuyao_test_read_large_window");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("big.txt");
        let content: String = (1..=10000).map(|i| format!("row-{i}\n")).collect();
        std::fs::write(&file_path, content).unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "offset": 9990,
            "limit": 3
        });
        let result = read_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("row-9990"));
        assert!(result.contains("row-9992"));
        assert!(!result.contains("row-9989"));
        assert!(!result.contains("row-9993"));
        assert!(result.contains("\"total_lines\":10000"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 非 UTF-8 字节位于窗口外的行：只读取窗口时不受影响（流式只解码窗口内行）
    #[tokio::test]
    async fn read_tolerates_bad_utf8_outside_window() {
        let dir = std::env::temp_dir().join("fuyao_test_read_bad_utf8_outside");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        let mut content = Vec::new();
        content.extend_from_slice(b"good line\n");
        content.extend_from_slice(&[0xff, 0xfe, b'\n']); // 窗口外的坏字节行
        std::fs::write(&file_path, content).unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "offset": 1,
            "limit": 1
        });
        let result = read_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("good line"));
        assert!(!result.contains("编码无法解析"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 非 UTF-8 字节位于窗口内的行：明确报编码错误
    #[tokio::test]
    async fn read_rejects_bad_utf8_inside_window() {
        let dir = std::env::temp_dir().join("fuyao_test_read_bad_utf8_inside");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, b"\xff\xfe\ngood\n").unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string(),
            "offset": 1,
            "limit": 1
        });
        let result = read_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("编码无法解析"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_nonexistent_file() {
        let args = serde_json::json!({ "path": "/nonexistent/file.txt" });
        let result = read_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("文件不存在"));
    }

    #[tokio::test]
    async fn read_binary_file_rejected() {
        let dir = std::env::temp_dir().join("fuyao_test_read_binary_full");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.png");
        std::fs::write(&file_path, b"\x89PNG").unwrap();

        let args = serde_json::json!({
            "path": file_path.to_string_lossy().to_string()
        });
        let result = read_file_impl(
            args,
            fuyao_api::ToolCallContext::default(),
            fuyao_api::CancellationToken::new(),
        )
        .await
        .to_wire();
        assert!(result.contains("二进制文件"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_directory_contents() {
        let dir = std::env::temp_dir().join("fuyao_test_read_dir_full");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(dir.join("subdir")).unwrap();

        let result = list_directory(&dir, &dir.to_string_lossy(), 1, 100).to_wire();
        assert!(result.contains("a.txt"));
        assert!(result.contains("subdir"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
