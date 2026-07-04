//! 处理回调步骤
//!
//! 执行引擎内部业务逻辑（如发送命令给 TurnExecutor）。

use super::ProcessCallback;

/// 执行处理回调
pub fn run(callback: ProcessCallback) {
    callback();
}
