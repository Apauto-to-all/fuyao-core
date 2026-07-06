//! 配置加载器 —— 单一加载入口
//!
//! 三层配置合并加载的唯一实现：全局 → Agent 目录 → 工作目录。
//!
//! ## 合并策略：按子段递归深合并
//!
//! 对原始 TOML 表做递归深合并（`deep_merge_tables`）：双方均为 table 则递归深入，
//! 否则高优先级覆盖低优先级。这样嵌套子段（如 `[tools.runner]`、`[llm.retry]`、
//! `[session.compression]`）天然得到字段级合并，而非整段替换。
//!
//! ## providers 特殊处理
//!
//! Provider 段走 `providers::load_providers` 容错解析（serde 不支持 TOML 整数→f64 价格
//! 自动转换），因此从合并表中 `remove` 出来单独解析，其余字段整体反序列化为 `FuyaoConfig`
//! （`providers` 字段以 `#[serde(skip)]` 跳过 serde），最后回填 providers。

use std::path::Path;

use serde::Deserialize;

use crate::AgentPaths;
use crate::config::FuyaoConfig;
use crate::config::error::ConfigError;
use crate::config::providers::load_providers;

/// 递归深合并两张 TOML 表
///
/// 规则：
/// - 同名键双方均为 table → 递归合并
/// - 否则（scalar / array / 单方 table）→ 用 `override` 的值覆盖 `base`
///
/// 数组视为整体覆盖（如 `[[cost.tiers]]` 不会跨层拼接），符合「高优先级完整重定义」语义。
fn deep_merge_tables(base: &mut toml::Table, override_data: &toml::Table) {
    for (key, val) in override_data {
        match base.get_mut(key) {
            // 双方均为 table → 递归
            Some(toml::Value::Table(base_t)) if val.is_table() => {
                if let Some(over_t) = val.as_table() {
                    deep_merge_tables(base_t, over_t);
                }
            }
            // 否则覆盖（含 scalar、array、以及类型不一致的情况）
            _ => {
                base.insert(key.clone(), val.clone());
            }
        }
    }
}

/// 三层配置合并加载
///
/// 从低优先级到高优先级逐层合并：全局 → Agent 目录 → 工作目录。
/// 每层配置文件不存在时跳过，全部不存在则返回 `None`。
///
/// # Arguments
/// * `global_path` - 全局配置文件路径（`~/.fuyao/fuyao.toml`，最低优先级）
/// * `agent_path` - Agent 目录配置文件路径
/// * `workspace_path` - 工作目录配置文件路径（最高优先级）
pub fn load_merged_config(
    global_path: Option<&Path>,
    agent_path: Option<&Path>,
    workspace_path: Option<&Path>,
) -> Result<Option<FuyaoConfig>, ConfigError> {
    let mut merged_table: toml::Table = toml::Table::new();
    let mut has_any_config = false;

    // 从低优先级到高优先级逐层深合并
    let paths: Vec<Option<&Path>> = vec![global_path, agent_path, workspace_path];
    for path in paths.into_iter().flatten() {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            let table: toml::Table = toml::from_str(&content)?;
            if !table.is_empty() {
                has_any_config = true;
                deep_merge_tables(&mut merged_table, &table);
            }
        }
    }

    if !has_any_config {
        return Ok(None);
    }

    // providers 单独走容错解析（serde 不支持 TOML 整数→f64 价格转换）
    let providers = merged_table
        .remove("providers")
        .map(|v| load_providers(&v))
        .unwrap_or_default();

    // 其余字段整体反序列化（providers 字段 #[serde(skip)]，不参与 serde）
    let mut config = FuyaoConfig::deserialize(toml::Value::Table(merged_table))?;
    config.providers = providers;

    Ok(Some(config))
}

/// 从 `AgentPaths` 加载配置
///
/// 使用 `AgentPaths::config_paths()` 获取三层路径并合并加载。
/// 优先级：工作目录 > Agent 目录 > 全局。
pub fn load_config(agent_paths: &AgentPaths) -> Result<Option<FuyaoConfig>, ConfigError> {
    let paths = agent_paths.config_paths();
    let all_paths = paths.all();
    load_merged_config(
        all_paths.get(2).copied(),  // global_（最低优先级）
        all_paths.get(1).copied(),  // agent
        all_paths.first().copied(), // workspace（最高优先级）
    )
}

