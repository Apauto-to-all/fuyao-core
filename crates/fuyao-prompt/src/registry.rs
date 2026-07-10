//! Agent 注册表
//!
//! 扫描 fuyao-agents/ 目录（全局层 + 项目层），解析 system.md + fuyao.toml，
//! 提供只读的 Agent 列举和查询能力。
//!
//! 不修改任何现有文件，纯读取层。

use crate::default::DEFAULT_FUYAO_AGENT;
use crate::loader::load_agent_definition;
use fuyao_api::get_workspace_agents_dir;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Agent 来源层
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum AgentSource {
    /// 全局层：~/.fuyao/fuyao-agents/
    Global,
    /// 项目层：{workspace}/.fuyao/fuyao-agents/
    Workspace,
}

/// Agent 完整信息（列表与详情共用）
///
/// 合并原 AgentInfo（轻量）与 AgentDetail（完整）为单一结构。
/// Rust 解析 md/toml 足够快，列表分页后单次返回量可控，
/// 前端点击卡片展开可直接复用列表数据，无需详情接口。
#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    /// Agent 标识（带来源前缀）：`global/{文件夹名}` / `workspace/{文件夹名}` / `default`
    pub id: String,
    /// 显示名（system.md frontmatter）
    pub name: String,
    /// 能力描述（system.md frontmatter）
    pub description: String,
    /// 来源层
    pub source: AgentSource,
    /// 完整系统提示词（system.md body）
    pub system_prompt: String,
    /// 模型配置（fuyao.toml model 字段）
    pub model: Option<String>,
    /// 工具配置（fuyao.toml `[tools]` 表的 key 列表）
    pub tools: Vec<String>,
    /// MCP 服务器引用（fuyao.toml `[mcp_servers]` 表的 key 列表）
    pub mcp_servers: Vec<String>,
    /// Profile 列表（profiles/*.md 文件名，无 .md 后缀）
    pub profiles: Vec<String>,
}

/// 分页响应
#[derive(Debug, Clone, Serialize)]
pub struct PagedAgents {
    /// 当前页的 Agent 列表
    pub items: Vec<AgentInfo>,
    /// Agent 总数（过滤后）
    pub total: usize,
    /// 当前页码（从 1 开始）
    pub page: usize,
    /// 每页大小
    pub size: usize,
}

/// Agent 文件内容读取响应（system.md + fuyao.toml 文本）
#[derive(Debug, Clone, Serialize)]
pub struct AgentContent {
    /// system.md 文本，不存在为空串
    pub system_md: String,
    /// fuyao.toml 文本，不存在为空串
    pub fuyao_toml: String,
}

/// 编辑的目标文件
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentFile {
    /// system.md（角色定义）
    SystemMd,
    /// fuyao.toml（能力配置）
    FuyaoToml,
}

impl AgentFile {
    /// 返回对应的物理文件名
    pub fn file_name(self) -> &'static str {
        match self {
            AgentFile::SystemMd => "system.md",
            AgentFile::FuyaoToml => "fuyao.toml",
        }
    }
}

/// 文件编辑请求（单文件保存）
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateContentRequest {
    /// 编辑的目标文件
    pub file: AgentFile,
    /// 新的文件内容（原样覆盖写入）
    pub content: String,
}

/// Agent 注册表错误
#[derive(Debug, Error)]
pub enum RegistryError {
    /// Agent 名称非法（含路径穿越 / 系统非法字符 / 保留名等）
    #[error("Agent 名称非法：{0}")]
    InvalidName(String),
    /// Agent 文件夹不存在
    #[error("Agent 不存在：{0}")]
    NotFound(String),
    /// Agent 已存在（创建时同名冲突）
    #[error("Agent 已存在：{0}")]
    AlreadyExists(String),
    /// 默认 Agent 禁止编辑或删除
    #[error("默认 Agent 禁止编辑或删除")]
    DefaultForbidden,
    /// 未配置工作目录，无法操作项目层 Agent
    #[error("未配置工作目录，无法操作项目层 Agent")]
    WorkspaceMissing,
    /// 文件读写失败
    #[error("文件操作失败：{0}")]
    Io(#[from] std::io::Error),
}

/// Agent 注册表
///
/// 扫描全局层和项目层的 fuyao-agents/ 目录，合并同名 Agent（项目层优先）。
pub struct AgentRegistry {
    /// 工作目录（用于扫描项目层 Agent）
    workspace: Option<PathBuf>,
    /// 全局基准路径（~/.fuyao），构造时注入
    fuyao_home: PathBuf,
}

impl AgentRegistry {
    /// 创建 AgentRegistry
    ///
    /// `workspace` 为 None 时只扫描全局层。
    /// `fuyao_home` 为全局基准路径，扫描 `{fuyao_home}/fuyao-agents/` 和加载默认 Agent。
    pub fn new(workspace: Option<PathBuf>, fuyao_home: PathBuf) -> Self {
        Self {
            workspace,
            fuyao_home,
        }
    }

