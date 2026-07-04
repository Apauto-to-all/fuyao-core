//! 网页内容缓存
//!
//! 提供内存 LRU + TTL 缓存，避免重复抓取和转换网页内容。

use super::types::CacheEntry;
use crate::config::{WEBFETCH_CACHE_MAX_SIZE, WEBFETCH_CACHE_TTL};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// 全局缓存实例
static CACHE: LazyLock<std::sync::Mutex<std::collections::HashMap<String, CacheEntry>>> =
    LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// 生成缓存键
fn make_cache_key(url: &str, output_format: &str) -> String {
    format!("{url}:{output_format}")
}

/// 获取当前时间戳（秒）
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// 淘汰过期条目，保证缓存大小不超过上限
fn evict_if_needed(cache: &mut std::collections::HashMap<String, CacheEntry>) {
    // 先淘汰过期条目
    let now = now_secs();
    cache.retain(|_, entry| now.saturating_sub(entry.timestamp) < WEBFETCH_CACHE_TTL);

    // 如果仍然超限，按时间戳淘汰最旧的
    while cache.len() > WEBFETCH_CACHE_MAX_SIZE {
        let oldest_key = cache
            .iter()
            .min_by_key(|(_, entry)| entry.timestamp)
            .map(|(k, _)| k.clone());
        if let Some(key) = oldest_key {
            cache.remove(&key);
        } else {
            break;
        }
    }
}

/// 获取缓存内容
pub fn get_cached_content(url: &str, output_format: &str) -> Option<CacheEntry> {
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let key = make_cache_key(url, output_format);

    match cache.get(&key) {
        Some(entry) => {
            let now = now_secs();
            if now.saturating_sub(entry.timestamp) < WEBFETCH_CACHE_TTL {
                Some(entry.clone())
            } else {
                None
            }
        }
        None => None,
    }
}

/// 设置缓存内容
pub fn set_cached_content(
    url: &str,
    output_format: &str,
    content: String,
    content_type: String,
    status: u16,
) {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    evict_if_needed(&mut cache);

    let key = make_cache_key(url, output_format);
    cache.insert(
        key,
        CacheEntry {
            content,
            content_type,
            status,
            timestamp: now_secs(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：清空缓存
    fn clear_cache() {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.clear();
    }

    #[test]
    fn cache_set_and_get() {
        clear_cache();
        set_cached_content(
            "https://example.com",
            "markdown",
            "test content".to_string(),
            "text/html".to_string(),
            200,
        );

        let entry = get_cached_content("https://example.com", "markdown");
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.content, "test content");
        assert_eq!(entry.status, 200);
    }

    #[test]
    fn cache_miss() {
        clear_cache();
        let entry = get_cached_content("https://nonexistent.com", "markdown");
        assert!(entry.is_none());
    }

    #[test]
    fn cache_different_format() {
        clear_cache();
        set_cached_content(
            "https://example.com",
            "markdown",
            "md content".to_string(),
            "text/html".to_string(),
            200,
        );
        set_cached_content(
            "https://example.com",
            "html",
            "html content".to_string(),
            "text/html".to_string(),
            200,
        );

        let md_entry = get_cached_content("https://example.com", "markdown");
        let html_entry = get_cached_content("https://example.com", "html");
        assert_eq!(md_entry.unwrap().content, "md content");
        assert_eq!(html_entry.unwrap().content, "html content");
    }

    #[test]
    fn cache_clear() {
        clear_cache();
        set_cached_content(
            "https://example.com",
            "markdown",
            "content".to_string(),
            "text/html".to_string(),
            200,
        );
        clear_cache();
        let entry = get_cached_content("https://example.com", "markdown");
        assert!(entry.is_none());
    }
}
