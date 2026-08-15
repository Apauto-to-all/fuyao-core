//! 文件追踪器
//!
//! 提供文件读取状态追踪功能，包含外部编辑检测。
//!
//! ## 功能
//!
//! - **外部编辑检测**: 检测文件是否被外部进程修改，警告用户重新读取
//!
//! ## 实现
//!
//! 使用全局 `LazyLock<Mutex<HashMap>>` 存储，按 task_id 隔离。
//! 每个 task 维护独立的 read_timestamps 数据。
//! 数据容量有上限（READ_TIMESTAMPS_CAP），超出时淘汰条目。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::config::READ_TIMESTAMPS_CAP;

struct TaskData {
    read_timestamps: HashMap<String, i64>,
}

impl TaskData {
    fn new() -> Self {
        Self {
            read_timestamps: HashMap::new(),
        }
    }

    /// 容量淘汰：HashMap 迭代序任意，超限时淘汰的是任意条目而非最旧条目，
    /// 过期检测本就是尽力而为的提示语义，不要求精确 LRU
    fn cap(&mut self) {
        if self.read_timestamps.len() > READ_TIMESTAMPS_CAP {
            let excess = self.read_timestamps.len() - READ_TIMESTAMPS_CAP;
            let to_remove: Vec<_> = self.read_timestamps.keys().take(excess).cloned().collect();
            for key in to_remove {
                self.read_timestamps.remove(&key);
            }
        }
    }
}

static TRACKER: LazyLock<Mutex<HashMap<String, TaskData>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 记录文件读取操作
///
/// 更新读取时间戳，供外部编辑检测使用。超出容量时自动淘汰条目。
pub fn record_read(resolved_path: &str, task_id: &str) {
    let mut tracker = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
    let task_data = tracker
        .entry(task_id.to_string())
        .or_insert_with(TaskData::new);

    if let Some(mtime) = get_mtime(resolved_path) {
        task_data
            .read_timestamps
            .insert(resolved_path.to_string(), mtime);
    }

    task_data.cap();
}

/// 更新文件读取时间戳
pub fn update_read_timestamp(filepath: &str, task_id: &str) {
    let expanded = crate::common::expand_tilde(filepath);
    let resolved = expanded.to_string_lossy().to_string();
    let mtime = match get_mtime(&resolved) {
        Some(m) => m,
        None => return,
    };

    let mut tracker = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
    let task_data = tracker
        .entry(task_id.to_string())
        .or_insert_with(TaskData::new);
    task_data.read_timestamps.insert(resolved, mtime);
    task_data.cap();
}

/// 检查文件是否被外部编辑
///
/// 如果文件自上次读取后被修改，返回警告信息。
pub fn check_file_staleness(filepath: &str, task_id: &str) -> Option<String> {
    let expanded = crate::common::expand_tilde(filepath);
    let resolved = expanded.to_string_lossy().to_string();

    let read_mtime = {
        let tracker = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
        tracker
            .get(task_id)?
            .read_timestamps
            .get(&resolved)?
            .to_owned()
    };

    let current_mtime = get_mtime(&resolved)?;

    if current_mtime != read_mtime {
        Some(format!(
            "警告: {filepath} 自上次读取后已被修改（外部编辑或并发操作）。您之前读取的内容可能已过期，建议重新读取后再写入。"
        ))
    } else {
        None
    }
}

fn get_mtime(path: &str) -> Option<i64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staleness_detection() {
        let dir = std::env::temp_dir().join("fuyao_test_staleness");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "original").unwrap();

        let resolved = file_path.to_string_lossy().to_string();
        let task_id = "test_task_staleness";

        record_read(&resolved, task_id);

        // File unchanged
        assert!(check_file_staleness(&resolved, task_id).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
