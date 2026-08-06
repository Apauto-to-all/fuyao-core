//! init_engine 多次调用幂等性测试
//!
//! 验证多 engine 装配场景：连续调 init_engine 不因 set_config 重复而 panic。
//!
//! 背景：全局配置是进程级 OnceLock 单例，set_config 第二次调用会 panic。
//! init_engine 内部用 is_config_set 守卫——首 engine set 后，后续 init 跳过 set，
//! 共享同一份配置。本测试钉死该幂等行为，防回归。
//!
//! init_engine 在无 Provider 配置时返回 NoProviderAvailable，但 set_config 在
//! Provider 检查之前执行——故两次调用都能触达 set_config 路径，验证幂等守卫生效。
//!
//! 独立成文件：set_config 一旦成功，OnceLock 占用会污染依赖「未 set」状态的测试。

mod common;

use std::path::PathBuf;

use common::temp_agent_paths;
use fuyao_api::EngineParams;
use fuyao_app::{InitError, init_engine};

/// 连续两次 init_engine：第一次 set_config，第二次必须跳过（幂等），不 panic
///
/// 两次都因无 Provider 返回 NoProviderAvailable，但关键的 set_config 路径
/// 在 Provider 检查前执行——若幂等守卫失效，第二次会在 set_config panic。
#[tokio::test]
async fn init_engine_twice_is_idempotent_no_panic() {
    let (paths, _home) = temp_agent_paths();
    let params = EngineParams {
        agent_paths: paths.clone(),
    };

    // 第一次 init：set_config 执行，最终 NoProvider（无配置）
    let first = init_engine(&params).await;
    assert!(
        matches!(first, Err(InitError::NoProviderAvailable)),
        "首次 init 无 Provider 应返 NoProviderAvailable，实际：{:?}",
        first.err()
    );

    // 第二次 init：幂等守卫必须跳过 set_config，不 panic，同样 NoProvider
    let second = init_engine(&params).await;
    assert!(
        matches!(second, Err(InitError::NoProviderAvailable)),
        "二次 init 应幂等不 panic，返 NoProviderAvailable，实际：{:?}",
        second.err()
    );
}

/// 多个不同 workspace 的 init_engine 连续调用：模拟多工作区多 engine 装配
///
/// 每个 workspace 有不同的 agent_paths（不同临时目录），但共享同一进程的
/// config 单例。验证任意次数的 init 都不 panic（多 engine 场景的核心保证）。
#[tokio::test]
async fn init_engine_many_workspaces_no_panic() {
    for i in 0..3 {
        let (mut paths, _home) = temp_agent_paths();
        // 每个 paths 用不同 workspace，模拟不同工作目录
        paths.workspace = Some(PathBuf::from("/tmp").join(format!("ws-{i}")));
        let params = EngineParams { agent_paths: paths };
        // 都 NoProvider（无配置），只验证不 panic
        let result = init_engine(&params).await;
        assert!(
            matches!(result, Err(InitError::NoProviderAvailable)),
            "第 {i} 次 init 应返 NoProvider，不 panic，实际：{:?}",
            result.err()
        );
    }
}
