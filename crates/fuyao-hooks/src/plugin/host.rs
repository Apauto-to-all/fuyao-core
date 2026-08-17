//! 插件装配宿主
//!
//! [`PluginHost`] 收集所有插件工厂（[`Plugin`](crate::Plugin)），提供：
//! - 批量生成实例（[`create_instances`](PluginHost::create_instances)，生成前内置重名校验）
//! - 批量销毁工厂（[`dispose_all`](PluginHost::dispose_all)）
//!
//! 关键约束：
//! - [`Plugin::create_instance`](crate::Plugin::create_instance) 和 [`Plugin::dispose`](crate::Plugin::dispose)
//!   是**同步**调用。panic 防护必须用 [`std::panic::catch_unwind`]（不能用 FutureExt::catch_unwind）。
//! - [`PluginInstance::register`](crate::PluginInstance::register) 也是同步的（只 push 闭包到 Vec）。
//! - 单个插件 panic 不阻塞其他插件（tracing warn + 继续）。

use std::panic::AssertUnwindSafe;

use crate::Plugin;
use crate::PluginInstance;

/// 插件装配错误（create_instances / validate_unique_names 阶段）
#[derive(Debug, thiserror::Error)]
pub enum PluginInstallError {
    /// 插件重名：core 不接受任何重名情况
    #[error("插件重名: {name}")]
    DuplicateName { name: String },
}

/// 把 panic payload（`Box<dyn Any + Send>`）转为可读 String
///
/// 暴露为 `pub` 是为了让 fuyao-core 在 register 阶段做同步 panic 防护时复用同一份逻辑
/// （engine 在 assemble_session_hooks 里 catch_unwind instance.register，需要把 payload 转可读字符串）。
pub fn panic_payload_to_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "未知 panic".to_string()
    }
}

/// 插件实例配对：`(插件名, 该 session 的实例)`
///
/// 插件名供装配方构造绑定该插件身份的 [`SessionSender`](crate::SessionSender)
/// （注入消息 source 可追溯）。
pub type NamedPluginInstance = (String, Box<dyn PluginInstance>);

/// 插件装配宿主
///
/// 收集插件工厂（[`Plugin`](crate::Plugin)），引擎启动时持有 `Arc<PluginHost>`，
/// 在每个 session 装配时调用 [`create_instances`](Self::create_instances) 生成
/// 该 session 的所有插件实例。
///
/// 不持有 HooksRegistry —— 由调用方（引擎）在每个 session 装配时新建 registry，
/// 调用 [`PluginInstance::register`](crate::PluginInstance::register) 注册 hooks。
pub struct PluginHost {
    plugins: Vec<Box<dyn Plugin>>,
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginHost {
    /// 创建空主机
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    /// 添加插件工厂（注册顺序 = create_instances 返回顺序）
    pub fn add(&mut self, plugin: Box<dyn Plugin>) {
        self.plugins.push(plugin);
    }

    /// 当前插件名称列表（调试用）
    pub fn list(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.name()).collect()
    }

    /// 校验插件名称唯一性（crate 内部辅助方法）
    ///
    /// core 不接受任何重名情况——检测到重名立即返回 `Err`。
    /// 由 [`create_instances`](Self::create_instances) 在生成实例前内部调用，
    /// 避免半途中断残留状态。
    pub(crate) fn validate_unique_names(&self) -> Result<(), PluginInstallError> {
        let mut seen = std::collections::HashSet::new();
        for plugin in &self.plugins {
            let name = plugin.name().to_string();
            if !seen.insert(name.clone()) {
                return Err(PluginInstallError::DuplicateName { name });
            }
        }
        Ok(())
    }

    /// 批量生成所有插件的实例（每个插件调用一次 create_instance）
    ///
    /// 调用前会先做重名校验（重名硬失败）。生成顺序 = 插件注册顺序。
    /// 返回 `(插件名, 实例)` 配对——插件名供装配方构造绑定该插件身份的
    /// [`SessionSender`](crate::SessionSender)（注入消息 source 可追溯）。
    ///
    /// **panic 防护**：单个插件 create_instance 崩溃不阻塞其他插件（tracing warn + 继续）。
    /// 该崩溃插件的实例会被跳过（不在返回的 Vec 里）。
    ///
    /// 返回的实例 Vec 由调用方逐个 `instance.register(&mut hooks, &sender)`
    /// 注册到该 session 的 registry。
    pub fn create_instances(&self) -> Result<Vec<NamedPluginInstance>, PluginInstallError> {
        // 先做重名校验（在生成实例前，避免半途中断残留状态）
        self.validate_unique_names()?;

        let mut instances = Vec::with_capacity(self.plugins.len());
        for plugin in &self.plugins {
            let name = plugin.name().to_string();
            // 同步 panic 防护：create_instance 是同步调用
            match std::panic::catch_unwind(AssertUnwindSafe(|| plugin.create_instance())) {
                Ok(instance) => instances.push((name, instance)),
                Err(payload) => {
                    tracing::warn!(
                        plugin = %name,
                        phase = "create_instance",
                        recovered = true,
                        cause = %panic_payload_to_string(&*payload),
                        "插件 create_instance panic 已恢复（跳过该插件）"
                    );
                }
            }
        }
        Ok(instances)
    }

    /// 遍历所有插件工厂调用 dispose（逆序 LIFO）。单个失败不阻塞。
    ///
    /// dispose 是同步调用，panic 防护用 `std::panic::catch_unwind`。
    pub fn dispose_all(&self) {
        for plugin in self.plugins.iter().rev() {
            let name = plugin.name().to_string();
            match std::panic::catch_unwind(AssertUnwindSafe(|| plugin.dispose())) {
                Ok(()) => {}
                Err(payload) => {
                    tracing::warn!(
                        plugin = %name,
                        phase = "dispose",
                        recovered = true,
                        cause = %panic_payload_to_string(&*payload),
                        "插件 dispose panic 已恢复"
                    );
                }
            }
        }
    }
}
