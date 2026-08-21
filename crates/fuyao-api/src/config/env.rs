//! 环境变量加载
//!
//! 从三层路径加载 `.env` 文件到进程环境变量。与 TOML 配置的三层合并并行，
//! 同属配置加载范畴，故收入 config 模块作为单一加载入口的一部分。
//!
//! 加载顺序（低优先级 → 高优先级）：
//! 1. 全局路径：`~/.fuyao/.env`
//! 2. Agent 路径：`{agent_root}/.env`
//! 3. 工作区路径：`{workspace}/.env`
//!
//! 高优先级覆盖低优先级同名变量。

use crate::AgentPaths;

/// 从三层路径加载 `.env` 文件
///
/// 使用 `dotenvy` 解析 `.env` 文件，支持变量替换、引号等标准格式。
/// 任一层文件不存在时静默跳过，不报错。
pub fn load_env(agent_paths: &AgentPaths) {
    let paths = agent_paths.env_paths();
    // all() 返回 [workspace, agent, global]，反转后从 global 开始（低优先级→高优先级）
    for path in paths.all().into_iter().rev() {
        // dotenvy::from_path_override 解析 .env 并设置到 std::env，
        // 已存在的变量会被覆盖（高优先级覆盖低优先级）
        let _ = dotenvy::from_path_override(path);
    }
}

/// 三层 `.env` 的运行时增量补载（只补进程环境缺失的变量）
///
/// 启动后的管理面写回（供应商创建把 api_key 写进 global 层 `.env`）发生在
/// `load_env` 之后，新变量不会自动进进程环境——运行时注册前调本函数把三层
/// `.env` 的**缺失项**补进来，补齐「写盘 → 立即可用」链路的环境侧一环。
///
/// 只补缺失、不覆盖既有：进程内已存在的变量（用户手动 export、更高层
/// `.env` 已加载的值）优先级视为高于本函数，运行时补载不冲掉它们——
/// 与 [`load_env`] 的启动期全量覆盖语义刻意不同（启动期三层间靠覆盖定序，
/// 运行期既有值是更强来源）。
///
/// 任一层文件不存在 / 解析失败时静默跳过该层（补载是尽力而为的增量动作，
/// 缺失后果由 api_key 解析链的 fail-loud 报错兜底）。
pub fn load_env_missing(agent_paths: &AgentPaths) {
    let paths = agent_paths.env_paths();
    // 低优先级 → 高优先级逐层补载（先补 global 的缺失，再让高层的缺失项覆盖补齐）
    for path in paths.all().into_iter().rev() {
        let Ok(iter) = dotenvy::from_path_iter(path) else {
            continue;
        };
        for (key, value) in iter.flatten() {
            if std::env::var(&key).is_err() {
                // SAFETY: 补载发生在引擎装配层的同步注册路径；Rust 2024 将
                // set_var 标记 unsafe 是因为多线程下读写环境变量有数据竞争，
                // 此处仅在变量缺失时写入一次（is_err → set 的窗口内并发写同
                // 名变量最终值一致，均为 .env 声明值，无逻辑分叉）
                unsafe {
                    std::env::set_var(&key, &value);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_env_nonexistent_paths_no_panic() {
        let paths = AgentPaths {
            agent_id: Some("global/nonexistent_test".to_string()),
            workspace: None,
            ..Default::default()
        };
        // 全部不存在的路径不应 panic
        load_env(&paths);
    }

    /// 进程唯一的测试变量名（避免并行测试间环境变量串扰）
    fn unique_var(suffix: &str) -> String {
        format!("FUYAO_TEST_ENV_MISSING_{}_{suffix}", std::process::id())
    }

    /// 缺失的变量被补载，既有变量不被覆盖（运行时补载只补缺失）
    #[test]
    fn load_env_missing_fills_gap_without_overriding() {
        let home = tempfile::tempdir().unwrap();
        let missing = unique_var("gap");
        let existing = unique_var("keep");
        std::fs::write(
            home.path().join(".env"),
            format!("{missing}=from-file\n{existing}=from-file\n"),
        )
        .unwrap();
        let paths = AgentPaths {
            agent_id: None,
            workspace: None,
            extra_dirs: Vec::new(),
            fuyao_home: home.path().to_path_buf(),
        };

        // SAFETY: 变量名含进程 id，仅本测试进程使用，不与其他并行测试冲突
        unsafe {
            std::env::set_var(&existing, "in-process");
        }

        load_env_missing(&paths);

        // SAFETY: 同上，读取与清理仅涉及本测试的进程唯一变量
        unsafe {
            assert_eq!(
                std::env::var(&missing).as_deref(),
                Ok("from-file"),
                "缺失变量应从 .env 补载"
            );
            assert_eq!(
                std::env::var(&existing).as_deref(),
                Ok("in-process"),
                "既有变量不应被补载覆盖"
            );
            std::env::remove_var(&missing);
            std::env::remove_var(&existing);
        }
    }

    /// .env 文件不存在时补载静默跳过（不 panic、不设置任何变量）
    #[test]
    fn load_env_missing_absent_file_is_noop() {
        let home = tempfile::tempdir().unwrap();
        let paths = AgentPaths {
            agent_id: None,
            workspace: None,
            extra_dirs: Vec::new(),
            fuyao_home: home.path().to_path_buf(),
        };
        load_env_missing(&paths);
    }
}
