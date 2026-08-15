//! Shell 选择（自动探测 + 显式配置）
//!
//! 两种模式，由 `[tools.terminal].shell` 决定：
//! - `auto`（默认）：自动探测。Windows 优先级：Git Bash > PowerShell > cmd；Unix 优先级：bash > sh
//! - 显式名（与 shell_type 词表同名）：按名定位二进制，非法名或定位失败由引擎
//!   启动校验拒绝启动（fail loud）

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

/// shell 配置的自动探测值
const SHELL_AUTO: &str = "auto";

/// shell 显式名合法词表（与 `ShellInfo::shell_type` 同名）
///
/// 供启动校验与错误提示共用；`auto` 不在其中（单独短路）。
pub const SHELL_EXPLICIT_NAMES: [&str; 5] = ["git_bash", "powershell", "cmd", "bash", "sh"];

/// 缓存 Shell 检测结果（进程生命周期内不变）
static SHELL_INFO: LazyLock<ShellInfo> = LazyLock::new(detect_shell);

/// 查找可用的 shell（缓存结果）
///
/// 配置为 `auto` 时走自动探测（Windows: Git Bash > PowerShell > cmd，Unix: bash > sh）；
/// 配置显式名时按名定位二进制，定位失败告警后回退自动探测——正常流程下启动校验
/// 已拒绝非法配置，该回退是校验被跳过场景（如单测直调）的兜底，不 panic。
pub fn find_shell() -> &'static ShellInfo {
    &SHELL_INFO
}

/// 实际检测可用 shell
fn detect_shell() -> ShellInfo {
    let configured = fuyao_api::get_config().tools.terminal.shell.clone();
    if configured != SHELL_AUTO {
        if let Some(info) = resolve_explicit_shell(&configured) {
            return info;
        }
        tracing::warn!(shell = %configured, "配置的 shell 定位失败，回退自动探测");
    }
    detect_shell_auto()
}

/// 自动探测可用 shell（Windows: Git Bash > PowerShell > cmd，Unix: bash > sh）
fn detect_shell_auto() -> ShellInfo {
    if cfg!(windows) {
        find_git_bash()
            .or_else(find_powershell)
            .unwrap_or_else(find_cmd_fallback)
    } else {
        find_bash().unwrap_or_else(find_sh_fallback)
    }
}

/// 按显式名定位 shell 二进制（带存在性校验）
///
/// 名字必须在词表内，词表外返回 None。各名字的定位链：
/// - git_bash：从 PATH 上的 git.exe 推断同发行版的 bin\bash.exe
/// - powershell：pwsh 优先，powershell 兜底
/// - cmd：PATH 查找，SystemRoot 推断回退
/// - bash：PATH 查找
/// - sh：PATH 查找，$SHELL 回退
fn resolve_explicit_shell(name: &str) -> Option<ShellInfo> {
    match name {
        "git_bash" => find_git_bash(),
        "powershell" => find_powershell(),
        "cmd" => find_cmd_explicit(),
        "bash" => find_bash(),
        "sh" => find_sh_explicit(),
        _ => None,
    }
}

/// 校验 `[tools.terminal].shell` 配置值（引擎启动期 fail loud 挂载点调用）
///
/// - `auto`：合法，走自动探测
/// - 词表内显式名：定位二进制，定位失败报错——显式配置是用户强意图，不静默换 shell
/// - 词表外（含空串）：报错并列出全部合法值
///
/// 返回 Err 时携带面向用户的错误信息与修正建议。
pub fn validate_shell_name(configured: &str) -> Result<(), String> {
    if configured == SHELL_AUTO {
        return Ok(());
    }
    if !SHELL_EXPLICIT_NAMES.contains(&configured) {
        return Err(format!(
            "shell = \"{configured}\" 不在合法值内（合法值：auto | {}），请修正 [tools.terminal].shell",
            SHELL_EXPLICIT_NAMES.join(" | ")
        ));
    }
    if resolve_explicit_shell(configured).is_none() {
        return Err(format!(
            "shell = \"{configured}\" 的可执行文件未找到（PATH 与回退位置均无），请安装对应 shell 或改回 \"auto\""
        ));
    }
    Ok(())
}

/// 按 shell 类型生成工具描述末尾的语法提示行
///
/// Windows 系 shell 的命令语法与 Unix 假设不同，需向模型披露实际执行环境；
/// bash / sh 与默认 Unix 假设一致，不追加。纯函数便于单测。
pub(crate) fn shell_syntax_hint(shell_type: &str) -> Option<&'static str> {
    match shell_type {
        "git_bash" => {
            Some("命令经 Git Bash 执行，使用 Unix 语法（路径用 /，不支持 dir/del 等 CMD 命令）。")
        }
        "powershell" => Some("命令经 PowerShell 执行。"),
        "cmd" => Some("命令经 CMD 执行（路径用 \\）。"),
        _ => None,
    }
}

// ==================== 各 shell 定位器 ====================

/// Git Bash 探测：从 PATH 上的 git.exe 推断同发行版的 bin\bash.exe
fn find_git_bash() -> Option<ShellInfo> {
    let git_path = which("git")?;
    let git_dir = PathBuf::from(&git_path);
    let root = git_dir.parent()?.parent()?;
    let bash_path = root.join("bin").join("bash.exe");
    if bash_path.is_file() {
        Some(ShellInfo {
            path: bash_path.to_string_lossy().to_string(),
            arg: "-c",
            shell_type: "git_bash",
        })
    } else {
        None
    }
}

