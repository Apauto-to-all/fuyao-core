//! 全局配置句柄集成测试：get_config 未 set 的兜底契约
//!
//! `get_config` 未 set 时返回 `FuyaoConfig::default()`，绝不 panic。
//! 这是规避初始化时序风险的关键兜底，也是「不 set 的测试天然隔离」的基石。
//!
//! 注意：本二进制**绝不调用 `set_config`**——`set_config` 基于 `OnceLock`，
//! 进程内只能 set 一次。`set_config` 的 panic 语义在 `set_config_panic_test.rs`
//! 独立二进制中验证，避免污染本进程的 OnceLock 状态。

use fuyao_api::{FuyaoConfig, get_config};

#[test]
fn get_config_returns_default_when_not_set() {
    // 本进程未调用 set_config（见文件头说明），应返回 default
    let config = get_config();
    // 钉死：返回的是有效 default，不 panic
    let default = FuyaoConfig::default();
    assert_eq!(
        config.llm.request_timeout_secs,
        default.llm.request_timeout_secs
    );
    assert_eq!(config.logging.level, default.logging.level);
}

#[test]
fn get_config_never_panics_even_in_isolated_process() {
    // 连续多次调用都应安全（兜底契约）
    let _a = get_config();
    let _b = get_config();
    let _c = get_config();
    // 若到达此处即证明不 panic
}
