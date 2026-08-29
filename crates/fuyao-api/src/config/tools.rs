//! 工具系统配置（开关 + 运行器 + 高频限制 + 终端）
//!
//! - `enabled`：原 `FuyaoConfig.tools` 的扁平开关迁移至此子表
//! - `runner`：工具并发策略（`ToolRunnerConfig`，本模块定义）
//! - `limits`：迁移自 `fuyao-tools/src/config.rs` 的高频可调项
//! - `terminal`：终端工具行为（bash 工具的 shell 选择）
//!
//! 非高频项（`MAX_READ_CHARS`、各类缓存容量、`REDACT_SECRETS`、`SEARCH_EXCLUDE_DIRS`、
//! `WEBFETCH_USER_AGENT`、`WEBFETCH_CACHE_*` 等）保持 const，不纳入。

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

/// 工具系统高频可调限制（迁移自 `fuyao-tools/src/config.rs` 高频项）
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ToolsLimitsConfig {
    /// 搜索命令超时（秒），原 `SEARCH_TIMEOUT=60`
    pub search_timeout_secs: u64,
    /// 搜索工具（glob/grep）单次返回结果数的硬上限。
    /// limit 参数超出此值会被静默截断——防大值请求撑爆上下文，需要更大批量时由用户上调
    pub search_max_results: usize,
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
            search_max_results: 500,
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

/// 工具运行器配置
///
/// 定义工具执行策略：并行规则、最大并发数等。
///
/// 判断顺序：
/// 1. never_parallel_tools → 强制串行
/// 2. path_scoped_tools → 检查路径重叠，重叠则串行，否则跳过后续检查
/// 3. parallel_safe_tools → 在此列表则可并行，否则串行
///
/// TOML 短键名：为配置文件书写简洁，字段经 `#[serde(rename)]` 映射为
/// `never_parallel` / `parallel_safe` / `path_scoped`（见 `[tools.runner]`）。
/// 容器级 `#[serde(default)]` 使缺省字段回退到下方手动 `Default` impl 的硬编码值。
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct ToolRunnerConfig {
    /// 最大并发执行的工具数量
    pub max_concurrent: u32,

    /// 必须串行执行的工具（如需要用户交互）
    #[serde(rename = "never_parallel")]
    pub never_parallel_tools: HashSet<String>,

    /// 只读工具，无共享可变状态，可安全并行
    ///
    /// 注意：path_scoped_tools 中的工具会先做路径检查，检查通过后跳过此检查。
    /// 例如 read 同时在两个列表中，但只走 path_scoped_tools 检查路径重叠，
    /// 不会走到 parallel_safe_tools 的通用检查。
    #[serde(rename = "parallel_safe")]
    pub parallel_safe_tools: HashSet<String>,

    /// 文件工具，可并行但要检查路径是否重叠
    ///
    /// 注意：这些工具会先做特殊检查（路径重叠），检查通过后跳过 parallel_safe_tools 检查。
    /// 例如 read 在此列表中，会检查路径是否重叠，重叠则串行，不重叠则可并行。
    #[serde(rename = "path_scoped")]
    pub path_scoped_tools: HashSet<String>,
}

impl Default for ToolRunnerConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 8,
            never_parallel_tools: HashSet::from(["bash".to_string(), "todowrite".to_string()]),
            parallel_safe_tools: HashSet::from([
                "read".to_string(),
                "glob".to_string(),
                "grep".to_string(),
                "skill".to_string(),
                "webfetch".to_string(),
                "subagent".to_string(),
            ]),
            path_scoped_tools: HashSet::from([
                "read".to_string(),
                "write".to_string(),
                "edit".to_string(),
            ]),
        }
    }
}

/// 终端工具配置（bash 工具的 shell 选择），对应 TOML `[tools.terminal]`
///
/// `shell` 为字符串而非枚举：未知值在 serde 层放行（避免配置文件因一个字段整体解析失败），
/// 由引擎启动校验统一拦截报错（fail loud，见 fuyao-tools 的 shell 启动校验）。
///
/// 消费走字段链 `get_config().tools.terminal.shell`（shell 检测与启动期校验两处），
/// 调用方无需按名导入本类型——全仓按名引用为零是字段链消费的预期形态，
/// 不构成死代码判据。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// bash 工具使用的 shell：
    /// - `"auto"`（默认）：自动探测（Windows: Git Bash > PowerShell > cmd；Unix: bash > sh）
    /// - 显式名（与 shell_type 词表同名）：`git_bash` / `powershell` / `cmd` / `bash` / `sh`，
    ///   按名定位二进制；名字非法或二进制不存在时引擎启动期报错拒绝启动
    pub shell: String,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            shell: "auto".to_string(),
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
    /// 终端工具配置（bash 的 shell 选择），对应 TOML `[tools.terminal]`
    pub terminal: TerminalConfig,
}

impl ToolsConfig {
    /// 判断指定工具是否被显式禁用
    ///
    /// 统一工具开关判定逻辑（供注册漏斗 `register_tool` 调用，覆盖所有工具来源）：
    /// - 显式 `false`：返回 `true`（被禁用）
    /// - 未列出或显式 `true`：返回 `false`（默认启用）
    ///
    /// 抽为纯方法而非内联在 `register_tool`：`set_config` 是 `OnceLock`（重复 set panic），
    /// 内联将无法在不污染全局状态的前提下测试过滤分支。
    pub fn is_tool_disabled(&self, name: &str) -> bool {
        self.enabled.get(name) == Some(&false)
    }
}