    /// 列举 Agent（全局 + 项目合并 + 默认 Agent），支持分页、来源过滤与关键字搜索
    ///
    /// - `page`：页码，从 1 开始（< 1 视为 1）
    /// - `size`：每页大小（< 1 视为 1）
    /// - `scope`：来源过滤，`Some("global")` 仅全局层、`Some("workspace")` 仅项目层、`None` 全部
    /// - `q`：关键字过滤，匹配文件夹名（去前缀）子串，大小写不敏感
    ///
    /// 默认 Agent（id = `default`）仅在 `scope = None`（全部）且 `q` 未命中过滤时出现。
    pub fn list(
        &self,
        page: usize,
        size: usize,
        scope: Option<&str>,
        q: Option<&str>,
    ) -> PagedAgents {
        // key = 文件夹名（去前缀），用于项目层覆盖同名全局
        let mut results: HashMap<String, AgentInfo> = HashMap::new();

        // 全局层
        self.scan_dir(
            &self.fuyao_home.join("fuyao-agents"),
            AgentSource::Global,
            &mut results,
        );

        // 项目层（覆盖同名全局）
        if let Some(ref ws) = self.workspace {
            self.scan_dir(
                &get_workspace_agents_dir(ws),
                AgentSource::Workspace,
                &mut results,
            );
        }

        // 默认 Agent（来自 ~/.fuyao/，仅在无 scope 过滤时纳入）
        if scope.is_none()
            && let Some(default_agent) = self.load_default_agent()
        {
            results.insert("default".to_string(), default_agent);
        }

        // 关键字小写化（q 仅匹配文件夹名，大小写不敏感）
        let q_lower = q.map(|s| s.to_lowercase());

        // 来源过滤（按 id 前缀）+ 关键字过滤（匹配文件夹名子串）
        let mut filtered: Vec<AgentInfo> = results
            .into_values()
            .filter(|agent| match scope {
                Some("global") => agent.id.starts_with("global/"),
                Some("workspace") => agent.id.starts_with("workspace/"),
                _ => true,
            })
            .filter(|agent| match &q_lower {
                Some(query) => folder_name_of(&agent.id).to_lowercase().contains(query),
                None => true,
            })
            .collect();

        filtered.sort_by(|a, b| a.id.cmp(&b.id));

        // 分页
        let total = filtered.len();
        let page = page.max(1);
        let size = size.max(1);
        let start = (page - 1) * size;
        let items = if start >= total {
            Vec::new()
        } else {
            let end = (start + size).min(total);
            filtered.drain(start..end).collect()
        };

        PagedAgents {
            items,
            total,
            page,
            size,
        }
    }

    /// 查询单个 Agent 完整信息
    ///
    /// 解析 id 前缀定位目录：
    /// - `global/{名}` → 全局层
    /// - `workspace/{名}` → 项目层
    /// - `default` → 默认 Agent（~/.fuyao/）
    ///
    /// Agent 不存在返回 None。
    pub fn get(&self, id: &str) -> Option<AgentInfo> {
        // 默认 Agent
        if id == "default" {
            return self.load_default_agent();
        }

        // 解析前缀
        let (source, name) = if let Some(name) = id.strip_prefix("global/") {
            (AgentSource::Global, name)
        } else if let Some(name) = id.strip_prefix("workspace/") {
            (AgentSource::Workspace, name)
        } else {
            return None;
        };

        // 定位目录
        let dir = match source {
            AgentSource::Global => self.fuyao_home.join("fuyao-agents").join(name),
            AgentSource::Workspace => {
                let ws = self.workspace.as_ref()?;
                get_workspace_agents_dir(ws).join(name)
            }
        };

        if !dir.exists() {
            return None;
        }

        self.read_info(id, &dir, source)
    }

