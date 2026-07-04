//! 退出码解读
//!
//! 某些退出码不代表真正的错误（如 grep=1 表示无匹配）。

use regex::Regex;
use std::sync::LazyLock;

/// 命令分隔符正则（缓存）
static COMMAND_SEPARATOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*(?:\|\||&&|[|;])\s*").expect("无效的退出码正则表达式"));

/// 解读常见非零退出码
pub fn interpret_exit_code(command: &str, exit_code: i32) -> Option<String> {
    if exit_code == 0 {
        return None;
    }

    // 拆分管道/链式命令，取最后一段
    let segments: Vec<&str> = COMMAND_SEPARATOR_RE.split(command).collect();
    let last_segment = segments.last().unwrap_or(&command).trim();

    // 跳过环境变量赋值，取第一个真正的命令
    let cmd_name = last_segment
        .split_whitespace()
        .find(|w| !w.contains('=') || w.starts_with('-'))
        .unwrap_or("");

    if cmd_name.is_empty() {
        return None;
    }

    // 去除路径前缀
    let cmd_base = cmd_name
        .rsplit('/')
        .next()
        .unwrap_or(cmd_name)
        .strip_suffix(".exe")
        .unwrap_or(cmd_name);

    // 命令退出码语义表
    let meanings: &[(&str, &[(i32, &str)])] = &[
        // grep 系列: 1=无匹配（正常）
        ("grep", &[(1, "无匹配结果")]),
        ("egrep", &[(1, "无匹配结果")]),
        ("fgrep", &[(1, "无匹配结果")]),
        ("rg", &[(1, "无匹配结果")]),
        ("ag", &[(1, "无匹配结果")]),
        ("ack", &[(1, "无匹配结果")]),
        // diff: 1=文件不同
        ("diff", &[(1, "文件内容不同")]),
        ("colordiff", &[(1, "文件内容不同")]),
        // find: 1=部分目录不可访问
        ("find", &[(1, "部分目录不可访问（结果可能仍有效）")]),
        // test/[: 1=条件不成立
        ("test", &[(1, "测试条件不成立")]),
        ("[", &[(1, "测试条件不成立")]),
        // curl: 常见非错误码
        (
            "curl",
            &[
                (6, "DNS 解析失败"),
                (7, "连接被拒绝"),
                (22, "HTTP 错误（如 404、500）"),
                (28, "请求超时"),
            ],
        ),
        // git: 1 通常正常
        (
            "git",
            &[(1, "非零退出（通常正常，如 git diff 有差异时返回 1）")],
        ),
    ];

    for (name, codes) in meanings {
        if cmd_base == *name {
            for (code, meaning) in *codes {
                if exit_code == *code {
                    return Some(meaning.to_string());
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpret_exit_code_grep_no_match() {
        let result = interpret_exit_code("grep pattern file", 1);
        assert_eq!(result, Some("无匹配结果".to_string()));
    }

    #[test]
    fn interpret_exit_code_zero() {
        let result = interpret_exit_code("echo hello", 0);
        assert!(result.is_none());
    }

    #[test]
    fn interpret_exit_code_unknown() {
        let result = interpret_exit_code("some_cmd", 42);
        assert!(result.is_none());
    }

    #[test]
    fn interpret_exit_code_curl_timeout() {
        let result = interpret_exit_code("curl http://example.com", 28);
        assert_eq!(result, Some("请求超时".to_string()));
    }
}