/// 对账工具配置中的未知工具名（拼错 / 已卸载）
///
/// 统一两层工具配置的未知名处理：全局 `[tools.enabled]` 与定义层 `tools`
/// 共用本函数。返回不在已知工具集合内的配置 key，调用方按所属层级
/// （`layer` 字段）逐个记 WARN 日志后忽略——不阻断、不报错，用户错误由用户承担。
///
/// 纯函数（不记日志、不依赖 tracing），便于在不污染全局状态的前提下测试。
/// 已知集合由调用方传入（全局层传注册表全部名、定义层同），避免本函数依赖 fuyao-core。
pub fn unknown_tool_names(
    tools: &HashMap<String, bool>,
    known: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<String> {
    let known: std::collections::HashSet<String> =
        known.into_iter().map(|s| s.as_ref().to_string()).collect();
    tools
        .keys()
        .filter(|name| !known.contains(name.as_str()))
        .cloned()
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_limits_config_defaults_match_hardcoded() {
        let c = ToolsLimitsConfig::default();
        assert_eq!(c.search_timeout_secs, 60);
        assert_eq!(c.search_max_results, 500);
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

    /// `is_tool_disabled` 三种情况：显式 false / 显式 true / 未列出
    #[test]
    fn is_tool_disabled_semantics() {
        let mut cfg = ToolsConfig::default();
        cfg.enabled.insert("bash".to_string(), false);
        cfg.enabled.insert("grep".to_string(), true);

        // 显式 false → 被禁用
        assert!(cfg.is_tool_disabled("bash"));
        // 显式 true → 未禁用
        assert!(!cfg.is_tool_disabled("grep"));
        // 未列出 → 默认启用
        assert!(!cfg.is_tool_disabled("read"));
        // 空 enabled → 一切默认启用
        assert!(!ToolsConfig::default().is_tool_disabled("any"));
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

    #[test]
    fn tool_runner_config_default_max_concurrent_is_8() {
        let config = ToolRunnerConfig::default();
        assert_eq!(config.max_concurrent, 8);
    }

    #[test]
    fn tool_runner_config_default_never_parallel_contains_bash() {
        let config = ToolRunnerConfig::default();
        assert!(config.never_parallel_tools.contains("bash"));
        assert!(config.never_parallel_tools.contains("todowrite"));
    }

    #[test]
    fn tool_runner_config_default_parallel_safe_contains_read() {
        let config = ToolRunnerConfig::default();
        assert!(config.parallel_safe_tools.contains("read"));
        assert!(config.parallel_safe_tools.contains("glob"));
        assert!(config.parallel_safe_tools.contains("grep"));
        // subagent 派生独立 child session，无跨调用共享状态，可安全并行
        assert!(config.parallel_safe_tools.contains("subagent"));
    }

    #[test]
    fn tool_runner_config_default_path_scoped_contains_file_tools() {
        let config = ToolRunnerConfig::default();
        assert!(config.path_scoped_tools.contains("read"));
        assert!(config.path_scoped_tools.contains("write"));
        assert!(config.path_scoped_tools.contains("edit"));
    }

    /// TerminalConfig 默认 shell 为 auto
    #[test]
    fn terminal_config_default_shell_is_auto() {
        assert_eq!(TerminalConfig::default().shell, "auto");
        // 聚合配置缺省时连带取子默认
        assert_eq!(ToolsConfig::default().terminal.shell, "auto");
    }

    /// [tools.terminal] 反序列化：显式 shell 名 + 缺省回退 auto
    #[test]
    fn deserialize_tools_terminal_shell() {
        let toml_str = r#"
[tools.terminal]
shell = "powershell"
"#;
        #[derive(Deserialize)]
        struct Wrap {
            tools: ToolsConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.tools.terminal.shell, "powershell");
        // 整段缺省时回退 auto
        assert_eq!(ToolsConfig::default().terminal.shell, "auto");
    }

    /// 未知 shell 值在 serde 层放行：拦截职责在引擎启动校验（fail loud），
    /// 配置文件不因单个字段值非法而整体解析失败
    #[test]
    fn deserialize_tools_terminal_unknown_shell_parses() {
        let toml_str = r#"
[tools.terminal]
shell = "zsh"
"#;
        #[derive(Deserialize)]
        struct Wrap {
            tools: ToolsConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.tools.terminal.shell, "zsh");
    }

    /// unknown_tool_names：返回不在已知集合内的配置 key
    #[test]
    fn unknown_tool_names_finds_misspelled() {
        let mut tools = HashMap::new();
        tools.insert("read".to_string(), true);
        tools.insert("wrtie".to_string(), false); // 拼错的 write
        tools.insert("bash".to_string(), false);

        let known = ["read", "write", "bash"];
        let mut unknown = unknown_tool_names(&tools, known);
        unknown.sort();
        assert_eq!(unknown, vec!["wrtie".to_string()]);
    }

    #[test]
    fn unknown_tool_names_empty_when_all_known() {
        let mut tools = HashMap::new();
        tools.insert("read".to_string(), true);
        tools.insert("bash".to_string(), false);
        let unknown = unknown_tool_names(&tools, ["read", "write", "bash", "edit"]);
        assert!(unknown.is_empty());
    }

    #[test]
    fn unknown_tool_names_empty_config() {
        let tools = HashMap::new();
        let unknown = unknown_tool_names(&tools, ["read"]);
        assert!(unknown.is_empty());
    }
}