    /// 扫描单个目录，将发现的 Agent 加入 results
    ///
    /// key 为文件夹名（不含来源前缀），用于项目层覆盖同名全局；
    /// Agent 的 id 字段带来源前缀（`global/` 或 `workspace/`）。
    fn scan_dir(&self, dir: &Path, source: AgentSource, results: &mut HashMap<String, AgentInfo>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return, // 目录不存在 → 跳过
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let folder = entry.file_name().to_string_lossy().to_string();
            let id = match source {
                AgentSource::Global => format!("global/{folder}"),
                AgentSource::Workspace => format!("workspace/{folder}"),
            };

            // 读取完整信息（无 system.md 或解析失败 → 跳过）
            if let Some(info) = self.read_info(&id, &path, source) {
                results.insert(folder, info);
            }
        }
    }

    /// 从 Agent 目录读取完整信息
    ///
    /// 文件夹存在即是一个 Agent：
    /// - 有 system.md（可读）→ 解析 frontmatter(name/description) + body(system_prompt)
    /// - 无 system.md 或读取失败 → name=文件夹名、description=空、system_prompt=全局默认提示词
    ///
    /// 这样空文件夹创建后立即可见、可编辑。
    fn read_info(&self, id: &str, dir: &Path, source: AgentSource) -> Option<AgentInfo> {
        // 文件夹名（无 system.md 时作为 name）
        let folder_name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        // 解析 system.md（缺失或读取失败 → 用全局默认提示词填充）
        let (name, description, system_prompt) = match load_agent_definition(&dir.join("system.md"))
        {
            Some(def) => (def.name, def.description, def.system_prompt),
            None => (
                folder_name,
                String::new(),
                DEFAULT_FUYAO_AGENT.system_prompt.clone(),
            ),
        };

        // 解析 fuyao.toml（可选）
        let (model, tools, mcp_servers) = parse_fuyao_toml(&dir.join("fuyao.toml"));

        // 扫描 profiles/ 子目录
        let profiles = scan_profiles(&dir.join("profiles"));

        Some(AgentInfo {
            id: id.to_string(),
            name,
            description,
            source,
            system_prompt,
            model,
            tools,
            mcp_servers,
            profiles,
        })
    }

    /// 创建 Agent（空文件夹）
    ///
    /// 不生成 system.md / fuyao.toml，编辑时按需创建。
    /// 同名文件夹已存在 → `AlreadyExists`。
    pub fn create(&self, source: AgentSource, name: &str) -> Result<(), RegistryError> {
        validate_name(name)?;
        let dir = self.agent_dir(source, name)?;
        if dir.exists() {
            return Err(RegistryError::AlreadyExists(name.to_string()));
        }
        std::fs::create_dir_all(&dir)?;
        Ok(())
    }

    /// 读取 Agent 文件内容（system.md + fuyao.toml 文本）
    ///
    /// 文件不存在返回空串（前端显示空白可编辑）。
    pub fn read_content(
        &self,
        source: AgentSource,
        name: &str,
    ) -> Result<AgentContent, RegistryError> {
        self.reject_default(source, name)?;
        validate_name(name)?;
        let dir = self.agent_dir(source, name)?;
        if !dir.exists() {
            return Err(RegistryError::NotFound(name.to_string()));
        }
        Ok(AgentContent {
            system_md: read_file_or_empty(&dir.join("system.md")),
            fuyao_toml: read_file_or_empty(&dir.join("fuyao.toml")),
        })
    }

    /// 写入 Agent 文件内容（单文件覆盖）
    ///
    /// 后端零验证——原样覆盖写入，文件不存在则创建。
    pub fn write_content(
        &self,
        source: AgentSource,
        name: &str,
        file: AgentFile,
        content: &str,
    ) -> Result<(), RegistryError> {
        self.reject_default(source, name)?;
        validate_name(name)?;
        let dir = self.agent_dir(source, name)?;
        if !dir.exists() {
            return Err(RegistryError::NotFound(name.to_string()));
        }
        std::fs::write(dir.join(file.file_name()), content)?;
        Ok(())
    }

