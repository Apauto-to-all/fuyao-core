//! 分页处理
//!
//! 提供分页参数验证和内容切片功能。

/// 分页结果
pub struct PaginationResult {
    /// 分页后的内容
    pub content: String,
    /// 内容总长度
    pub total_length: usize,
    /// 是否有更多内容
    pub has_more: bool,
    /// 下一页偏移量
    pub next_offset: Option<usize>,
}

/// 验证并规范化分页参数
///
/// limit 默认/上限从全局配置 `get_config().tools.limits.webfetch_max_output_chars` 读取。
pub fn validate_pagination(offset: Option<usize>, limit: Option<usize>) -> (usize, usize) {
    let max_output = fuyao_api::get_config()
        .tools
        .limits
        .webfetch_max_output_chars;
    let valid_offset = offset.unwrap_or(0);
    let valid_limit = limit.unwrap_or(max_output);
    let valid_limit = valid_limit.clamp(1, max_output);
    (valid_offset, valid_limit)
}

/// 应用分页逻辑
pub fn apply_pagination(content: &str, offset: usize, limit: usize) -> PaginationResult {
    let total_length = content.len();

    if offset >= total_length {
        return PaginationResult {
            content: String::new(),
            total_length,
            has_more: false,
            next_offset: None,
        };
    }

    let end_pos = (offset + limit).min(total_length);
    let paginated_content = content[offset..end_pos].to_string();

    let has_more = end_pos < total_length;
    let next_offset = if has_more { Some(end_pos) } else { None };

    PaginationResult {
        content: paginated_content,
        total_length,
        has_more,
        next_offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_pagination_defaults() {
        let max_output = fuyao_api::get_config()
            .tools
            .limits
            .webfetch_max_output_chars;
        let (offset, limit) = validate_pagination(None, None);
        assert_eq!(offset, 0);
        assert_eq!(limit, max_output);
    }

    #[test]
    fn validate_pagination_clamps_limit() {
        let max_output = fuyao_api::get_config()
            .tools
            .limits
            .webfetch_max_output_chars;
        let (_, limit) = validate_pagination(None, Some(0));
        assert_eq!(limit, 1);

        let (_, limit) = validate_pagination(None, Some(max_output + 100));
        assert_eq!(limit, max_output);
    }

    #[test]
    fn apply_pagination_basic() {
        let content = "Hello, World!";
        let result = apply_pagination(content, 0, 5);
        assert_eq!(result.content, "Hello");
        assert_eq!(result.total_length, 13);
        assert!(result.has_more);
        assert_eq!(result.next_offset, Some(5));
    }

    #[test]
    fn apply_pagination_offset_beyond_content() {
        let content = "Short";
        let result = apply_pagination(content, 100, 10);
        assert!(result.content.is_empty());
        assert_eq!(result.total_length, 5);
        assert!(!result.has_more);
        assert!(result.next_offset.is_none());
    }

    #[test]
    fn apply_pagination_exact_fit() {
        let content = "12345";
        let result = apply_pagination(content, 0, 5);
        assert_eq!(result.content, "12345");
        assert!(!result.has_more);
        assert!(result.next_offset.is_none());
    }
}
