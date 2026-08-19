//! Web 工具类型定义
//!
//! 定义 webfetch 工具的参数、URL 安全检查、抓取结果、重定向提示等数据模型。

/// webfetch 工具参数（类型化解析）
///
/// timeout / offset / limit 缺省由 handler 从全局配置取默认值并收敛。
#[derive(Debug, serde::Deserialize)]
pub struct WebFetchArgs {
    /// 要抓取的 URL（必须以 http:// 或 https:// 开头）
    pub url: String,
    /// 输出格式：markdown（默认）、html
    pub output_format: Option<String>,
    /// 超时时间（秒）
    pub timeout: Option<u64>,
    /// 跳过前面的字符数
    pub offset: Option<u64>,
    /// 限制返回的字符数
    pub limit: Option<u64>,
}

/// URL 安全检查结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct URLSafetyResult {
    /// 是否安全
    pub safe: bool,
    /// 解析的 hostname
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// 解析到的 IP 地址
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_ip: Option<String>,
    /// 给 AI 的错误信息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 解决建议
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// WebFetch 输出结果
#[derive(Debug, Clone, serde::Serialize)]
pub struct WebFetchResult {
    /// 抓取的 URL
    pub url: String,
    /// 抓取的内容
    pub content: String,
    /// 输出格式
    pub output_format: String,
    /// 内容字节数
    #[serde(rename = "bytes")]
    pub content_bytes: usize,
    /// HTTP 状态码
    pub status: u16,
    /// Content-Type
    pub content_type: String,
    /// 耗时（毫秒）
    pub duration_ms: u64,
    /// 当前偏移量
    pub offset: usize,
    /// 当前限制
    pub limit: usize,
    /// 内容总长度
    pub total_length: usize,
    /// 下一页偏移量（出现即表示还有更多内容）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// 跨域名重定向时的目标 URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
}

/// 跨域名重定向提示
#[derive(Debug, Clone, serde::Serialize)]
pub struct WebFetchRedirect {
    /// 原始 URL
    pub original_url: String,
    /// 重定向目标 URL
    pub redirect_url: String,
    /// HTTP 状态码
    pub status: u16,
    /// 提示信息
    pub message: String,
}

/// 缓存条目
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// 转换后的内容
    pub content: String,
    /// HTTP Content-Type
    pub content_type: String,
    /// HTTP 状态码
    pub status: u16,
    /// 缓存时间戳（秒）
    pub timestamp: u64,
}
