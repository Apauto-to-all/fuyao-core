//! git 子进程封装
//!
//! git CLI 的异步调用（固定携带 `--git-dir` + `--work-tree` 与字节精确的换行配置）、
//! 构造探测（git 可用性 + 影子仓初始化）、NUL 分隔输出的解析、exclude 模式转义。

use crate::SnapshotError;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Windows 子进程不分配控制台窗口，GUI 宿主派生时不闪现终端窗口
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// git 调用器：绑定一个影子仓（git-dir）与一个用户工作区（work-tree）
///
/// 每次调用都显式携带 `--git-dir` 与 `--work-tree`、以工作区为子进程工作目录，
/// 并以 `core.autocrlf=false` 保证对象内容按原始字节存储。
#[derive(Debug)]
pub(crate) struct GitInvoker {
    /// git 可执行程序名（从 PATH 解析）
    program: String,
    /// 影子仓目录（git-dir）
    git_dir: PathBuf,
    /// 用户工作区目录（work-tree）
    work_tree: PathBuf,
}

impl GitInvoker {
    /// 构造调用器
    pub(crate) fn new(program: &str, git_dir: &Path, work_tree: &Path) -> Self {
        Self {
            program: program.to_string(),
            git_dir: git_dir.to_path_buf(),
            work_tree: work_tree.to_path_buf(),
        }
    }

    /// 执行一条 git 子命令，成功返回 stdout 原始字节
    ///
    /// stdin 关闭、stdout/stderr 接管；退出码非零折成 [`SnapshotError::Git`]，
    /// stderr 一并带回供上层诊断。
    pub(crate) async fn run(&self, args: &[&str]) -> Result<Vec<u8>, SnapshotError> {
        let mut cmd = tokio::process::Command::new(&self.program);
        // 全局选项：换行配置在前，随后锁定影子仓与工作区
        cmd.arg("-c")
            .arg("core.autocrlf=false")
            .arg(format!("--git-dir={}", self.git_dir.display()))
            .arg(format!("--work-tree={}", self.work_tree.display()))
            // 以工作区为工作目录，保证无 pathspec 的命令覆盖整个工作区
            .current_dir(&self.work_tree)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        // output() 内部并发收拢 stdout 与 stderr，不会因管道缓冲阻塞子进程
        let output = cmd.output().await.map_err(|e| SnapshotError::Spawn {
            program: self.program.clone(),
            cause: e.to_string(),
        })?;

        if !output.status.success() {
            return Err(SnapshotError::Git {
                command: self.describe(args),
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(output.stdout)
    }

    /// 生成用于错误信息的命令描述（如 `git add -A`）
    fn describe(&self, args: &[&str]) -> String {
        format!("git {}", args.join(" "))
    }
}

/// 构造探测：验证 git 可用并初始化影子仓
///
/// ① `git --version` 确认 git 在 PATH 且可执行；
/// ② `git --git-dir <snapshot_root> init` 建影子仓（已存在时幂等补齐目录结构）。
/// 任何一步失败返回人类可读原因，供禁用态 WARN 日志使用。
pub(crate) async fn probe_shadow_repo(program: &str, snapshot_root: &Path) -> Result<(), String> {
    let version = tokio::process::Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("git 不在 PATH 或不可执行（{program}）: {e}"))?;
    if !version.status.success() {
        return Err(format!(
            "git --version 退出码 {}: {}",
            version.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&version.stderr).trim()
        ));
    }

    let init = tokio::process::Command::new(program)
        .arg(format!("--git-dir={}", snapshot_root.display()))
        .arg("init")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("影子仓初始化进程启动失败: {e}"))?;
    if !init.status.success() {
        return Err(format!(
            "影子仓初始化失败（{}）: {}",
            snapshot_root.display(),
            String::from_utf8_lossy(&init.stderr).trim()
        ));
    }
    Ok(())
}

/// 解析 `ls-files --others --exclude-standard -z` 输出为未跟踪路径列表
///
/// 输出为 NUL 分隔的原始字节路径（`-z` 关闭引号转义，非 ASCII 原样输出）。
pub(crate) fn parse_ls_files_z(raw: &[u8], command: &str) -> Result<Vec<String>, SnapshotError> {
    decode_nul_paths(raw, command)
}

/// 解析 `ls-tree -r --name-only -z <tree>` 输出为树内文件路径列表
///
/// `--name-only` 让每条记录只输出路径本身，`-z` 以 NUL 分隔且关闭引号转义。
pub(crate) fn parse_ls_tree_z(raw: &[u8], command: &str) -> Result<Vec<String>, SnapshotError> {
    decode_nul_paths(raw, command)
}

/// 解析 `diff-tree -r -z <old> <new>` 输出为变更文件集
///
/// 每条记录形如 `:old_mode new_mode old_sha new_sha 状态` + NUL + 路径 + NUL，
/// 路径为工作区相对路径；取记录中的路径段，忽略其余元数据。
pub(crate) fn parse_diff_tree_z(raw: &[u8], command: &str) -> Result<Vec<String>, SnapshotError> {
    let mut files = Vec::new();
    let mut chunks = raw.split(|&b| b == b'\0');
    while let Some(meta) = chunks.next() {
        // 空段只出现在输出尾部
        if meta.is_empty() {
            continue;
        }
        if !meta.starts_with(b":") {
            return Err(SnapshotError::Parse {
                command: command.to_string(),
                cause: format!("元数据段缺少 ':' 前缀: {}", String::from_utf8_lossy(meta)),
            });
        }
        // 元数据段的下一段必为路径
        let path = chunks.next().ok_or_else(|| SnapshotError::Parse {
            command: command.to_string(),
            cause: "记录缺少路径段".to_string(),
        })?;
        if path.is_empty() {
            return Err(SnapshotError::Parse {
                command: command.to_string(),
                cause: "路径段为空".to_string(),
            });
        }
        files.push(decode_path(path, command)?);
    }
    Ok(files)
}

