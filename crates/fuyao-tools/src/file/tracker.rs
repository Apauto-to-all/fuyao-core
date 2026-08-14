//! 文件追踪器
//!
//! 提供文件读取状态追踪功能，包含去重检测、外部编辑检测。
//!
//! ## 功能
//!
//! - **去重检测**: 检测文件是否自上次读取后未修改，避免重复读取浪费上下文
//! - **外部编辑检测**: 检测文件是否被外部进程修改，警告用户重新读取
//!
//! ## 实现
//!
//! 使用全局 `LazyLock<Mutex<HashMap>>` 存储，按 task_id 隔离。
//! 每个 task 维护独立的 dedup / read_timestamps 两组数据。
//! 数据容量有上限（DEDUP_CAP / READ_TIMESTAMPS_CAP），超出时淘汰条目。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::config::{DEDUP_CAP, READ_TIMESTAMPS_CAP};

struct TaskData {
    dedup: HashMap<(String, usize, usize), i64>,
    read_timestamps: HashMap<String, i64>,
}

impl TaskData {
    fn new() -> Self {
        Self {
            dedup: HashMap::new(),
            read_timestamps: HashMap::new(),
        }
    }

    /// 容量淘汰：HashMap 迭代序任意，超限时淘汰的是任意条目而非最旧条目，
    /// 去重 / 过期检测本就是尽力而为的提示语义，不要求精确 LRU
    fn cap(&mut self) {
        if self.dedup.len() > DEDUP_CAP {
            let excess = self.dedup.len() - DEDUP_CAP;
            let to_remove: Vec<_> = self.dedup.keys().take(excess).cloned().collect();
            for key in to_remove {
                self.dedup.remove(&key);
            }
        }

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

/// 检查文件去重
///
/// 如果文件自上次读取后未修改，返回去重提示。
pub fn check_dedup(
    resolved_path: &str,
    offset: usize,
    limit: usize,
    task_id: &str,
) -> Option<serde_json::Value> {
    let tracker = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
    let task_data = tracker.get(task_id)?;
    let key = (resolved_path.to_string(), offset, limit);
    let cached_mtime = task_data.dedup.get(&key)?;

    let current_mtime = get_mtime(resolved_path)?;
    if current_mtime == *cached_mtime {
        Some(serde_json::json!({
            "content": "文件自上次读取后未修改。之前读取的内容仍然有效，请参考之前的 read 结果，无需重新读取。",
            "path": resolved_path,
            "dedup": true,
        }))
    } else {
        None
    }
}

/// 记录文件读取操作
///
/// 更新去重缓存与时间戳两组数据。超出容量时自动淘汰条目。
pub fn record_read(resolved_path: &str, offset: usize, limit: usize, task_id: &str) {
    let mut tracker = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
    let task_data = tracker
        .entry(task_id.to_string())
        .or_insert_with(TaskData::new);

    if let Some(mtime) = get_mtime(resolved_path) {
        task_data
            .dedup
            .insert((resolved_path.to_string(), offset, limit), mtime);
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
    fn record_and_dedup() {
        let dir = std::env::temp_dir().join("fuyao_test_tracker");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let resolved = file_path.to_string_lossy().to_string();
        let task_id = "test_task_dedup";

        record_read(&resolved, 1, 500, task_id);

        let result = check_dedup(&resolved, 1, 500, task_id);
        assert!(result.is_some());
        assert!(result.unwrap()["dedup"].as_bool().unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn staleness_detection() {
        let dir = std::env::temp_dir().join("fuyao_test_staleness");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "original").unwrap();

        let resolved = file_path.to_string_lossy().to_string();
        let task_id = "test_task_staleness";

        record_read(&resolved, 1, 500, task_id);

        // File unchanged
        assert!(check_file_staleness(&resolved, task_id).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