/// 生成唯一临时文件路径（仅供测试用）
#[cfg(test)]
fn temp_config_path(name: &str, content: &str) -> std::path::PathBuf {
    use std::io::Write;
    let mut path = std::env::temp_dir();
    path.push(format!(
        "fuyao_config_test_{}_{}.toml",
        std::process::id(),
        name
    ));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_empty_paths_returns_none() {
        let result = load_merged_config(None, None, None).unwrap();
        assert!(result.is_none());
    }

    /// 深合并：同名 table 递归，scalar 覆盖
    #[test]
    fn deep_merge_tables_recurses_and_overrides() {
        let mut base: toml::Table =
            toml::from_str(r#"model = "a"\n[llm]\nrequest_timeout_secs = 300\n[llm.retry]\ninitial_delay_ms = 2000"#.replace("\\n", "\n").as_str()).unwrap();
        let over: toml::Table = toml::from_str(
            r#"[llm]\nrequest_timeout_secs = 600\n[llm.retry]\nmax_delay_ms = 999"#
                .replace("\\n", "\n")
                .as_str(),
        )
        .unwrap();

        deep_merge_tables(&mut base, &over);

        assert_eq!(base.get("model").and_then(|v| v.as_str()), Some("a"));
        // llm.request_timeout_secs 被覆盖
        assert_eq!(
            base.get("llm")
                .unwrap()
                .get("request_timeout_secs")
                .and_then(|v| v.as_integer()),
            Some(600)
        );
        // llm.retry.initial_delay_ms 来自 base（递归合并不丢字段）
        assert_eq!(
            base.get("llm")
                .unwrap()
                .get("retry")
                .unwrap()
                .get("initial_delay_ms")
                .and_then(|v| v.as_integer()),
            Some(2000)
        );
        // llm.retry.max_delay_ms 来自 over
        assert_eq!(
            base.get("llm")
                .unwrap()
                .get("retry")
                .unwrap()
                .get("max_delay_ms")
                .and_then(|v| v.as_integer()),
            Some(999)
        );
    }

    /// 三层优先级：workspace > agent > global（端到端）
    #[test]
    fn three_layer_precedence_workspace_overrides_global() {
        let global = temp_config_path(
            "global",
            r#"
model = "global/model"
[llm]
request_timeout_secs = 100
[tools.limits]
terminal_default_timeout_secs = 60
"#,
        );
        let agent = temp_config_path(
            "agent",
            r#"
[llm]
connect_timeout_secs = 20
"#,
        );
        let workspace = temp_config_path(
            "workspace",
            r#"
model = "workspace/model"
[tools.limits]
terminal_default_timeout_secs = 240
"#,
        );

        let cfg = load_merged_config(Some(&global), Some(&agent), Some(&workspace))
            .unwrap()
            .unwrap();

        // model：workspace 覆盖 global
        assert_eq!(cfg.model.as_deref(), Some("workspace/model"));
        // llm.request_timeout_secs 来自 global（无更高优先级覆盖）
        assert_eq!(cfg.llm.request_timeout_secs, 100);
        // llm.connect_timeout_secs 来自 agent（agent 覆盖 default）
        assert_eq!(cfg.llm.connect_timeout_secs, 20);
        // tools.limits.terminal_default_timeout_secs：workspace 覆盖 global
        assert_eq!(cfg.tools.limits.terminal_default_timeout_secs, 240);
        // llm.retry 缺省走 default（三层都未指定）
        assert_eq!(cfg.llm.retry.initial_delay_ms, 2000);

        // 清理
        let _ = std::fs::remove_file(&global);
        let _ = std::fs::remove_file(&agent);
        let _ = std::fs::remove_file(&workspace);
    }

    /// providers 走容错解析（整数价格不被丢弃）
    #[test]
    fn providers_tolerant_parsing_via_loader() {
        let workspace = temp_config_path(
            "providers",
            r#"
[providers.aliyun]
name = "阿里云百炼"
[providers.aliyun.models."qwen3.6-plus"]
name = "qwen3.6-plus"
[providers.aliyun.models."qwen3.6-plus".cost]
input = 2
"#,
        );

        let cfg = load_merged_config(None, None, Some(&workspace))
            .unwrap()
            .unwrap();
        let model = &cfg.providers["aliyun"].models["qwen3.6-plus"];
        assert_eq!(model.cost.input, Some(2.0));

        let _ = std::fs::remove_file(&workspace);
    }

    /// mcp_servers 反序列化
    #[test]
    fn mcp_servers_deserialized() {
        let workspace = temp_config_path(
            "mcp",
            r#"
[mcp_servers.test]
command = "node"
args = ["server.js"]
"#,
        );

        let cfg = load_merged_config(None, None, Some(&workspace))
            .unwrap()
            .unwrap();
        assert!(cfg.mcp_servers.contains_key("test"));
        assert_eq!(cfg.mcp_servers["test"].command.as_deref(), Some("node"));

        let _ = std::fs::remove_file(&workspace);
    }
}