/// NUL 分隔输出统一解码为路径列表
fn decode_nul_paths(raw: &[u8], command: &str) -> Result<Vec<String>, SnapshotError> {
    raw.split(|&b| b == b'\0')
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| decode_path(chunk, command))
        .collect()
}

/// 单个路径段按 UTF-8 解码
fn decode_path(chunk: &[u8], command: &str) -> Result<String, SnapshotError> {
    String::from_utf8(chunk.to_vec()).map_err(|_| SnapshotError::Parse {
        command: command.to_string(),
        cause: "路径不是合法 UTF-8".to_string(),
    })
}

/// 把工作区相对路径转为影子仓 `info/exclude` 的排除模式
///
/// 以 `/` 锚定工作区根做字面匹配：转义 glob 元字符（`*` `?` `[` `]` `\`），
/// 行尾空格加转义防止被忽略。
pub(crate) fn exclude_pattern(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 2);
    out.push('/');
    for ch in path.chars() {
        if matches!(ch, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    if out.ends_with(' ') {
        out.pop();
        out.push_str("\\ ");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ls-files -z 输出：NUL 分隔、含中文与空格、尾部 NUL
    #[test]
    fn parse_ls_files_z_decodes_nul_separated_paths() {
        let raw = "新 目录/space file.txt\0a.txt\0".as_bytes();
        let paths = parse_ls_files_z(raw, "git ls-files").unwrap();
        assert_eq!(paths, vec!["新 目录/space file.txt", "a.txt"]);
    }

    /// ls-files -z 空输出得到空列表
    #[test]
    fn parse_ls_files_z_empty_output() {
        assert!(parse_ls_files_z(b"", "git ls-files").unwrap().is_empty());
    }

    /// ls-tree -z 输出：纯路径段 NUL 分隔，含中文与空格
    #[test]
    fn parse_ls_tree_z_decodes_name_only_paths() {
        let raw = "a.txt\0子 目录/space file.txt\0".as_bytes();
        let paths = parse_ls_tree_z(raw, "git ls-tree").unwrap();
        assert_eq!(paths, vec!["a.txt", "子 目录/space file.txt"]);
    }

    /// ls-tree -z 空树输出得到空列表
    #[test]
    fn parse_ls_tree_z_empty_output() {
        assert!(parse_ls_tree_z(b"", "git ls-tree").unwrap().is_empty());
    }

    /// diff-tree -z 输出：元数据段与路径段成对出现，只取路径
    #[test]
    fn parse_diff_tree_z_extracts_paths_only() {
        // 路径段含 UTF-8 字节序列「新 目录」与空格
        let raw = b":100644 000000 626799f0f85326a8c1fc522db584e86cdfccd51f 0000000000000000000000000000000000000000 D\0gone.txt\0:000000 100644 0000000000000000000000000000000000000000 587be6b4c3f93f93c489c0111bba5596147a26cb A\0\xe6\x96\xb0 \xe7\x9b\xae\xe5\xbd\x95/space file.txt\0";
        let files = parse_diff_tree_z(raw, "git diff-tree").unwrap();
        assert_eq!(files, ["gone.txt", "新 目录/space file.txt"]);
    }

    /// diff-tree -z 两树相同输出为空
    #[test]
    fn parse_diff_tree_z_empty_output() {
        assert!(parse_diff_tree_z(b"", "git diff-tree").unwrap().is_empty());
    }

    /// diff-tree -z 记录缺少路径段时报解析错误
    #[test]
    fn parse_diff_tree_z_missing_path_segment() {
        let err = parse_diff_tree_z(b":100644 100644 aaaa bbbb M\0", "git diff-tree").unwrap_err();
        assert!(matches!(err, SnapshotError::Parse { .. }));
    }

    /// 路径含非法 UTF-8 字节时报解析错误
    #[test]
    fn parse_ls_files_z_rejects_invalid_utf8() {
        let err = parse_ls_files_z(&[0xff, 0xfe, 0x00], "git ls-files").unwrap_err();
        assert!(matches!(err, SnapshotError::Parse { .. }));
    }

    /// 普通路径：仅锚定前缀，空格保持字面
    #[test]
    fn exclude_pattern_plain_path_anchored() {
        assert_eq!(exclude_pattern("docs/read me.md"), "/docs/read me.md");
        assert_eq!(exclude_pattern("中文 文件.txt"), "/中文 文件.txt");
    }

    /// glob 元字符逐个转义，保持字面匹配
    #[test]
    fn exclude_pattern_escapes_glob_metacharacters() {
        assert_eq!(exclude_pattern("a*b?[c].txt"), "/a\\*b\\?\\[c\\].txt");
        assert_eq!(exclude_pattern("反斜杠\\名字"), "/反斜杠\\\\名字");
    }

    /// 行尾空格转义，避免被当作可裁剪的空白
    #[test]
    fn exclude_pattern_escapes_trailing_space() {
        assert_eq!(exclude_pattern("tail .txt "), "/tail .txt\\ ");
    }
}
