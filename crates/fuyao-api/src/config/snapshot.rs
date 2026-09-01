//! 文件快照配置（`[snapshot]` 段）
//!
//! 影子 git 快照机制的总开关与采集上限：`enabled = false` 时引擎全程跳过文件
//! 快照（工具批不再采集、回退降级为仅消息回退），`max_untracked_mb` 限制未跟踪
//! 大文件入快照（超限文件写入影子仓 `info/exclude` 排除，防止 node_modules 级
//! 目录撑爆快照仓）。

use serde::Deserialize;

/// 文件快照配置（`[snapshot]` 段）
///
/// 由装配层读取后传入快照器构造：`enabled` 决定是否构造可用态快照器，
/// `max_untracked_mb` 成为影子仓的未跟踪文件采集上限。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SnapshotConfig {
    /// 文件快照总开关（默认开启）：false 时全程跳过采集与文件回退，
    /// 消息回退语义不受影响
    pub enabled: bool,
    /// 未跟踪文件入快照的大小上限（MB）：超限文件被排除在快照之外
    pub max_untracked_mb: u64,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_untracked_mb: 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认值：总开关开、未跟踪上限 2 MB
    #[test]
    fn snapshot_config_defaults() {
        let c = SnapshotConfig::default();
        assert!(c.enabled);
        assert_eq!(c.max_untracked_mb, 2);
    }

    /// 段部分配置：缺省字段走 Default
    #[test]
    fn deserialize_snapshot_partial() {
        let toml_str = r#"
[snapshot]
max_untracked_mb = 8
"#;
        #[derive(Deserialize)]
        struct Wrap {
            snapshot: SnapshotConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert_eq!(w.snapshot.max_untracked_mb, 8);
        assert!(w.snapshot.enabled, "未配置的 enabled 走默认开");
    }

    /// 显式关闭总开关
    #[test]
    fn deserialize_snapshot_disabled() {
        let toml_str = r#"
[snapshot]
enabled = false
"#;
        #[derive(Deserialize)]
        struct Wrap {
            snapshot: SnapshotConfig,
        }
        let w: Wrap = toml::from_str(toml_str).unwrap();
        assert!(!w.snapshot.enabled);
        assert_eq!(w.snapshot.max_untracked_mb, 2);
    }
}