/// PowerShell 探测：pwsh 优先，powershell 兜底
fn find_powershell() -> Option<ShellInfo> {
    which("pwsh")
        .or_else(|| which("powershell"))
        .map(|path| ShellInfo {
            path,
            arg: "-Command",
            shell_type: "powershell",
        })
}

/// bash 探测：PATH 查找
fn find_bash() -> Option<ShellInfo> {
    which("bash").map(|path| ShellInfo {
        path,
        arg: "-c",
        shell_type: "bash",
    })
}

/// cmd 显式解析：PATH 查找，SystemRoot 推断回退，均校验存在性
///
/// 显式配置必须真实可用，任何一级候选定位不到文件即失败（交由启动校验报错）。
fn find_cmd_explicit() -> Option<ShellInfo> {
    let path = which("cmd").or_else(|| {
        std::env::var("SystemRoot")
            .ok()
            .map(|root| format!("{root}\\System32\\cmd.exe"))
    })?;
    if PathBuf::from(&path).is_file() {
        Some(ShellInfo {
            path,
            arg: "/c",
            shell_type: "cmd",
        })
    } else {
        None
    }
}

/// cmd 兜底解析（自动探测链终点，始终有结果）
///
/// PATH 查找 → SystemRoot 推断 → 字面量 cmd.exe，末级不校验存在性：
/// cmd 是 Windows 出厂必带的兜底，探测链落到末级说明环境已异常，给出可诊断路径即可。
fn find_cmd_fallback() -> ShellInfo {
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
}

/// sh 显式解析：PATH 查找，$SHELL 回退，均校验存在性
fn find_sh_explicit() -> Option<ShellInfo> {
    let path = which("sh").or_else(|| std::env::var("SHELL").ok())?;
    if PathBuf::from(&path).is_file() {
        Some(ShellInfo {
            path,
            arg: "-c",
            shell_type: "sh",
        })
    } else {
        None
    }
}

/// sh 兜底解析（自动探测链终点，始终有结果）
///
/// $SHELL 环境变量 → /bin/sh 字面量，不校验存在性：Unix 出厂必带 /bin/sh。
fn find_sh_fallback() -> ShellInfo {
    let sh_path = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    ShellInfo {
        path: sh_path,
        arg: "-c",
        shell_type: "sh",
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

    /// shell_syntax_hint：Windows 系三种 shell 各有语法提示，bash / sh / 未知无提示
    #[test]
    fn shell_syntax_hint_branches() {
        assert_eq!(
            shell_syntax_hint("git_bash"),
            Some("命令经 Git Bash 执行，使用 Unix 语法（路径用 /，不支持 dir/del 等 CMD 命令）。")
        );
        assert_eq!(
            shell_syntax_hint("powershell"),
            Some("命令经 PowerShell 执行。")
        );
        assert_eq!(
            shell_syntax_hint("cmd"),
            Some("命令经 CMD 执行（路径用 \\）。")
        );
        assert_eq!(shell_syntax_hint("bash"), None);
        assert_eq!(shell_syntax_hint("sh"), None);
        assert_eq!(shell_syntax_hint("fish"), None);
    }

    /// validate_shell_name：auto 合法（默认走自动探测）
    #[test]
    fn validate_shell_name_auto_ok() {
        assert!(validate_shell_name("auto").is_ok());
    }

    /// validate_shell_name：词表外（空串 / 拼错 / 大小写不符）报错，回显原值并列出合法值
    #[test]
    fn validate_shell_name_unknown_rejected() {
        for bad in ["", "zsh", "gitbash", "Powershell", "auto "] {
            let err = validate_shell_name(bad).unwrap_err();
            assert!(
                err.contains("合法值"),
                "值 {bad:?} 的报错应列出合法值：{err}"
            );
            assert!(err.contains("auto"), "值 {bad:?} 的报错应提示 auto：{err}");
            assert!(err.contains(bad), "值 {bad:?} 的报错应回显原值：{err}");
        }
    }

    /// validate_shell_name：词表内名字不会触发词表错误——要么定位成功，要么报定位失败
    /// （定位结果依赖当前机器，两种结果都合法，仅约束错误分类）
    #[test]
    fn validate_shell_name_in_vocab_never_vocab_error() {
        for name in SHELL_EXPLICIT_NAMES {
            if let Err(err) = validate_shell_name(name) {
                assert!(
                    err.contains("未找到"),
                    "词表内 {name} 只允许定位失败报错：{err}"
                );
            }
        }
    }

    /// resolve_explicit_shell：词表外名字一律 None
    #[test]
    fn resolve_explicit_shell_unknown_name_is_none() {
        assert!(resolve_explicit_shell("").is_none());
        assert!(resolve_explicit_shell("zsh").is_none());
        assert!(resolve_explicit_shell("auto").is_none());
    }

    /// resolve_explicit_shell：词表内名字解析成功时 shell_type 与名字一致
    #[test]
    fn resolve_explicit_shell_type_matches_name() {
        for name in SHELL_EXPLICIT_NAMES {
            if let Some(info) = resolve_explicit_shell(name) {
                assert_eq!(info.shell_type, name);
            }
        }
    }
}
