//! 命令执行
//!
//! 进程组杀死、命令执行、结果格式化。

use super::exit_code::interpret_exit_code;
use super::output::{decode_output, strip_ansi, truncate_output};
use super::safety::build_safe_env;
use super::shell::ShellInfo;
use super::types::BashToolResult;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

// =========== 进程组杀死 ===========

/// 杀死进程，Unix 上杀死整个进程组（含子进程）
fn kill_process(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        // 通过 kill 进程组来杀死所有子进程
        if let Some(id) = child.id() {
            unsafe {
                libc::killpg(id as i32, libc::SIGTERM);
            }
        }
    }

    // Windows 或 Unix fallback：杀主进程
    let _ = child.start_kill();
}

// =========== 命令执行结果 ===========

/// 命令执行结果
#[derive(Debug)]
pub struct TerminalResult {
    /// 是否成功（exit_code == 0）
    pub success: bool,
    /// 标准输出（stdout + stderr 合并）
    pub output: String,
    /// 进程退出码
    pub exit_code: i32,
    /// 执行错误信息
    pub error: Option<String>,
    /// 是否超时
    pub timed_out: bool,
    /// 命令执行时的工作目录
    pub working_dir: Option<String>,
    /// 执行耗时（毫秒）
    pub execution_time_ms: Option<f64>,
    /// 退出码解读
    pub exit_code_meaning: Option<String>,
    /// Shell 类型
    pub shell_type: &'static str,
}

// =========== 命令执行 ===========

/// 执行 shell 命令
///
/// 在指定目录下执行命令，带超时控制、环境变量屏蔽和进程组杀死。
/// stderr 通过 shell 重定向合并到 stdout（2>&1），保持输出交错顺序。
pub async fn execute_command(
    command: &str,
    working_dir: Option<&Path>,
    timeout: Duration,
    shell_info: &ShellInfo,
) -> TerminalResult {
    let start = Instant::now();
    let safe_env = build_safe_env();

    // 在 shell 层面合并 stderr 到 stdout，保持输出交错顺序
    // 与 Python 的 stderr=STDOUT 行为一致
    let merged_command = format!("{command} 2>&1");

    let mut cmd = tokio::process::Command::new(&shell_info.path);
    cmd.arg(shell_info.arg)
        .arg(&merged_command)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .envs(&safe_env);

    // Unix: 创建新进程组，超时可杀整组
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            return TerminalResult {
                success: false,
                output: String::new(),
                exit_code: -1,
                error: Some(format!("命令执行失败: {e}")),
                timed_out: false,
                working_dir: working_dir.map(|p| p.to_string_lossy().to_string()),
                execution_time_ms: Some(elapsed_ms),
                exit_code_meaning: None,
                shell_type: shell_info.shell_type,
            };
        }
    };

    // 带超时等待
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            let exit_code = status.code().unwrap_or(-1);

            // 读取 stdout（stderr 已通过 2>&1 合并到 stdout）
            let stdout_bytes = match child.stdout.take() {
                Some(mut s) => {
                    let mut buf = Vec::new();
                    let _ = tokio::io::AsyncReadExt::read_to_end(&mut s, &mut buf).await;
                    buf
                }
                None => Vec::new(),
            };

            // 解码输出
            let mut combined = decode_output(&stdout_bytes);

            // 清理 ANSI 转义
            combined = strip_ansi(&combined);

            // 截断过长输出
            combined = truncate_output(combined.trim());

            // 解读退出码
            let exit_code_meaning = interpret_exit_code(command, exit_code);

            TerminalResult {
                success: exit_code == 0,
                output: combined,
                exit_code,
                error: None,
                timed_out: false,
                working_dir: working_dir.map(|p| p.to_string_lossy().to_string()),
                execution_time_ms: Some(elapsed_ms),
                exit_code_meaning,
                shell_type: shell_info.shell_type,
            }
        }
        Ok(Err(e)) => {
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            TerminalResult {
                success: false,
                output: String::new(),
                exit_code: -1,
                error: Some(format!("命令执行错误: {e}")),
                timed_out: false,
                working_dir: working_dir.map(|p| p.to_string_lossy().to_string()),
                execution_time_ms: Some(elapsed_ms),
                exit_code_meaning: None,
                shell_type: shell_info.shell_type,
            }
        }
        Err(_) => {
            // 超时，杀死进程组
            kill_process(&mut child);
            // Windows 上 Child::drop 会调用 wait()，而 kill 后 wait 可能 panic（Rust 标准库 bug）
            // 使用 forget 避免 drop，让 OS 回收进程资源
            std::mem::forget(child);

            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            TerminalResult {
                success: false,
                output: String::new(),
                exit_code: 124,
                error: Some(format!("命令在 {} 秒后超时", timeout.as_secs())),
                timed_out: true,
                working_dir: working_dir.map(|p| p.to_string_lossy().to_string()),
                execution_time_ms: Some(elapsed_ms),
                exit_code_meaning: None,
                shell_type: shell_info.shell_type,
            }
        }
    }
}

