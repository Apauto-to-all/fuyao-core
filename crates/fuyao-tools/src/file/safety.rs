//! 路径安全检查
//!
//! 提供文件路径安全检查功能，包含敏感路径保护、设备文件检测、二进制文件检测、
//! 框架内部路径保护、写入范围限制。
//!
//! ## 检查层级
//!
//! 1. **敏感路径前缀**: /etc/、/boot/、C:\Windows\ 等系统目录
//! 2. **敏感系统文件**: /etc/passwd、/etc/shadow 等关键配置
//! 3. **用户敏感文件**: ~/.ssh/id_rsa、~/.bashrc 等（主目录下检查）
//! 4. **特殊文件名**: .env、.htpasswd、authorized_keys（任意位置）
//! 5. **设备文件**: /dev/zero、/dev/random 等无限输出设备
//! 6. **二进制文件**: .exe、.png、.pdf 等不可读文本的文件
//! 7. **框架内部路径**: .fuyao/.env（防止 Agent 读取框架敏感数据）
//! 8. **写入范围限制**: write / edit 仅允许落在工作目录与 fuyao 数据目录内的路径

use std::path::Path;

use fuyao_api::ToolCallContext;

use crate::common::is_within;

/// 二进制文件扩展名，不建议直接访问
const BINARY_EXTENSIONS: &[&str] = &[
    ".pyc", ".pyo", ".so", ".dll", ".dylib", ".exe", ".bin", ".png", ".jpg", ".jpeg", ".gif",
    ".bmp", ".ico", ".webp", ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".zip",
    ".tar", ".gz", ".rar", ".7z", ".bz2", ".mp3", ".mp4", ".avi", ".mov", ".wav", ".flac", ".db",
    ".sqlite", ".sqlite3",
];

/// 设备文件路径，不建议直接访问
const BLOCKED_DEVICE_PATHS: &[&str] = &[
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/full",
    "/dev/stdin",
    "/dev/tty",
    "/dev/console",
    "/dev/stdout",
    "/dev/stderr",
    "/dev/fd/0",
    "/dev/fd/1",
    "/dev/fd/2",
];

/// 内部路径后缀，不建议直接访问
/// 只保护 .env（API Keys 等敏感数据），其他路径放开让 AI 能自主排查问题
const INTERNAL_PATH_SUFFIXES: &[&str] = &["/.env"];

/// 敏感路径前缀，不建议直接访问
const SENSITIVE_PATH_PREFIXES: &[&str] = &[
    "/etc/",
    "/boot/",
    "/usr/lib/systemd/",
    "/private/etc/",
    "/private/var/",
    "C:\\Windows\\",
    "C:\\Program Files\\",
    "C:\\Program Files (x86)\\",
    "/sys/",
    "/proc/",
    "/dev/",
];

/// 敏感文件名，不建议直接访问
const DENIED_FILENAMES: &[&str] = &[
    ".ssh/authorized_keys",
    ".ssh/authorized_keys2",
    ".ssh/id_rsa",
    ".ssh/id_rsa.pub",
    ".ssh/id_dsa",
    ".ssh/id_dsa.pub",
    ".ssh/id_ecdsa",
    ".ssh/id_ecdsa.pub",
    ".ssh/id_ed25519",
    ".ssh/id_ed25519.pub",
    ".ssh/known_hosts",
    ".bashrc",
    ".bash_profile",
    ".bash_logout",
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".zlogin",
    ".zlogout",
    ".profile",
    ".bash_history",
    ".zsh_history",
    ".sh_history",
    ".gitconfig",
    ".gitignore_global",
    ".npmrc",
    ".pypirc",
    ".netrc",
    ".pgpass",
    ".my.cnf",
    ".mylogin.cnf",
    ".env",
    ".htpasswd",
    "authorized_keys",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

/// 敏感系统路径，不建议直接访问
const DENIED_SYSTEM_PATHS: &[&str] = &[
    "/etc/passwd",
    "/etc/shadow",
    "/etc/sudoers",
    "/etc/hosts",
    "/etc/resolv.conf",
    "/etc/fstab",
    "/etc/crontab",
    "/etc/anacrontab",
    "/etc/cron.d/",
    "/etc/cron.daily/",
    "/etc/cron.hourly/",
    "/etc/cron.monthly/",
    "/etc/cron.weekly/",
    "/etc/ssh/sshd_config",
    "/etc/ssh/ssh_config",
    "/etc/nginx/nginx.conf",
    "/etc/apache2/apache2.conf",
    "/etc/apache2/httpd.conf",
    "/etc/httpd/conf/httpd.conf",
    "/etc/systemd/system/",
    "/etc/init.d/",
    "/etc/rc.local",
    "/boot/grub/grub.cfg",
    "/boot/grub2/grub.cfg",
    "/proc/",
    "/sys/",
    "/dev/",
];

/// 标准化路径，处理 ~、相对路径等
fn normalize_path(filepath: &str) -> String {
    let expanded = crate::common::expand_tilde(filepath);
    let path = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default().join(&expanded)
    };
    // dunce::canonicalize doesn't exist, just normalize
    path.to_string_lossy().to_string()
}

