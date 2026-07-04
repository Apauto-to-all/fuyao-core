//! 内容转换
//!
//! HTML → Markdown 转换，包含噪音元素移除和格式化。

use htmd::HtmlToMarkdown;
use scraper::{Html, Selector};

/// 结构性噪音标签（整个元素移除，包括内容）
const NOISE_TAGS: &[&str] = &[
    "script", "style", "meta", "link", "noscript", "iframe", "title", "nav", "header", "footer",
    "aside",
];

/// 噪音 class/id 关键词（小写匹配）
const NOISE_PATTERNS: &[&str] = &[
    "sidebar",
    "nav",
    "footer",
    "header",
    "menu",
    "breadcrumb",
    "toc",
    "table-of-contents",
    "theme",
    "dark-mode",
    "light-mode",
    "permalink",
    "edit-page",
    "source-link",
    "page-header",
    "site-header",
    "site-footer",
    "page-footer",
    "page-nav",
    "site-nav",
    "top-nav",
    "toolbar",
    "cookie",
    "banner",
    "complementary",
    "contentinfo",
    "navigation",
    "search-form",
    "search-box",
    "social",
    "share",
    "related",
    "comment",
    "feedback",
];

/// ¶ 锚点链接文本
const ANCHOR_TEXTS: &[&str] = &["¶", "🔗", "#", "Link to this heading"];

/// 判断元素的 class/id 是否匹配噪音模式
fn is_noise_element(element: &scraper::ElementRef) -> bool {
    let classes = element.value().attr("class").unwrap_or("");
    let element_id = element.value().attr("id").unwrap_or("");

    for val in classes
        .split_whitespace()
        .chain(std::iter::once(element_id))
    {
        if val.is_empty() {
            continue;
        }
        let lower = val.to_lowercase();
        if NOISE_PATTERNS.iter().any(|p| lower.contains(p)) {
            return true;
        }
    }
    false
}

/// 用 scraper 移除噪音元素，返回清理后的 HTML 字符串
fn clean_html(html: &str) -> String {
    let document = Html::parse_document(html);

    // 收集需要移除的元素的 HTML 范围
    // 策略：用 scraper 找到噪音元素，从原始 HTML 中移除它们的 outer_html
    let mut noise_htmls: Vec<String> = Vec::new();

    // 1. 移除结构性噪音标签
    for tag_name in NOISE_TAGS {
        if let Ok(selector) = Selector::parse(tag_name) {
            for element in document.select(&selector) {
                noise_htmls.push(element.html());
            }
        }
    }

    // 2. 移除带噪音 class/id 的元素
    if let Ok(selector) = Selector::parse("*") {
        for element in document.select(&selector) {
            if is_noise_element(&element) {
                noise_htmls.push(element.html());
            }
        }
    }

    // 3. 移除 ¶ 锚点链接
    if let Ok(selector) = Selector::parse("a") {
        for element in document.select(&selector) {
            let text: String = element.text().collect();
            let trimmed = text.trim();
            if ANCHOR_TEXTS.contains(&trimmed) {
                noise_htmls.push(element.html());
            }
        }
    }

    // 从原始 HTML 中移除噪音元素
    let mut result = html.to_string();
    for noise in &noise_htmls {
        result = result.replace(noise, "");
    }

    if result.trim().is_empty() {
        return html.to_string();
    }

    result
}

/// HTML → Markdown 转换
///
/// 流程：
/// 1. 用 scraper 移除结构性噪音标签和噪音 class/id 元素
/// 2. 移除 ¶ 锚点链接
/// 3. 用 htmd 转换为 Markdown
pub fn convert_html_to_markdown(html: &str) -> String {
    let cleaned = clean_html(html);

    let converter = HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "nav", "header", "footer", "aside"])
        .build();

    match converter.convert(&cleaned) {
        Ok(md) => md,
        Err(_) => match converter.convert(html) {
            Ok(md) => md,
            Err(_) => html.to_string(),
        },
    }
}

/// 根据格式转换内容
pub fn convert_content(content: &str, content_type: &str, output_format: &str) -> String {
    let is_html = content_type.to_lowercase().contains("text/html");

    if !is_html {
        return content.to_string();
    }

    if output_format == "html" {
        return content.to_string();
    }

    convert_html_to_markdown(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_html_to_markdown_heading() {
        let html = "<h1>Hello</h1><p>World</p>";
        let md = convert_html_to_markdown(html);
        assert!(md.contains("Hello"));
        assert!(md.contains("World"));
    }

    #[test]
    fn convert_html_to_markdown_removes_script() {
        let html = "<p>Content</p><script>alert('xss')</script>";
        let md = convert_html_to_markdown(html);
        assert!(md.contains("Content"));
        assert!(!md.contains("alert"));
    }

    #[test]
    fn convert_html_to_markdown_removes_nav() {
        let html = "<nav><a href='/'>Home</a></nav><p>Main content</p>";
        let md = convert_html_to_markdown(html);
        assert!(md.contains("Main content"));
    }

    #[test]
    fn convert_content_non_html_passthrough() {
        let content = "plain text content";
        let result = convert_content(content, "text/plain", "markdown");
        assert_eq!(result, "plain text content");
    }

    #[test]
    fn convert_content_html_format_passthrough() {
        let html = "<h1>Title</h1>";
        let result = convert_content(html, "text/html", "html");
        assert_eq!(result, "<h1>Title</h1>");
    }

    #[test]
    fn convert_content_html_to_markdown() {
        let html = "<h1>Title</h1><p>Paragraph</p>";
        let result = convert_content(html, "text/html", "markdown");
        assert!(result.contains("Title"));
        assert!(result.contains("Paragraph"));
    }
}
