//! 工具系统配置模块
//!
//! 定义所有工具共享的常量，包括读取限制、搜索配置、脱敏开关等。

/// 最大读取字符数
pub const MAX_READ_CHARS: usize = 100_000;

/// 大文件提示字节数
pub const LARGE_FILE_HINT_BYTES: u64 = 512_000;

/// 读取历史记录缓存大小
pub const READ_HISTORY_CAP: usize = 500;

/// 去重缓存大小
pub const DEDUP_CAP: usize = 1000;

/// 读取时间戳缓存大小
pub const READ_TIMESTAMPS_CAP: usize = 1000;

/// 是否脱敏敏感信息
pub const REDACT_SECRETS: bool = true;

/// 默认搜索限制
pub const SEARCH_DEFAULT_LIMIT: usize = 50;

/// 最大搜索限制
pub const SEARCH_MAX_LIMIT: usize = 100;

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

/// 搜索命令超时时间（秒）
pub const SEARCH_TIMEOUT: u64 = 60;

/// 默认终端超时时间（秒）
pub const TERMINAL_DEFAULT_TIMEOUT: u64 = 120;

/// 最大终端超时时间（秒）
pub const TERMINAL_MAX_TIMEOUT: u64 = 6000;

/// 最大终端输出字符数
pub const TERMINAL_MAX_OUTPUT_CHARS: usize = 50_000;

/// 默认 WebFetch 超时时间（秒）
pub const WEBFETCH_DEFAULT_TIMEOUT: u64 = 30;

/// 最大 WebFetch 超时时间（秒）
pub const WEBFETCH_MAX_TIMEOUT: u64 = 180;

/// 最大输出字符数
pub const WEBFETCH_MAX_OUTPUT_CHARS: usize = 100_000;

/// 最大下载字节数（5MB）
pub const WEBFETCH_MAX_DOWNLOAD_BYTES: usize = 5 * 1024 * 1024;

/// 模拟浏览器 User-Agent
pub const WEBFETCH_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
     AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/143.0.0.0 Safari/537.36";

/// 缓存最大条目数
pub const WEBFETCH_CACHE_MAX_SIZE: usize = 100;

/// 缓存 TTL（秒），默认 15分钟
pub const WEBFETCH_CACHE_TTL: u64 = 900;
