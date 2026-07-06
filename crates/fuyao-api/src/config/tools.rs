//! 工具系统配置（开关 + 运行器 + 高频限制）
//!
//! - `enabled`：原 `FuyaoConfig.tools` 的扁平开关迁移至此子表
//! - `runner`：引用 `agent::ToolRunnerConfig`（并发策略，定义在 agent 模块）
//! - `limits`：迁移自 `fuyao-tools/src/config.rs` 的高频可调项
//!
//! 非高频项（`MAX_READ_CHARS`、各类缓存容量、`REDACT_SECRETS`、`SEARCH_EXCLUDE_DIRS`、
//! `WEBFETCH_USER_AGENT`、`WEBFETCH_CACHE_*` 等）保持 const，不纳入。

use std::collections::HashMap;

use serde::Deserialize;

use crate::agent::ToolRunnerConfig;

/// 工具系统高频可调限制（迁移自 `fuyao-tools/src/config.rs` 高频项）
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ToolsLimitsConfig {
    /// 搜索命令超时（秒），原 `SEARCH_TIMEOUT=60`
    pub search_timeout_secs: u64,
    /// 默认终端超时（秒），原 `TERMINAL_DEFAULT_TIMEOUT=120`
    pub terminal_default_timeout_secs: u64,
    /// 最大终端超时（秒），原 `TERMINAL_MAX_TIMEOUT=6000`
    pub terminal_max_timeout_secs: u64,
    /// 最大终端输出字符数，原 `TERMINAL_MAX_OUTPUT_CHARS=50000`
    pub terminal_max_output_chars: usize,
    /// 默认 WebFetch 超时（秒），原 `WEBFETCH_DEFAULT_TIMEOUT=30`
    pub webfetch_default_timeout_secs: u64,
    /// 最大 WebFetch 超时（秒），原 `WEBFETCH_MAX_TIMEOUT=180`
    pub webfetch_max_timeout_secs: u64,
    /// 最大 WebFetch 输出字符数，原 `WEBFETCH_MAX_OUTPUT_CHARS=100000`
    pub webfetch_max_output_chars: usize,
    /// 最大 WebFetch 下载字节数（5MB），原 `WEBFETCH_MAX_DOWNLOAD_BYTES=5242880`
    pub webfetch_max_download_bytes: usize,
}

impl Default for ToolsLimitsConfig {
    fn default() -> Self {
        Self {
            search_timeout_secs: 60,
            terminal_default_timeout_secs: 120,
            terminal_max_timeout_secs: 6000,
            terminal_max_output_chars: 50_000,
            webfetch_default_timeout_secs: 30,
            webfetch_max_timeout_secs: 180,
            webfetch_max_output_chars: 100_000,
            webfetch_max_download_bytes: 5 * 1024 * 1024,
        }
    }
}

/// 工具系统聚合配置
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ToolsConfig {
    /// 工具开关，key=工具名，value=是否启用。未列出的工具默认启用。
    /// 原 `FuyaoConfig.tools` 的扁平 `Map<String,bool>` 迁移至此子表 `[tools.enabled]`。
    pub enabled: HashMap<String, bool>,
    /// 工具运行器配置（并发策略），对应 TOML `[tools.runner]`
    pub runner: ToolRunnerConfig,
    /// 高频可调限制，对应 TOML `[tools.limits]`
    pub limits: ToolsLimitsConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_limits_config_defaults_match_hardcoded() {
        let c = ToolsLimitsConfig::default();
        assert_eq!(c.search_timeout_secs, 60);
        assert_eq!(c.terminal_default_timeout_secs, 120);
        assert_eq!(c.terminal_max_timeout_secs, 6000);
        assert_eq!(c.terminal_max_output_chars, 50_000);
        assert_eq!(c.webfetch_default_timeout_secs, 30);
        assert_eq!(c.webfetch_max_timeout_secs, 180);
        assert_eq!(c.webfetch_max_output_chars, 100_000);
        assert_eq!(c.webfetch_max_download_bytes, 5 * 1024 * 1024);
    }

    #[test]
    fn deserialize_tools_enabled_and_limits() {
        let toml_str = r#"
[tools.enabled]
bash = false
grep = true
[tools.limits]
terminal_default_timeout_secs = 240
"#;
        #[derive(Deserialize)]
        struct Wrap {
            tools: ToolsConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.tools.enabled.get("bash"), Some(&false));
        assert_eq!(w.tools.enabled.get("grep"), Some(&true));
        assert_eq!(w.tools.limits.terminal_default_timeout_secs, 240);
        // 缺省
        assert_eq!(w.tools.limits.search_timeout_secs, 60);
        assert_eq!(w.tools.runner.max_concurrent, 8);
    }

    /// tools.runner 反序列化（验证 TOML 短键名 never_parallel 等映射正确）
    #[test]
    fn deserialize_tools_runner_short_keys() {
        let toml_str = r#"
[tools.runner]
max_concurrent = 16
never_parallel = ["bash", "todowrite", "edit"]
parallel_safe = ["glob"]
path_scoped = ["read", "write"]
"#;
        #[derive(Deserialize)]
        struct Wrap {
            tools: ToolsConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.tools.runner.max_concurrent, 16);
        assert!(w.tools.runner.never_parallel_tools.contains("edit"));
        assert!(w.tools.runner.parallel_safe_tools.contains("glob"));
        assert!(w.tools.runner.path_scoped_tools.contains("read"));
    }
}
