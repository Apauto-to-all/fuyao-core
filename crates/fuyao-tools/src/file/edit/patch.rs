//! V4A 补丁解析
//!
//! 解析 V4A 格式补丁，支持多文件编辑操作。
//!
//! ## V4A 格式
//!
//! ```text
//! *** Begin Patch
//! *** Update File: src/main.rs
//! @@ context_hint @@
//! -old line
//! +new line
//! *** Add File: new.txt
//! +hello world
//! *** Delete File: old.txt
//! *** Move File: src/a.rs -> src/b.rs
//! *** End Patch
//! ```
//!
//! ## 操作类型
//!
//! - **Update**: 修改现有文件，通过 hunk 上下文匹配替换位置
//! - **Add**: 创建新文件，内容从 + 行提取
//! - **Delete**: 删除文件
//! - **Move**: 移动/重命名文件

use regex::Regex;
use std::sync::LazyLock;

/// V4A 补丁解析用到的全部正则
///
/// 五条模式均为编译期常量字面量，聚合为进程级静态量只编译一次，
/// 避免每次 `parse_v4a_patch` 调用都重复编译。
struct PatchRegexes {
    update: Regex,
    add: Regex,
    delete: Regex,
    mv: Regex,
    hint: Regex,
}

static RES: LazyLock<PatchRegexes> = LazyLock::new(|| PatchRegexes {
    update: Regex::new(r"^\*\*\*\s*Update\s+File:\s*(.+)").expect("无效 Update 正则"),
    add: Regex::new(r"^\*\*\*\s*Add\s+File:\s*(.+)").expect("无效 Add 正则"),
    delete: Regex::new(r"^\*\*\*\s*Delete\s+File:\s*(.+)").expect("无效 Delete 正则"),
    mv: Regex::new(r"^\*\*\*\s*Move\s+File:\s*(.+?)\s*->\s*(.+)").expect("无效 Move 正则"),
    hint: Regex::new(r"^@@\s*(.+?)\s*@@").expect("无效 hint 正则"),
});

/// 补丁操作类型
#[derive(Debug, Clone, PartialEq)]
pub enum OperationType {
    Add,
    Update,
    Delete,
    Move,
}

/// 补丁行
#[derive(Debug, Clone)]
pub struct HunkLine {
    pub prefix: String,
    pub content: String,
}

/// 补丁块
#[derive(Debug, Clone)]
pub struct Hunk {
    pub context_hint: Option<String>,
    pub lines: Vec<HunkLine>,
}

/// 补丁操作
#[derive(Debug, Clone)]
pub struct PatchOperation {
    pub operation: OperationType,
    pub file_path: String,
    pub new_path: Option<String>,
    pub hunks: Vec<Hunk>,
}