/// 检查是否为敏感系统路径
///
/// 依次检查：敏感路径前缀 → 敏感系统文件 → 用户敏感文件 → 特殊文件名。
/// Windows 路径不区分大小写比较。
///
/// # 参数
///
/// - `filepath`: 待检查的文件路径
/// - `action`: 操作类型（"写入"、"修改"），用于错误信息
///
/// # 返回
///
/// `None` 表示安全，`Some(错误信息)` 表示拒绝。
pub fn check_sensitive_path(filepath: &str, action: &str) -> Option<String> {
    let normalized = normalize_path(filepath);
    let normalized_lower = normalized.to_lowercase();

    for prefix in SENSITIVE_PATH_PREFIXES {
        let prefix_check = prefix.to_lowercase();
        if normalized_lower.starts_with(&prefix_check) || normalized.starts_with(prefix) {
            return Some(format!("拒绝{action}敏感系统路径: {filepath}"));
        }
    }

    for denied_path in DENIED_SYSTEM_PATHS {
        let denied_check = denied_path.to_lowercase();
        if normalized_lower == denied_check || normalized_lower.starts_with(&denied_check) {
            return Some(format!("拒绝{action}系统关键文件: {filepath}"));
        }
    }

    // 检查用户主目录下的敏感文件
    if let Some(home) = crate::common::dirs_home()
        && let Ok(rel) = Path::new(&normalized).strip_prefix(&home)
    {
        let rel_normalized = rel.to_string_lossy().replace('\\', "/");
        for denied_file in DENIED_FILENAMES {
            if rel_normalized == *denied_file
                || rel_normalized.starts_with(&format!("{denied_file}/"))
            {
                return Some(format!("拒绝{action}敏感用户文件: ~/{denied_file}"));
            }
        }
    }

    let basename = Path::new(&normalized)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if ["env", "htpasswd", "authorized_keys"]
        .iter()
        .any(|s| basename == format!(".{s}") || basename == *s)
    {
        return Some(format!("拒绝{action}敏感文件: {basename}"));
    }

    None
}

/// 检查写入路径是否落在允许清单内（write / edit 共用的写入范围判定）
///
/// 允许清单两条，命中任一即放行：
/// 1. `workspace` 工作目录（含其子树；workspace 层 agent 数据在
///    `{workspace}/.fuyao/` 内部，天然被本条覆盖）
/// 2. `fuyao_home` 整个子树（含其下 fuyao-agents、logs 等全部数据目录）
///
/// # 返回
///
/// `None` 表示放行，`Some(错误信息)` 表示拒绝（信息含被拒路径，可直接进错误信封）。
pub fn check_write_scope(
    resolved: &Path,
    workspace: Option<&Path>,
    fuyao_home: Option<&Path>,
) -> Option<String> {
    if let Some(ws) = workspace
        && is_within(resolved, ws)
    {
        return None;
    }

    if let Some(home) = fuyao_home
        && is_within(resolved, home)
    {
        return None;
    }

    Some(format!(
        "写入被限制在工作目录内，拒绝写入: {}",
        resolved.display()
    ))
}

/// 基于调用上下文判定写入范围（handler 入口共用前奏）
///
/// 组装 [`check_write_scope`] 的两个入参：
/// - workspace：`ctx.agent_paths.workspace`，缺省回退进程当前目录
/// - fuyao_home：`ctx.agent_paths.fuyao_home`，agent_paths 缺失时取全局默认
pub(crate) fn check_write_scope_ctx(ctx: &ToolCallContext, resolved: &Path) -> Option<String> {
    let workspace = ctx
        .workspace()
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok());
    let fuyao_home = ctx
        .agent_paths
        .as_ref()
        .map(|ap| ap.fuyao_home.clone())
        .unwrap_or_else(fuyao_api::get_fuyao_home);
    check_write_scope(resolved, workspace.as_deref(), Some(&fuyao_home))
}