    /// 解析 `{source}/{name}` → 物理路径（CRUD 专用，直接映射，不自动 resolve）
    fn agent_dir(&self, source: AgentSource, name: &str) -> Result<PathBuf, RegistryError> {
        let base = match source {
            AgentSource::Global => self.fuyao_home.join("fuyao-agents"),
            AgentSource::Workspace => {
                let ws = self
                    .workspace
                    .as_ref()
                    .ok_or(RegistryError::WorkspaceMissing)?;
                get_workspace_agents_dir(ws)
            }
        };
        Ok(base.join(name))
    }

    /// 拒绝 default（编辑/删除保护：对应 ~/.fuyao/ 而非 fuyao-agents/ 子目录）
    fn reject_default(&self, source: AgentSource, name: &str) -> Result<(), RegistryError> {
        if source == AgentSource::Global && name == "default" {
            return Err(RegistryError::DefaultForbidden);
        }
        Ok(())
    }

    /// 加载默认 Agent（来自 `{fuyao_home}/agents/default.md`）
    ///
    /// - 定义文件：`{fuyao_home}/agents/default.md` 存在则解析，否则用硬编码 `DEFAULT_FUYAO_AGENT`
    /// - fuyao.toml：`{fuyao_home}/fuyao.toml` 存在则解析 model/tools/mcp_servers
    fn load_default_agent(&self) -> Option<AgentInfo> {
        // 解析定义文件（缺失则用硬编码默认 Agent）
        let (name, description, system_prompt) =
            match load_agent_definition(&self.fuyao_home.join("agents").join("default.md")) {
                Some(def) => (def.name, def.description, def.system_prompt),
                None => (
                    DEFAULT_FUYAO_AGENT.name.clone(),
                    DEFAULT_FUYAO_AGENT.description.clone(),
                    DEFAULT_FUYAO_AGENT.system_prompt.clone(),
                ),
            };

        // 解析 fuyao.toml（可选）
        let (model, tools, mcp_servers) = parse_fuyao_toml(&self.fuyao_home.join("fuyao.toml"));

        Some(AgentInfo {
            id: "default".to_string(),
            name,
            description,
            source: AgentSource::Global,
            system_prompt,
            model,
            tools,
            mcp_servers,
            profiles: Vec::new(),
        })
    }
}

/// 解析 fuyao.toml 提取 model / tools / mcp_servers
///
/// 轻量解析，不依赖 fuyao-config 全套。
fn parse_fuyao_toml(path: &Path) -> (Option<String>, Vec<String>, Vec<String>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (None, Vec::new(), Vec::new()); // 文件不存在 → 空配置
    };

    let Ok(table) = toml::from_str::<toml::Table>(&content) else {
        return (None, Vec::new(), Vec::new()); // 解析失败 → 空配置
    };

    // model：顶层字符串字段
    let model = table
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // tools：[tools] 表的 key 列表
    let tools: Vec<String> = table
        .get("tools")
        .and_then(|v| v.as_table())
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();

    // mcp_servers：[mcp_servers] 表的 key 列表
    let mcp_servers: Vec<String> = table
        .get("mcp_servers")
        .and_then(|v| v.as_table())
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();

    (model, tools, mcp_servers)
}

/// 扫描 profiles/ 目录，返回 .md 文件名列表（无后缀）
fn scan_profiles(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new(); // 目录不存在 → 空列表
    };

    let mut profiles = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path.extension().is_some_and(|ext| ext == "md")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            profiles.push(stem.to_string());
        }
    }

    profiles.sort();
    profiles
}

/// 从 id 提取文件夹名（去 global/ 或 workspace/ 前缀，default 保持原样）
///
/// 用于 `q` 关键字匹配（只比对文件夹名，不比对前缀）。
fn folder_name_of(id: &str) -> &str {
    id.strip_prefix("global/")
        .or_else(|| id.strip_prefix("workspace/"))
        .unwrap_or(id)
}

