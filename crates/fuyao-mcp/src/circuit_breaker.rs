//! MCP 熔断器
//!
//! 连续失败超过阈值时打开熔断器，阻止后续调用，冷却后自动恢复（half-open）。

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Instant;

/// 熔断器全局状态
struct BreakerState {
    /// 各 server 的连续错误计数
    error_counts: HashMap<String, u32>,
    /// 各 server 熔断器打开的时间点
    opened_at: HashMap<String, Instant>,
}

static BREAKER_STATE: LazyLock<Mutex<BreakerState>> = LazyLock::new(|| {
    Mutex::new(BreakerState {
        error_counts: HashMap::new(),
        opened_at: HashMap::new(),
    })
});

/// 从中毒的 Mutex 中恢复，或正常获取锁
fn recover_or_lock<T>(state: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}

/// 增加错误计数，达到阈值时打开熔断器
pub fn bump_error(server_name: &str) {
    let threshold = fuyao_api::get_config().mcp.circuit_breaker_threshold;
    let mut state = recover_or_lock(&BREAKER_STATE);
    let count = state
        .error_counts
        .entry(server_name.to_string())
        .or_insert(0);
    *count += 1;
    if *count >= threshold {
        // 仅在熔断器从关闭→打开的瞬间记日志，避免每次失败重复记
        if *count == threshold {
            tracing::warn!(
                name = %server_name,
                consecutive_failures = *count,
                "MCP server 熔断"
            );
        }
        state
            .opened_at
            .insert(server_name.to_string(), Instant::now());
    }
}

/// 重置错误计数，关闭熔断器
pub fn reset_error(server_name: &str) {
    let mut state = recover_or_lock(&BREAKER_STATE);
    state.error_counts.remove(server_name);
    state.opened_at.remove(server_name);
}

/// 检查熔断器状态
///
/// 返回 None 表示放行，返回 Some(msg) 表示熔断器已打开。
/// 冷却期过后自动放行（half-open）。
pub fn check_breaker(server_name: &str) -> Option<String> {
    let cfg = fuyao_api::get_config();
    let threshold = cfg.mcp.circuit_breaker_threshold;
    let cooldown = cfg.mcp.circuit_breaker_cooldown_secs;

    let state = recover_or_lock(&BREAKER_STATE);
    let count = state.error_counts.get(server_name).copied().unwrap_or(0);
    if count < threshold {
        return None;
    }

    let opened_at = state.opened_at.get(server_name)?;
    let age = opened_at.elapsed().as_secs();

    if age < cooldown {
        let remaining = cooldown.saturating_sub(age).max(1);
        Some(format!(
            "MCP server '{server_name}' is unreachable after {count} consecutive failures. Auto-retry available in ~{remaining}s."
        ))
    } else {
        // 冷却期过，half-open 放行
        None
    }
}

/// 重置所有熔断器状态（测试用）
#[cfg(test)]
pub(crate) fn reset_all() {
    let mut state = recover_or_lock(&BREAKER_STATE);
    state.error_counts.clear();
    state.opened_at.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全局测试锁，确保 circuit_breaker 测试串行运行
    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn setup() {
        reset_all();
    }

    /// 默认阈值（与 McpGlobalConfig::default().circuit_breaker_threshold 一致）
    const CIRCUIT_BREAKER_THRESHOLD: u32 = 3;
    /// 默认冷却秒（与 McpGlobalConfig::default().circuit_breaker_cooldown_secs 一致）
    const CIRCUIT_BREAKER_COOLDOWN_SEC: u64 = 60;

    #[test]
    fn bump_error_increments_count() {
        let _lock = recover_or_lock(&TEST_LOCK);
        setup();
        bump_error("server1");
        let state = recover_or_lock(&BREAKER_STATE);
        assert_eq!(state.error_counts.get("server1"), Some(&1));
    }

    #[test]
    fn bump_error_opens_breaker_at_threshold() {
        let _lock = recover_or_lock(&TEST_LOCK);
        setup();
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            bump_error("server1");
        }
        let state = recover_or_lock(&BREAKER_STATE);
        assert!(state.opened_at.contains_key("server1"));
    }

    #[test]
    fn bump_error_increments_beyond_threshold() {
        let _lock = recover_or_lock(&TEST_LOCK);
        setup();
        for _ in 0..5 {
            bump_error("server1");
        }
        let state = recover_or_lock(&BREAKER_STATE);
        assert_eq!(state.error_counts.get("server1"), Some(&5));
    }

    #[test]
    fn reset_error_clears_count_and_opened_at() {
        let _lock = recover_or_lock(&TEST_LOCK);
        setup();
        bump_error("server1");
        bump_error("server1");
        reset_error("server1");
        let state = recover_or_lock(&BREAKER_STATE);
        assert!(!state.error_counts.contains_key("server1"));
        assert!(!state.opened_at.contains_key("server1"));
    }

    #[test]
    fn check_breaker_returns_none_when_below_threshold() {
        let _lock = recover_or_lock(&TEST_LOCK);
        setup();
        assert!(check_breaker("server1").is_none());
    }

    #[test]
    fn check_breaker_returns_message_when_open() {
        let _lock = recover_or_lock(&TEST_LOCK);
        setup();
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            bump_error("server1");
        }
        let msg = check_breaker("server1");
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("unreachable"));
    }

    #[test]
    fn check_breaker_returns_none_after_cooldown() {
        let _lock = recover_or_lock(&TEST_LOCK);
        setup();
        {
            let mut state = recover_or_lock(&BREAKER_STATE);
            state
                .error_counts
                .insert("server1".to_string(), CIRCUIT_BREAKER_THRESHOLD);
            state.opened_at.insert(
                "server1".to_string(),
                Instant::now() - std::time::Duration::from_secs(CIRCUIT_BREAKER_COOLDOWN_SEC + 10),
            );
        }
        assert!(check_breaker("server1").is_none());
    }

    #[test]
    fn reset_all_clears_everything() {
        let _lock = recover_or_lock(&TEST_LOCK);
        setup();
        bump_error("server1");
        bump_error("server2");
        reset_all();
        let state = recover_or_lock(&BREAKER_STATE);
        assert!(state.error_counts.is_empty());
        assert!(state.opened_at.is_empty());
    }
}