// =========== 结果格式化 ===========

/// 格式化 TerminalResult 为 JSON 字符串
pub fn format_result(result: TerminalResult) -> String {
    // 无输出时填充提示，避免 AI 误判为异常
    let output = if result.output.trim().is_empty() && result.success {
        "（无输出）".to_string()
    } else {
        result.output
    };

    let output = BashToolResult {
        success: result.success,
        output,
        exit_code: result.exit_code,
        error: result.error,
        timed_out: result.timed_out,
        working_dir: result.working_dir,
        execution_time_ms: result
            .execution_time_ms
            .map(|ms| (ms * 10.0).round() / 10.0),
        exit_code_meaning: result.exit_code_meaning,
        shell_type: result.shell_type.to_string(),
    };
    serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_result_success() {
        let result = TerminalResult {
            success: true,
            output: "hello".to_string(),
            exit_code: 0,
            error: None,
            timed_out: false,
            working_dir: Some("/tmp".to_string()),
            execution_time_ms: Some(100.0),
            exit_code_meaning: None,
            shell_type: "bash",
        };
        let json = format_result(result);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["output"], "hello");
        assert_eq!(parsed["exit_code"], 0);
        assert_eq!(parsed["shell_type"], "bash");
    }

    #[test]
    fn format_result_timeout() {
        let result = TerminalResult {
            success: false,
            output: String::new(),
            exit_code: 124,
            error: Some("命令在 1 秒后超时".to_string()),
            timed_out: true,
            working_dir: None,
            execution_time_ms: Some(1000.0),
            exit_code_meaning: None,
            shell_type: "bash",
        };
        let json = format_result(result);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["timed_out"], true);
        assert_eq!(parsed["exit_code"], 124);
    }

    #[test]
    fn format_result_empty_output_success() {
        // 成功但无输出时，填充"（无输出）"提示
        let result = TerminalResult {
            success: true,
            output: String::new(),
            exit_code: 0,
            error: None,
            timed_out: false,
            working_dir: None,
            execution_time_ms: Some(50.0),
            exit_code_meaning: None,
            shell_type: "bash",
        };
        let json = format_result(result);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["output"], "（无输出）");
    }

    #[test]
    fn format_result_empty_output_failure() {
        // 失败且无输出时，不填充提示
        let result = TerminalResult {
            success: false,
            output: String::new(),
            exit_code: 1,
            error: Some("命令执行失败".to_string()),
            timed_out: false,
            working_dir: None,
            execution_time_ms: Some(50.0),
            exit_code_meaning: None,
            shell_type: "bash",
        };
        let json = format_result(result);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["output"], "");
    }

    #[tokio::test]
    async fn execute_command_timeout() {
        let shell_info = super::super::shell::find_shell();
        let command = if cfg!(windows) {
            "ping -n 6 127.0.0.1 > NUL"
        } else {
            "sleep 5"
        };
        let result = execute_command(command, None, Duration::from_secs(1), shell_info).await;
        assert!(result.timed_out);
        assert_eq!(result.exit_code, 124);
    }
}
