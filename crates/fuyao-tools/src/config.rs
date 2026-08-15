//! 工具系统配置常量
//!
//! 高频可调项（超时/限制类）已迁移至 `fuyao_api::config::ToolsLimitsConfig`，
//! 通过 `[tools.limits]` 配置段读取。此处仅保留非高频常量（多数用户无需调整）。

/// 最大读取字符数
pub const MAX_READ_CHARS: usize = 100_000;

/// 大文件提示字节数
pub const LARGE_FILE_HINT_BYTES: u64 = 512_000;

/// 读取时间戳缓存大小
pub const READ_TIMESTAMPS_CAP: usize = 1000;

/// 是否脱敏敏感信息
pub const REDACT_SECRETS: bool = true;

/// 搜索排除目录
pub const SEARCH_EXCLUDE_DIRS: &[&str] = &[
    ".venv",
    "venv",
    ".env",
    "node_modules",
    "__pycache__",
    ".git",
    ".hg",
    ".svn",
    ".jj",
    "dist",
    "build",
    ".eggs",
    ".mypy_cache",
    ".ruff_cache",
];

/// 模拟浏览器 User-Agent
pub const WEBFETCH_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
     AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/143.0.0.0 Safari/537.36";

/// 缓存最大条目数
pub const WEBFETCH_CACHE_MAX_SIZE: usize = 100;

/// 缓存 TTL（秒），默认 15分钟
pub const WEBFETCH_CACHE_TTL: u64 = 900;
