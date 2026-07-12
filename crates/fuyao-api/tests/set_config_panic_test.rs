//! 全局配置句柄集成测试：set_config 重复调用 panic 语义
//!
//! `set_config` 基于 `OnceLock`，进程内只能成功一次。重复调用是编程错误，直接 panic。
//! 本二进制故意触发一次成功的 set 后，再触发第二次 set 验证 panic。
//!
//! 独立成文件的原因：一旦本进程成功 `set_config`，OnceLock 即被占用，
//! 会污染任何依赖「未 set」状态的测试。故与 `global_state_test.rs` 物理隔离。

use std::sync::Arc;

use fuyao_api::{FuyaoConfig, set_config};

#[test]
fn set_config_twice_panics() {
    // 第一次 set 应成功（本二进制内首次调用）
    set_config(Arc::new(FuyaoConfig::default()));

    // 第二次 set 应 panic，消息含「set_config 重复调用」
    let result = std::panic::catch_unwind(|| {
        set_config(Arc::new(FuyaoConfig::default()));
    });
    assert!(result.is_err(), "第二次 set_config 必须 panic");

    // 验证 panic 消息包含预期文案（钉死错误语义，便于调用方理解）
    let payload = result.unwrap_err();
    let msg = payload_downcast_string(&payload);
    assert!(
        msg.contains("set_config 重复调用"),
        "panic 消息应含 'set_config 重复调用'，实际：{msg}"
    );
}

/// 从 panic payload 提取字符串消息（兼容 &str / String / 其他类型）
fn payload_downcast_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<非字符串 panic payload>".to_string()
}
