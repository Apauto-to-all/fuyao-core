//! 文件读取处理逻辑
//!
//! 提供文件读取功能，支持分页、行号显示、设备文件保护、相似文件名建议、
//! 外部编辑检测、循环检测、敏感信息脱敏。
//!
//! ## 文件读取
//!
//! 返回带行号的内容，使用 offset/limit 分页。自动遵守以下安全规则：
//! - 设备文件（/dev/zero 等）→ 拒绝
//! - 框架内部路径（.fuyao/.env）→ 拒绝
//! - 二进制文件（.exe、.png 等）→ 拒绝
//! - 文件不存在 → 建议相似文件名
//! - 内容超限（> MAX_READ_CHARS 字符）→ 拒绝并提示分页
//! - 读取结果自动脱敏 API Key 等敏感信息
//!
//! ## 目录读取
//!
//! 列出目录下的文件和子目录，按修改时间排序（目录优先）。
//! 自动跳过排除目录（.venv、node_modules、__pycache__ 等）。

use crate::common::resolve_path;
use crate::config::{LARGE_FILE_HINT_BYTES, MAX_READ_CHARS, SEARCH_EXCLUDE_DIRS};
use crate::file::helpers::suggest_similar_files;
use crate::file::read::types::{DirectoryEntry, DirectoryResult, MAX_LIMIT, ReadArgs, ReadResult};
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
                "使用 offset={} 继续读取（显示第 {offset}-{end_idx} 个条目，共 {total_count} 个）",
                end_idx + 1
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
/// 5. 内容大小检查（> MAX_READ_CHARS 则拒绝）
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
    let limit = limit.clamp(1, MAX_LIMIT) as usize;
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
    let mut content_lines: Vec<String> = Vec::with_capacity(limit);
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
        // 只解码并格式化窗口内的行；窗口外仅推进行号
        if line_no >= offset && content_lines.len() < limit {
            let line = match std::str::from_utf8(&raw_line) {
                Ok(s) => s.trim_end_matches(['\n', '\r']),
                // 区分 UTF-8 编码错误（InvalidData）和其他 IO 错误：编码错误给用户明确的修复提示
                Err(_) => {
                    return ToolOutput::error(format!(
                        "文件编码无法解析: {path}。文件可能包含非 UTF-8 字节，请用二进制编辑器查看。"
                    ));
                }
            };
            content_lines.push(format!("{:>6}\t{line}", line_no));
        }
    }
    let total_lines = line_no;
    let start_idx = offset - 1;
    let end_idx = std::cmp::min(start_idx + limit, total_lines);

    let output = content_lines.join("\n");

    if output.len() > MAX_READ_CHARS {
        return ToolOutput::error(format!(
            "读取内容超过安全限制 ({} > {} 字符)。请使用 offset 和 limit 参数读取更小的范围。文件共 {} 行。",
            output.len(),
            MAX_READ_CHARS,
            total_lines
        ));
    }

    let output = redact_sensitive_text(&output);
    let truncated = end_idx < total_lines;

    let result = ReadResult {
        result: output,
        path: path.to_string(),
        total_lines,
        file_size,
        offset,
        limit,
        truncated: if truncated { Some(true) } else { None },
        hint: if truncated {
            Some(format!(
                "使用 offset={} 继续读取（显示第 {offset}-{end_idx} 行，共 {total_lines} 行）",
                end_idx + 1
            ))
        } else {
            None
        },
        _hint: if file_size > LARGE_FILE_HINT_BYTES && limit > 200 && truncated {
            Some(format!(
                "此文件较大 ({} 字节)。建议使用 offset 和 limit 只读取需要的部分，以节省上下文。",
                file_size
            ))
        } else {
            None
        },
    };

    ToolOutput::ok(serde_json::to_value(result).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

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
