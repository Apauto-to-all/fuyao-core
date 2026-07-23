//! 按文件路径粒度的编辑锁
//!
//! 防止同一文件被并发编辑导致「读-改-写」覆盖：agent 可能在一条消息里并行发起
//! 多个针对同一文件的 edit，若不加锁，第二个会基于第一个写入前的快照计算，
//! 写回时覆盖掉第一个的改动。
//!
//! ## 设计
//!
//! - 全局 `HashMap<PathBuf, Arc<Mutex<()>>>`，每个规范化路径对应一把互斥锁；
//! - 同文件串行（互斥），不同文件并行（各有独立锁，互不阻塞）；
//! - 锁仅在「读-改-写」临界区内持有，临界区结束立即释放；
//! - 中毒（PoisonError）时恢复并继续——编辑失败不应毒死整个锁导致后续全部死锁。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// 全局锁表：规范化路径 → 该路径的互斥锁句柄
///
/// 外层 Mutex 保护 HashMap 本身的并发读写，内层 Arc<Mutex<()>> 才是「文件粒度」的编辑锁。
static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn lock_table() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取（或创建）指定路径的编辑锁句柄
///
/// 借助外层 HashMap 的短暂持锁获取内层 Arc<Mutex>，随后立即释放外层锁。
/// 内层 Arc<Mutex> 被克隆返回，由调用方在临界区内持有。
fn get_lock(path: &Path) -> Arc<Mutex<()>> {
    let key = path.to_path_buf();
    let table = lock_table();
    // 外层锁仅保护 HashMap 查找/插入，持锁时间极短
    let mut guard = table.lock().unwrap_or_else(|p| p.into_inner()); // HashMap 中毒时恢复
    guard
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// 在文件锁保护下执行闭包
///
/// 同一路径的多次调用串行执行，保证「读-改-写」原子性。
/// 闭包内若 panic 导致锁中毒，后续调用会恢复（`into_inner`）而非死锁。
pub fn with_file_lock<R>(path: &Path, f: impl FnOnce() -> R) -> R {
    let lock = get_lock(path);
    let _guard = lock.lock().unwrap_or_else(|p| p.into_inner()); // 中毒恢复
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn same_path_serializes() {
        let dir = std::env::temp_dir().join("fuyao_test_filelock_serialize");
        std::fs::create_dir_all(&dir).unwrap();
        // 用 Arc 共享路径，每个线程克隆一份
        let path = Arc::new(dir.join("target.txt"));

        // 计数并发执行中的最大重叠数：若串行，overlap 永远 ≤ 1
        static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
        static MAX_OVERLAP: AtomicUsize = AtomicUsize::new(0);

        let run = |p: Arc<PathBuf>| {
            std::thread::spawn(move || {
                with_file_lock(&p, || {
                    let cur = IN_FLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
                    MAX_OVERLAP.fetch_max(cur, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                })
            })
        };

        let h1 = run(Arc::clone(&path));
        let h2 = run(Arc::clone(&path));
        let h3 = run(Arc::clone(&path));
        h1.join().unwrap();
        h2.join().unwrap();
        h3.join().unwrap();

        // 串行执行下，同时在临界区的数量不应超过 1
        assert_eq!(
            MAX_OVERLAP.load(Ordering::SeqCst),
            1,
            "同一路径应串行执行，最大重叠应为 1"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn different_paths_parallel() {
        let dir = std::env::temp_dir().join("fuyao_test_filelock_parallel");
        std::fs::create_dir_all(&dir).unwrap();
        let path_a = dir.join("a.txt");
        let path_b = dir.join("b.txt");

        let start = std::time::Instant::now();
        let h1 = std::thread::spawn(move || {
            with_file_lock(&path_a, || {
                std::thread::sleep(std::time::Duration::from_millis(50));
            })
        });
        let h2 = std::thread::spawn(move || {
            with_file_lock(&path_b, || {
                std::thread::sleep(std::time::Duration::from_millis(50));
            })
        });
        h1.join().unwrap();
        h2.join().unwrap();
        let elapsed = start.elapsed();

        // 不同文件并行：总耗时应明显小于串行的 100ms
        assert!(
            elapsed < std::time::Duration::from_millis(90),
            "不同路径应并行执行，实际耗时 {:?}",
            elapsed
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn returns_closure_result() {
        let path = Path::new("/tmp/fuyao_test_lock_result");
        let result: i32 = with_file_lock(path, || 42);
        assert_eq!(result, 42);
    }
}