/// 检查是否为框架内部路径
///
/// 防止 Agent 读取框架内部的缓存、配置、会话等文件，避免 prompt injection 攻击。
/// 目前只保护 `.fuyao/.env`（API Keys 等敏感数据），其他路径放开让 AI 自主排查问题。
pub fn is_internal_path(filepath: &str) -> bool {
    let expanded = crate::common::expand_tilde(filepath);
    let resolved_norm = expanded.to_string_lossy().replace('\\', "/");

    let fuyao_dir_idx = match resolved_norm.find("/.fuyao") {
        Some(idx) => idx,
        None => return false,
    };

    let fuyao_dir_end = fuyao_dir_idx + "/.fuyao".len();
    let rel_path = &resolved_norm[fuyao_dir_end..];

    for suffix in INTERNAL_PATH_SUFFIXES {
        if !suffix.ends_with('/') {
            let filename = suffix.trim_start_matches('/');
            if !filename.is_empty() {
                let basename = Path::new(filepath)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if basename == filename {
                    return true;
                }
            }
        } else {
            let dir_path = suffix.trim_end_matches('/');
            if rel_path == dir_path || rel_path.starts_with(&format!("{dir_path}/")) {
                return true;
            }
        }
    }

    false
}

/// 检查是否为二进制文件扩展名
///
/// 匹配 .exe、.png、.pdf、.zip 等 31 种常见二进制格式。
/// 大小写不敏感（.PNG 和 .png 均匹配）。
pub fn has_binary_extension(filepath: &str) -> bool {
    let ext = Path::new(filepath)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    BINARY_EXTENSIONS.iter().any(|e| *e == format!(".{ext}"))
}

