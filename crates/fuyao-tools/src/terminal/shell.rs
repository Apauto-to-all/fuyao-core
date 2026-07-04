//! Shell 自动选择
//!
//! Windows 优先级：Git Bash > PowerShell > cmd
//! Unix 优先级：bash > sh

use std::path::PathBuf;
use std::sync::LazyLock;

/// Shell 信息
#[derive(Debug, Clone)]
pub struct ShellInfo {
    /// Shell 可执行文件路径
    pub path: String,
    /// Shell 参数（-c / -Command / /c）
    pub arg: &'static str,
    /// Shell 类型标识
    pub shell_type: &'static str,
}

/// 缓存 Shell 检测结果（进程生命周期内不变）
static SHELL_INFO: LazyLock<ShellInfo> = LazyLock::new(detect_shell);

/// 查找可用的 shell（缓存结果）
///
/// Windows 优先级：Git Bash > PowerShell > cmd
/// Unix 优先级：bash > sh
pub fn find_shell() -> &'static ShellInfo {
    &SHELL_INFO
}

/// 实际检测可用 shell
fn detect_shell() -> ShellInfo {
    if cfg!(windows) {
        // 1. 优先 Git Bash：从 git.exe 推断 bin\bash.exe
        if let Some(git_path) = which("git") {
            let git_root = PathBuf::from(&git_path);
            let root = git_root.parent().and_then(|p| p.parent());
            if let Some(root) = root {
                let bash_path = root.join("bin").join("bash.exe");
                if bash_path.is_file() {
                    return ShellInfo {
                        path: bash_path.to_string_lossy().to_string(),
                        arg: "-c",
                        shell_type: "git_bash",
                    };
                }
            }
        }

        // 2. PowerShell（pwsh 优先，fallback powershell）
        if let Some(pwsh) = which("pwsh") {
            return ShellInfo {
                path: pwsh,
                arg: "-Command",
                shell_type: "powershell",
            };
        }
        if let Some(ps) = which("powershell") {
            return ShellInfo {
                path: ps,
                arg: "-Command",
                shell_type: "powershell",
            };
        }

        // 3. 最后 cmd
        let cmd_path = which("cmd").unwrap_or_else(|| {
            std::env::var("SystemRoot")
                .map(|root| format!("{root}\\System32\\cmd.exe"))
                .unwrap_or_else(|_| "cmd.exe".to_string())
        });
        ShellInfo {
            path: cmd_path,
            arg: "/c",
            shell_type: "cmd",
        }
    } else {
        // Unix-like: bash > sh
        if let Some(bash) = which("bash") {
            return ShellInfo {
                path: bash,
                arg: "-c",
                shell_type: "bash",
            };
        }

        let sh_path = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        ShellInfo {
            path: sh_path,
            arg: "-c",
            shell_type: "sh",
        }
    }
}

/// 在 PATH 中查找可执行文件
fn which(name: &str) -> Option<String> {
    let (cmd, arg) = if cfg!(windows) {
        ("where", format!("{name}.exe"))
    } else {
        ("which", name.to_string())
    };

    let output = std::process::Command::new(cmd).arg(&arg).output().ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|p| PathBuf::from(p).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_shell_returns_valid() {
        let shell = find_shell();
        assert!(!shell.path.is_empty());
        assert!(!shell.shell_type.is_empty());
        // shell_type 应该是已知类型之一
        assert!(matches!(
            shell.shell_type,
            "git_bash" | "powershell" | "cmd" | "bash" | "sh"
        ));
    }
}