/// 读取文件内容，不存在或读取失败返回空串
fn read_file_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// 名称安全校验（作为文件夹名）
///
/// 拒绝：空串/纯空白、超长（>255）、含路径分隔符或 `..`、Windows 非法字符、
/// Windows 保留名（CON/PRN/NUL/AUX/COM1-9/LPT1-9）、保留字 `default`。
fn validate_name(name: &str) -> Result<(), RegistryError> {
    if name.trim().is_empty() {
        return Err(RegistryError::InvalidName("名称不能为空".to_string()));
    }
    if name.len() > 255 {
        return Err(RegistryError::InvalidName(
            "名称过长（超过 255 字符）".to_string(),
        ));
    }
    // 路径分隔符 / 路径穿越
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(RegistryError::InvalidName(
            "名称含非法路径字符（/ \\ 或 ..）".to_string(),
        ));
    }
    // Windows 非法字符
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return Err(RegistryError::InvalidName(
            "名称含 Windows 非法字符".to_string(),
        ));
    }
    // Windows 保留名
    if is_windows_reserved(name) {
        return Err(RegistryError::InvalidName(
            "名称为 Windows 保留名".to_string(),
        ));
    }
    // 保留给默认 Agent
    if name == "default" {
        return Err(RegistryError::InvalidName("default 为保留名".to_string()));
    }
    Ok(())
}