/// 检查是否为阻塞设备文件（无限输出或阻塞输入）
///
/// 匹配 /dev/zero、/dev/random 等设备，以及 /proc/self/fd/ 等 proc 文件系统路径。
pub fn is_blocked_device(filepath: &str) -> bool {
    let expanded = crate::common::expand_tilde(filepath);
    let path_str = expanded.to_string_lossy();

    if BLOCKED_DEVICE_PATHS.iter().any(|p| path_str == *p) {
        return true;
    }

    if path_str.starts_with("/proc/")
        && (path_str.ends_with("/fd/0")
            || path_str.ends_with("/fd/1")
            || path_str.ends_with("/fd/2"))
    {
        return true;
    }

    if path_str.starts_with("/proc/self/fd/") {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_extension_detection() {
        assert!(has_binary_extension("test.png"));
        assert!(has_binary_extension("test.EXE"));
        assert!(!has_binary_extension("test.rs"));
        assert!(!has_binary_extension("test.txt"));
    }

    #[test]
    fn blocked_device_detection() {
        assert!(is_blocked_device("/dev/zero"));
        assert!(is_blocked_device("/dev/random"));
        assert!(!is_blocked_device("/home/user/file.txt"));
    }

    #[test]
    fn sensitive_path_detection() {
        // 使用平台相关的路径进行测试
        if cfg!(windows) {
            assert!(check_sensitive_path("C:\\Windows\\system32", "写入").is_some());
            assert!(check_sensitive_path("C:\\Program Files\\test", "写入").is_some());
        } else {
            assert!(check_sensitive_path("/etc/passwd", "写入").is_some());
            assert!(check_sensitive_path("/etc/shadow", "写入").is_some());
            assert!(check_sensitive_path("/boot/grub/grub.cfg", "写入").is_some());
        }
    }

    #[test]
    fn normal_path_not_sensitive() {
        assert!(check_sensitive_path("/home/user/project/src/main.rs", "写入").is_none());
    }

    #[test]
    fn internal_path_detection() {
        assert!(is_internal_path("/home/user/.fuyao/.env"));
        assert!(!is_internal_path("/home/user/project/.env"));
    }

    // ── check_write_scope ───────────────────────────────────────

    /// workspace 子树内放行；workspace 本身视作在内；子树外拒绝
    #[test]
    fn write_scope_workspace_boundary() {
        let ws = Path::new("/tmp/project");
        assert!(check_write_scope(Path::new("/tmp/project/main.rs"), Some(ws), None).is_none());
        assert!(check_write_scope(Path::new("/tmp/project"), Some(ws), None).is_none());
        assert!(check_write_scope(Path::new("/tmp/project2/main.rs"), Some(ws), None).is_some());
        assert!(check_write_scope(Path::new("/other/main.rs"), Some(ws), None).is_some());
    }

    /// fuyao_home 整个子树放行：fuyao-agents、logs、根文件均在允许范围；子树外拒绝
    #[test]
    fn write_scope_fuyao_home_subtree() {
        let home = Path::new("/tmp/home");
        // fuyao-agents 数据目录
        assert!(
            check_write_scope(
                Path::new("/tmp/home/fuyao-agents/coder/notes.md"),
                None,
                Some(home)
            )
            .is_none()
        );
        // 非 fuyao-agents 的数据目录（如 logs）同样放行
        assert!(check_write_scope(Path::new("/tmp/home/logs/a.log"), None, Some(home)).is_none());
        // fuyao_home 根下的文件与 home 本身视作在子树内
        assert!(check_write_scope(Path::new("/tmp/home/fuyao.toml"), None, Some(home)).is_none());
        assert!(check_write_scope(Path::new("/tmp/home"), None, Some(home)).is_none());
        // 子树外拒绝
        assert!(
            check_write_scope(Path::new("/tmp/home2/fuyao-agents/a.md"), None, Some(home))
                .is_some()
        );
        assert!(check_write_scope(Path::new("/tmp/elsewhere/a.txt"), None, Some(home)).is_some());
    }

    /// 前缀边界：`E:\ws2` 不能误命中 `E:\ws` 前缀（组件级比较自带分隔符边界）
    #[test]
    fn write_scope_prefix_boundary() {
        let ws = Path::new("E:\\ws");
        assert!(check_write_scope(Path::new("E:\\ws\\a.txt"), Some(ws), None).is_none());
        assert!(check_write_scope(Path::new("E:\\ws2\\a.txt"), Some(ws), None).is_some());
    }

    /// 大小写不敏感：盘符与目录名大小写不同仍命中（Windows 路径语义）
    #[test]
    fn write_scope_case_insensitive() {
        let ws = Path::new("E:\\WS");
        assert!(check_write_scope(Path::new("e:\\ws\\a.txt"), Some(ws), None).is_none());
    }

    /// workspace 为 None 时不做工作目录判定，仅剩 fuyao_home 子树通道
    #[test]
    fn write_scope_without_workspace_uses_home_channel() {
        let home = Path::new("/tmp/home");
        assert!(
            check_write_scope(Path::new("/tmp/home/fuyao-agents/a.md"), None, Some(home)).is_none()
        );
        assert!(check_write_scope(Path::new("/tmp/elsewhere/a.txt"), None, Some(home)).is_some());
    }

    // ── check_write_scope_ctx ───────────────────────────────────

    /// 默认 ctx（无 agent_paths）：workspace 回退进程当前目录，cwd 下路径放行
    #[test]
    fn write_scope_ctx_falls_back_to_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let inside = cwd.join("fuyao_scope_inside_cwd.txt");
        assert!(check_write_scope_ctx(&ToolCallContext::default(), &inside).is_none());
        // cwd 之外且不被任何清单覆盖的路径拒绝
        let outside = std::env::temp_dir().join("fuyao_scope_outside_cwd.txt");
        assert!(check_write_scope_ctx(&ToolCallContext::default(), &outside).is_some());
    }

    /// 注入 agent_paths：按注入的 workspace 与 fuyao_home 判定，不受进程全局状态影响
    #[test]
    fn write_scope_ctx_uses_injected_agent_paths() {
        let temp = std::env::temp_dir().join("fuyao_test_scope_ctx");
        std::fs::create_dir_all(&temp).unwrap();
        let ctx = ToolCallContext {
            agent_paths: Some(fuyao_api::AgentPaths {
                workspace: Some(temp.join("ws")),
                fuyao_home: temp.join("home"),
                ..fuyao_api::AgentPaths::default()
            }),
            ..ToolCallContext::default()
        };
        // workspace 子树内放行
        assert!(
            check_write_scope_ctx(&ctx, &temp.join("ws").join("src").join("main.rs")).is_none()
        );
        // fuyao_home 子树整体放行：fuyao-agents 数据与根下文件均在内
        assert!(
            check_write_scope_ctx(
                &ctx,
                &temp
                    .join("home")
                    .join("fuyao-agents")
                    .join("a")
                    .join("data.db")
            )
            .is_none()
        );
        assert!(check_write_scope_ctx(&ctx, &temp.join("home").join("fuyao.toml")).is_none());
        assert!(
            check_write_scope_ctx(&ctx, &temp.join("home").join("logs").join("a.log")).is_none()
        );
        // 两清单之外拒绝：同盘前缀不误命中
        assert!(check_write_scope_ctx(&ctx, &temp.join("ws2").join("x.txt")).is_some());
        assert!(check_write_scope_ctx(&ctx, &temp.join("home2").join("y.txt")).is_some());

        std::fs::remove_dir_all(&temp).ok();
    }
}