/// 解析 V4A 格式补丁
///
/// 解析 `*** Begin Patch` / `*** End Patch` 之间的内容，
/// 识别 Update/Add/Delete/Move 操作和 hunk 块。
///
/// # 返回
///
/// `(操作列表, 错误信息)`。解析成功时错误为 None，失败时操作列表为空。
pub fn parse_v4a_patch(patch_content: &str) -> (Vec<PatchOperation>, Option<String>) {
    let lines: Vec<&str> = patch_content.split('\n').collect();
    let mut operations: Vec<PatchOperation> = Vec::new();

    let mut start_idx: Option<usize> = None;
    let mut end_idx: Option<usize> = None;

    for (i, line) in lines.iter().enumerate() {
        if line.contains("*** Begin Patch") || line.contains("***Begin Patch") {
            start_idx = Some(i);
        } else if line.contains("*** End Patch") || line.contains("***End Patch") {
            end_idx = Some(i);
            break;
        }
    }

    let start = match start_idx {
        Some(idx) => idx + 1,
        None => 0,
    };
    let end = match end_idx {
        Some(idx) => idx,
        None => lines.len(),
    };

    let mut current_op: Option<PatchOperation> = None;
    let mut current_hunk: Option<Hunk> = None;

    for line in lines.iter().take(end).skip(start) {
        if let Some(caps) = RES.update.captures(line) {
            if let Some(op) = current_op.take() {
                current_op = Some(finalize_op(op, &mut current_hunk));
                operations.push(current_op.take().unwrap());
            }
            current_op = Some(PatchOperation {
                operation: OperationType::Update,
                file_path: caps[1].trim().to_string(),
                new_path: None,
                hunks: Vec::new(),
            });
            current_hunk = None;
        } else if let Some(caps) = RES.add.captures(line) {
            if let Some(op) = current_op.take() {
                current_op = Some(finalize_op(op, &mut current_hunk));
                operations.push(current_op.take().unwrap());
            }
            current_op = Some(PatchOperation {
                operation: OperationType::Add,
                file_path: caps[1].trim().to_string(),
                new_path: None,
                hunks: Vec::new(),
            });
            current_hunk = Some(Hunk {
                context_hint: None,
                lines: Vec::new(),
            });
        } else if let Some(caps) = RES.delete.captures(line) {
            if let Some(op) = current_op.take() {
                current_op = Some(finalize_op(op, &mut current_hunk));
                operations.push(current_op.take().unwrap());
            }
            current_op = Some(PatchOperation {
                operation: OperationType::Delete,
                file_path: caps[1].trim().to_string(),
                new_path: None,
                hunks: Vec::new(),
            });
            operations.push(current_op.take().unwrap());
            current_op = None;
            current_hunk = None;
        } else if let Some(caps) = RES.mv.captures(line) {
            if let Some(op) = current_op.take() {
                current_op = Some(finalize_op(op, &mut current_hunk));
                operations.push(current_op.take().unwrap());
            }
            current_op = Some(PatchOperation {
                operation: OperationType::Move,
                file_path: caps[1].trim().to_string(),
                new_path: Some(caps[2].trim().to_string()),
                hunks: Vec::new(),
            });
            operations.push(current_op.take().unwrap());
            current_op = None;
            current_hunk = None;
        } else if line.starts_with("@@") {
            if current_op.is_some() {
                if let Some(hunk) = current_hunk.take()
                    && !hunk.lines.is_empty()
                    && let Some(op) = current_op.as_mut()
                {
                    op.hunks.push(hunk);
                }
                let hint = RES.hint.captures(line).map(|caps| caps[1].to_string());
                current_hunk = Some(Hunk {
                    context_hint: hint,
                    lines: Vec::new(),
                });
            }
        } else if current_op.is_some() && !line.is_empty() {
            if current_hunk.is_none() {
                current_hunk = Some(Hunk {
                    context_hint: None,
                    lines: Vec::new(),
                });
            }

            if let Some(hunk) = current_hunk.as_mut()
                && let Some(ch) = line.chars().next()
            {
                match ch {
                    '+' => hunk.lines.push(HunkLine {
                        prefix: "+".to_string(),
                        content: line[1..].to_string(),
                    }),
                    '-' => hunk.lines.push(HunkLine {
                        prefix: "-".to_string(),
                        content: line[1..].to_string(),
                    }),
                    ' ' => hunk.lines.push(HunkLine {
                        prefix: " ".to_string(),
                        content: line[1..].to_string(),
                    }),
                    '\\' => {}
                    _ => hunk.lines.push(HunkLine {
                        prefix: " ".to_string(),
                        content: line.to_string(),
                    }),
                }
            }
        }
    }

    if let Some(op) = current_op.take() {
        let op = finalize_op(op, &mut current_hunk);
        operations.push(op);
    }

    if operations.is_empty() {
        return (
            Vec::new(),
            Some(
                "补丁内容为空或未识别 V4A 格式（需要 *** Update File: 或 *** Add File: 标记）"
                    .to_string(),
            ),
        );
    }

    let mut parse_errors = Vec::new();
    for op in &operations {
        if op.file_path.is_empty() {
            parse_errors.push("操作缺少文件路径".to_string());
        }
        if op.operation == OperationType::Update && op.hunks.is_empty() {
            parse_errors.push(format!("UPDATE {}: 未找到修改内容", op.file_path));
        }
        if op.operation == OperationType::Move && op.new_path.is_none() {
            parse_errors.push(format!("MOVE {}: 缺少目标路径", op.file_path));
        }
    }

    if !parse_errors.is_empty() {
        return (
            Vec::new(),
            Some(format!("解析错误: {}", parse_errors.join("; "))),
        );
    }

    (operations, None)
}

/// 将当前 hunk 收集到操作中（如果 hunk 非空）
fn finalize_op(mut op: PatchOperation, current_hunk: &mut Option<Hunk>) -> PatchOperation {
    if let Some(hunk) = current_hunk.take()
        && !hunk.lines.is_empty()
    {
        op.hunks.push(hunk);
    }
    op
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_update_patch() {
        let patch = "*** Begin Patch\n*** Update File: src/main.rs\n@@ context @@\n-old line\n+new line\n*** End Patch";
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].operation, OperationType::Update);
        assert_eq!(ops[0].file_path, "src/main.rs");
        assert_eq!(ops[0].hunks.len(), 1);
        assert_eq!(ops[0].hunks[0].lines.len(), 2);
    }

    #[test]
    fn parse_add_patch() {
        let patch = "*** Begin Patch\n*** Add File: new.txt\n+hello world\n*** End Patch";
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].operation, OperationType::Add);
    }

    #[test]
    fn parse_delete_patch() {
        let patch = "*** Begin Patch\n*** Delete File: old.txt\n*** End Patch";
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].operation, OperationType::Delete);
    }

    #[test]
    fn parse_move_patch() {
        let patch = "*** Begin Patch\n*** Move File: old.txt -> new.txt\n*** End Patch";
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].operation, OperationType::Move);
        assert_eq!(ops[0].new_path.as_deref(), Some("new.txt"));
    }

    #[test]
    fn parse_empty_patch() {
        let (ops, err) = parse_v4a_patch("");
        assert!(ops.is_empty());
        assert!(err.is_some());
    }

    #[test]
    fn parse_multi_file_patch() {
        let patch = "*** Begin Patch\n*** Update File: a.rs\n-old\n+new\n*** Add File: b.rs\n+content\n*** End Patch";
        let (ops, err) = parse_v4a_patch(patch);
        assert!(err.is_none());
        assert_eq!(ops.len(), 2);
    }
}
