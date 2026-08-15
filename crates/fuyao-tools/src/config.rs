//! 工具系统配置常量
//!
//! 高频可调项（超时/限制类）已迁移至 `fuyao_api::config::ToolsLimitsConfig`，
//! 通过 `[tools.limits]` 配置段读取。此处仅保留非高频常量（多数用户无需调整）。

/// read 工具单次返回内容的字符预算
///
/// 在收集循环内增量执行：窗口内每行累加其字节成本，达到预算即停止收集，
/// 内存天然有界。计量口径与 `String::len` 一致——UTF-8 字节数而非字符数，
/// 以字节作为上下文成本的代理（多字节字符会更快触顶，属预期行为）
pub const MAX_READ_CHARS: usize = 100_000;

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

/// grep 匹配行 / 上下文行最大字符数，超出截断加省略标记
///
/// 防单行巨物（压缩 JS、单行 JSON 等）绕过条数上限灌爆上下文：
/// limit 限条数，本常量限每条的字符数
pub const GREP_MAX_LINE_CHARS: usize = 300;

/// todowrite 单次提交的任务项数上限
///
/// 宽松兜底：正常规划永远到不了此量级，超限即报错拒绝整批提交
pub const TODO_MAX_ITEMS: usize = 200;