/// 判断是否为 Windows 保留名（含带扩展名的情况，如 CON.txt）
fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_uppercase();
    let core = stem.as_str();
    matches!(core, "CON" | "PRN" | "NUL" | "AUX")
        || core
            .strip_prefix("COM")
            .is_some_and(|s| matches!(s, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
        || core
            .strip_prefix("LPT")
            .is_some_and(|s| matches!(s, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_source_serializes_pascal_case() {
        assert_eq!(
            serde_json::to_string(&AgentSource::Global).unwrap(),
            "\"Global\""
        );
        assert_eq!(
            serde_json::to_string(&AgentSource::Workspace).unwrap(),
            "\"Workspace\""
        );
    }

    #[test]
    fn registry_list_empty_no_panic() {
        let temp = std::env::temp_dir().join("fuyao_test_registry_empty");
        std::fs::create_dir_all(&temp).unwrap();
        let registry = AgentRegistry::new(None, temp.clone());
        // list 接受分页参数，空目录不 panic
        let paged = registry.list(1, 10, None, None);
        assert_eq!(paged.page, 1);
        assert_eq!(paged.size, 10);
        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn registry_get_returns_none_for_no_prefix() {
        let temp = std::env::temp_dir().join("fuyao_test_registry_no_prefix");
        std::fs::create_dir_all(&temp).unwrap();
        let registry = AgentRegistry::new(None, temp.clone());
        assert!(registry.get("noprefix").is_none());
        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn registry_get_returns_none_for_nonexistent() {
        let temp = std::env::temp_dir().join("fuyao_test_registry_nonexistent");
        std::fs::create_dir_all(&temp).unwrap();
        let registry = AgentRegistry::new(None, temp.clone());
        assert!(registry.get("global/nonexistent_xyz").is_none());
        assert!(registry.get("workspace/nonexistent_xyz").is_none());
        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn parse_fuyao_toml_nonexistent() {
        let (model, tools, mcp) = parse_fuyao_toml(Path::new("/nonexistent/fuyao.toml"));
        assert!(model.is_none());
        assert!(tools.is_empty());
        assert!(mcp.is_empty());
    }

    #[test]
    fn parse_fuyao_toml_valid() {
        let temp = std::env::temp_dir().join("fuyao_test_registry_toml");
        std::fs::create_dir_all(&temp).unwrap();
        let toml_path = temp.join("fuyao.toml");
        std::fs::write(
            &toml_path,
            r#"model = "deepseek/deepseek-v4-flash"

[tools]
read = true
write = true
bash = false

[mcp_servers.filesystem]
command = "node"
"#,
        )
        .unwrap();

        let (model, tools, mcp) = parse_fuyao_toml(&toml_path);
        assert_eq!(model.as_deref(), Some("deepseek/deepseek-v4-flash"));
        assert!(tools.contains(&"read".to_string()));
        assert!(tools.contains(&"write".to_string()));
        assert!(tools.contains(&"bash".to_string()));
        assert_eq!(tools.len(), 3);
        assert_eq!(mcp, vec!["filesystem"]);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn scan_profiles_nonexistent_dir() {
        let profiles = scan_profiles(Path::new("/nonexistent/profiles"));
        assert!(profiles.is_empty());
    }

    #[test]
    fn scan_profiles_finds_md_files() {
        let temp = std::env::temp_dir().join("fuyao_test_registry_profiles");
        std::fs::create_dir_all(&temp).unwrap();
        std::fs::write(temp.join("python.md"), "# Python").unwrap();
        std::fs::write(temp.join("rust.md"), "# Rust").unwrap();
        std::fs::write(temp.join("readme.txt"), "not a profile").unwrap();

        let profiles = scan_profiles(&temp);
        assert_eq!(profiles.len(), 2);
        assert!(profiles.contains(&"python".to_string()));
        assert!(profiles.contains(&"rust".to_string()));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn registry_list_prefix_default_paging() {
        // 创建临时全局 Agent 目录结构
        let temp = std::env::temp_dir().join("fuyao_test_registry_prefix");
        let global_agents = temp.join("fuyao-agents").join("coder");
        std::fs::create_dir_all(&global_agents).unwrap();
        std::fs::write(
            global_agents.join("system.md"),
            "---\nname: 开发\ndescription: 代码开发\n---\n你是开发工程师",
        )
        .unwrap();
        std::fs::write(
            global_agents.join("fuyao.toml"),
            "model = \"deepseek/deepseek-v4-flash\"\n",
        )
        .unwrap();

        // profiles
        let profiles_dir = global_agents.join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        std::fs::write(profiles_dir.join("python.md"), "# Python guide").unwrap();

        // 全局 fuyao.toml（默认 Agent 的 model 来源）
        std::fs::write(temp.join("fuyao.toml"), "model = \"aliyun/qwen3.6-plus\"\n").unwrap();

        // 注入 fuyao_home 指向临时目录
        let registry = AgentRegistry::new(None, temp.clone());

        // 全部（含默认 Agent）：coder 带前缀，且包含 default
        let all = registry.list(1, 10, None, None);
        assert!(all.items.iter().any(|a| a.id == "global/coder"));
        assert!(all.items.iter().any(|a| a.id == "default"));
        assert_eq!(all.total, 2);

        // coder 字段完整
        let coder = all
            .items
            .iter()
            .find(|a| a.id == "global/coder")
            .expect("应找到 global/coder");
        assert_eq!(coder.name, "开发");
        assert_eq!(coder.description, "代码开发");
        assert_eq!(coder.source, AgentSource::Global);
        assert_eq!(coder.model.as_deref(), Some("deepseek/deepseek-v4-flash"));
        assert_eq!(coder.profiles, vec!["python"]);
        assert!(coder.system_prompt.contains("开发工程师"));

        // 默认 Agent 的 model 来自全局 fuyao.toml
        let default = all
            .items
            .iter()
            .find(|a| a.id == "default")
            .expect("应找到 default");
        assert_eq!(default.model.as_deref(), Some("aliyun/qwen3.6-plus"));

        // scope 过滤：仅全局 → 不含 default
        let global_only = registry.list(1, 10, Some("global"), None);
        assert!(
            global_only
                .items
                .iter()
                .all(|a| a.id.starts_with("global/"))
        );
        assert!(!global_only.items.iter().any(|a| a.id == "default"));

        // 分页：size=1 → 第一页 1 个，总数 2
        let page1 = registry.list(1, 1, None, None);
        assert_eq!(page1.items.len(), 1);
        assert_eq!(page1.total, 2);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn registry_list_project_overrides_global() {
        // 项目层同名 Agent 覆盖全局层（id 前缀变 workspace/）
        let temp = std::env::temp_dir().join("fuyao_test_registry_override");
        let global_agents = temp.join("fuyao-agents").join("coder");
        std::fs::create_dir_all(&global_agents).unwrap();
        std::fs::write(
            global_agents.join("system.md"),
            "---\nname: 全局开发\ndescription: 全局\n---\n全局",
        )
        .unwrap();

        // 项目层同名 coder
        let ws = temp.join("myproject");
        let project_agents = ws.join(".fuyao").join("fuyao-agents").join("coder");
        std::fs::create_dir_all(&project_agents).unwrap();
        std::fs::write(
            project_agents.join("system.md"),
            "---\nname: 项目开发\ndescription: 项目\n---\n项目",
        )
        .unwrap();

        // 注入 fuyao_home 指向临时目录
        let registry = AgentRegistry::new(Some(ws), temp.clone());
        let all = registry.list(1, 10, None, None);

        // coder 被项目层覆盖 → id 为 workspace/coder，name 为项目开发
        let coder = all
            .items
            .iter()
            .find(|a| a.id == "workspace/coder")
            .expect("项目层应覆盖为 workspace/coder");
        assert_eq!(coder.name, "项目开发");
        assert_eq!(coder.source, AgentSource::Workspace);
        // 不应同时存在 global/coder
        assert!(!all.items.iter().any(|a| a.id == "global/coder"));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn folder_name_of_strips_prefix() {
        assert_eq!(folder_name_of("global/coder"), "coder");
        assert_eq!(folder_name_of("workspace/translator"), "translator");
        assert_eq!(folder_name_of("default"), "default");
    }

    #[test]
    fn validate_name_accepts_valid() {
        assert!(validate_name("coder").is_ok());
        assert!(validate_name("my-agent").is_ok());
        assert!(validate_name("agent_42").is_ok());
        assert!(validate_name("翻译助手").is_ok());
    }

    #[test]
    fn validate_name_rejects_invalid() {
        // 空串 / 纯空白
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        // 路径分隔符 / 路径穿越
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("a../b").is_err());
        // Windows 非法字符
        assert!(validate_name("a<b").is_err());
        assert!(validate_name("a:b").is_err());
        assert!(validate_name("a*b").is_err());
        assert!(validate_name("a|b").is_err());
        // Windows 保留名
        assert!(validate_name("CON").is_err());
        assert!(validate_name("con.txt").is_err());
        assert!(validate_name("COM1").is_err());
        assert!(validate_name("LPT9").is_err());
        // 保留字 default
        assert!(validate_name("default").is_err());
        // 超长
        let long = "a".repeat(256);
        assert!(validate_name(&long).is_err());
    }

    #[test]
    fn registry_list_no_systemmd_fills_default() {
        // 空文件夹（无 system.md）→ list 返回，name=文件夹名，system_prompt=默认提示词
        let temp = std::env::temp_dir().join("fuyao_test_registry_empty_folder");
        let empty_agent = temp.join("fuyao-agents").join("blank");
        std::fs::create_dir_all(&empty_agent).unwrap();

        // 注入 fuyao_home 指向临时目录
        let registry = AgentRegistry::new(None, temp.clone());
        let all = registry.list(1, 10, None, None);

        let blank = all
            .items
            .iter()
            .find(|a| a.id == "global/blank")
            .expect("空文件夹应作为 Agent 出现");
        assert_eq!(blank.name, "blank");
        assert!(blank.description.is_empty());
        assert!(!blank.system_prompt.is_empty());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn registry_list_q_filter_matches_folder_name() {
        // q 仅匹配文件夹名（去前缀），大小写不敏感
        let temp = std::env::temp_dir().join("fuyao_test_registry_q");
        let agents = temp.join("fuyao-agents");
        std::fs::create_dir_all(agents.join("coder")).unwrap();
        std::fs::write(
            agents.join("coder").join("system.md"),
            "---\nname: c\n---\nx",
        )
        .unwrap();
        std::fs::create_dir_all(agents.join("translator")).unwrap();
        std::fs::write(
            agents.join("translator").join("system.md"),
            "---\nname: t\n---\nx",
        )
        .unwrap();

        // 注入 fuyao_home 指向临时目录
        let registry = AgentRegistry::new(None, temp.clone());

        // q="cod" → 仅 coder
        let filtered = registry.list(1, 10, None, Some("cod"));
        let ids: Vec<_> = filtered.items.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["global/coder"]);

        // q="TRAN" → 匹配 translator（大小写不敏感）
        let filtered = registry.list(1, 10, None, Some("TRAN"));
        let ids: Vec<_> = filtered.items.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["global/translator"]);

        // q 不命中任何文件夹名 → 空（含 default 也被过滤）
        let filtered = registry.list(1, 10, None, Some("zzz"));
        assert!(filtered.items.is_empty());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn registry_crud_create_and_conflict() {
        let temp = std::env::temp_dir().join("fuyao_test_registry_crud_create");
        // 清理上次失败运行可能残留的目录
        std::fs::remove_dir_all(&temp).ok();
        std::fs::create_dir_all(temp.join("fuyao-agents")).unwrap();

        // 注入 fuyao_home 指向临时目录
        let registry = AgentRegistry::new(None, temp.clone());

        // 创建空文件夹
        registry
            .create(AgentSource::Global, "newagent")
            .expect("创建应成功");
        assert!(temp.join("fuyao-agents").join("newagent").is_dir());

        // 同名 → AlreadyExists
        let err = registry
            .create(AgentSource::Global, "newagent")
            .unwrap_err();
        assert!(matches!(err, RegistryError::AlreadyExists(_)));

        // 非法名 → InvalidName
        let err = registry.create(AgentSource::Global, "a/b").unwrap_err();
        assert!(matches!(err, RegistryError::InvalidName(_)));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn registry_crud_read_content() {
        let temp = std::env::temp_dir().join("fuyao_test_registry_crud_read");
        let agent_dir = temp.join("fuyao-agents").join("reader");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("system.md"), "---\nname: r\n---\nbody").unwrap();

        // 注入 fuyao_home 指向临时目录
        let registry = AgentRegistry::new(None, temp.clone());

        // 读取：system.md 有内容，fuyao.toml 不存在 → 空串
        let content = registry
            .read_content(AgentSource::Global, "reader")
            .unwrap();
        assert_eq!(content.system_md, "---\nname: r\n---\nbody");
        assert!(content.fuyao_toml.is_empty());

        // 不存在 → NotFound
        let err = registry
            .read_content(AgentSource::Global, "nope")
            .unwrap_err();
        assert!(matches!(err, RegistryError::NotFound(_)));

        // default → DefaultForbidden
        let err = registry
            .read_content(AgentSource::Global, "default")
            .unwrap_err();
        assert!(matches!(err, RegistryError::DefaultForbidden));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn registry_crud_write_content() {
        let temp = std::env::temp_dir().join("fuyao_test_registry_crud_write");
        let agent_dir = temp.join("fuyao-agents").join("writer");
        std::fs::create_dir_all(&agent_dir).unwrap();

        // 注入 fuyao_home 指向临时目录
        let registry = AgentRegistry::new(None, temp.clone());

        // 写入 system.md（文件不存在则创建）
        registry
            .write_content(
                AgentSource::Global,
                "writer",
                AgentFile::SystemMd,
                "---\nname: w\n---\nwritten",
            )
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(agent_dir.join("system.md")).unwrap(),
            "---\nname: w\n---\nwritten"
        );

        // 写入 fuyao.toml
        registry
            .write_content(
                AgentSource::Global,
                "writer",
                AgentFile::FuyaoToml,
                "model = \"deepseek/deepseek-v4-flash\"\n",
            )
            .unwrap();
        assert!(agent_dir.join("fuyao.toml").exists());

        // 覆盖写入
        registry
            .write_content(AgentSource::Global, "writer", AgentFile::SystemMd, "覆盖")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(agent_dir.join("system.md")).unwrap(),
            "覆盖"
        );

        // default → DefaultForbidden
        let err = registry
            .write_content(AgentSource::Global, "default", AgentFile::SystemMd, "x")
            .unwrap_err();
        assert!(matches!(err, RegistryError::DefaultForbidden));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn agent_file_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&AgentFile::SystemMd).unwrap(),
            "\"system_md\""
        );
        assert_eq!(
            serde_json::to_string(&AgentFile::FuyaoToml).unwrap(),
            "\"fuyao_toml\""
        );
        // 反序列化（PUT 请求体 file 字段）
        let req: UpdateContentRequest =
            serde_json::from_str(r#"{"file":"system_md","content":"x"}"#).unwrap();
        assert_eq!(req.file, AgentFile::SystemMd);
        assert_eq!(req.content, "x");
    }
}
