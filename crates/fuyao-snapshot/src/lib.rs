//! 文件快照引擎：影子 git 仓全工作区采集
//!
//! 影子仓是独立于用户 `.git` 的快照仓：`--git-dir` 落快照目录、`--work-tree` 指向
//! 用户工作区，只做 plumbing 用法（`add -A` / `write-tree` / `diff-tree`）——无
//! commit、无分支、不写用户 `.git` 的任何东西。
//!
//! 公开面：
//! - [`FileSnapshot::new`]：构造探测——git 在 PATH 且影子仓初始化成功 → 可用态；
//!   失败 → 禁用态（WARN、绝不 panic），后续 [`FileSnapshot::track`] 静默跳过
//! - [`FileSnapshot::track`]：`add -A` → `write-tree` 得基线树 → `diff-tree` 对比
//!   上一棵树得变更文件集（首拍无上一树时变更集为空）
//!
//! 采集语义：未跟踪文件入册；`.gitignore` 规则生效；未跟踪且超过大小上限的文件
//! 写入影子仓 `info/exclude` 排除。同一实例的 track 经 tokio 互斥锁串行。

mod git;

use git::{GitInvoker, exclude_pattern, parse_diff_tree_z, parse_ls_files_z, probe_shadow_repo};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

/// 未跟踪文件入快照的默认大小上限（MB）
pub const DEFAULT_MAX_UNTRACKED_MB: u64 = 2;

/// 单次快照采集结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackOutcome {
    /// 基线树 hash（write-tree 结果，内容寻址）
    pub tree_hash: String,
    /// 与上一棵快照树的变更文件集（工作区相对路径、`/` 分隔）；首拍为空集
    pub files: Vec<String>,
}

/// 快照操作错误
#[derive(Debug)]
pub enum SnapshotError {
    /// git 子进程启动失败（如 git 从 PATH 消失）
    Spawn { program: String, cause: String },
    /// git 命令退出码非零（stderr 随错误带回）
    Git {
        command: String,
        code: i32,
        stderr: String,
    },
    /// git 输出解析失败（非法 UTF-8 或记录结构不完整）
    Parse { command: String, cause: String },
    /// 影子仓本地文件操作失败
    Io { path: String, cause: String },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Spawn { program, cause } => {
                write!(f, "git 子进程启动失败（{program}）: {cause}")
            }
            SnapshotError::Git {
                command,
                code,
                stderr,
            } => {
                write!(f, "git 命令失败（{command}，退出码 {code}）: {stderr}")
            }
            SnapshotError::Parse { command, cause } => {
                write!(f, "git 输出解析失败（{command}）: {cause}")
            }
            SnapshotError::Io { path, cause } => write!(f, "文件操作失败（{path}）: {cause}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// 文件快照器：一个用户工作区对应一个影子仓
///
/// 共享句柄语义（内部 [`Arc`]），同进程多会话可克隆同一实例共享影子仓；
/// 禁用态（[`FileSnapshot::new`] 探测失败）下所有操作静默跳过。
#[derive(Clone)]
pub struct FileSnapshot {
    /// None 即禁用态；Some 携带影子仓运行态
    repo: Option<Arc<ShadowRepo>>,
}

/// 影子仓运行态：路径、未跟踪大小上限与串行锁
struct ShadowRepo {
    /// git 调用器（绑定 git-dir 与 work-tree）
    git: GitInvoker,
    /// 影子仓目录（git-dir），`info/exclude` 落于此
    git_dir: PathBuf,
    /// 用户工作区目录（work-tree）
    work_tree: PathBuf,
    /// 未跟踪文件入快照的大小上限（字节）
    max_untracked_bytes: u64,
    /// 同一影子仓的 track 串行锁（git index 为单文件，并发写会争用 index.lock）
    lock: Mutex<()>,
}

impl FileSnapshot {
    /// 构造并探测
    ///
    /// git 在 PATH 且影子仓初始化成功 → 可用态；任何失败 → 禁用态（WARN 一条，
    /// 绝不 panic），后续 [`FileSnapshot::track`] 静默跳过。
    /// `max_untracked_mb`：未跟踪文件入快照的大小上限（MB），常用
    /// [`DEFAULT_MAX_UNTRACKED_MB`]。
    pub async fn new(
        work_tree: &Path,
        snapshot_root: &Path,
        max_untracked_mb: u64,
    ) -> FileSnapshot {
        Self::with_git_program("git", work_tree, snapshot_root, max_untracked_mb).await
    }

    /// 以指定 git 程序名构造并探测（程序名可注入，便于用不存在的名字验证降级路径）
    async fn with_git_program(
        program: &str,
        work_tree: &Path,
        snapshot_root: &Path,
        max_untracked_mb: u64,
    ) -> FileSnapshot {
        if let Err(cause) = probe_shadow_repo(program, snapshot_root).await {
            tracing::warn!(
                work_tree = %work_tree.display(),
                snapshot_root = %snapshot_root.display(),
                cause = %cause,
                "文件快照不可用，已进入禁用态"
            );
            return FileSnapshot { repo: None };
        }
        tracing::info!(
            work_tree = %work_tree.display(),
            snapshot_root = %snapshot_root.display(),
            "文件快照影子仓就绪"
        );
        FileSnapshot {
            repo: Some(Arc::new(ShadowRepo {
                git: GitInvoker::new(program, snapshot_root, work_tree),
                git_dir: snapshot_root.to_path_buf(),
                work_tree: work_tree.to_path_buf(),
                max_untracked_bytes: max_untracked_mb * 1024 * 1024,
                lock: Mutex::new(()),
            })),
        }
    }

    /// 是否处于可用态（构造探测通过）
    pub fn is_enabled(&self) -> bool {
        self.repo.is_some()
    }

    /// 对工作区做一次快照
    ///
    /// 流程：排除超限未跟踪文件 → `add -A` → `write-tree` 得基线树 →
    /// `diff-tree` 对比 `prev_tree` 得变更文件集。
    ///
    /// - 禁用态静默跳过，返回 `Ok(None)`
    /// - `prev_tree` 为 `None`（首拍）时变更集为空
    /// - git 报错时返回 [`SnapshotError`]，由上层决定降级策略
    /// - 同一实例的并发 track 经互斥锁串行
    pub async fn track(
        &self,
        prev_tree: Option<&str>,
    ) -> Result<Option<TrackOutcome>, SnapshotError> {
        let Some(repo) = self.repo.as_ref() else {
            return Ok(None);
        };
        let start = Instant::now();
        // 串行段覆盖整个采集流程：exclude 写入 → add → write-tree → diff
        let _guard = repo.lock.lock().await;

        repo.exclude_oversized_untracked().await?;
        repo.stage_all().await?;
        let tree_hash = repo.write_tree().await?;
        let files = match prev_tree {
            Some(prev) => repo.diff_changed_files(prev, &tree_hash).await?,
            None => Vec::new(),
        };

        tracing::info!(
            elapsed_ms = start.elapsed().as_millis() as u64,
            tree_hash = %tree_hash,
            changed = files.len(),
            "快照采集完成"
        );
        Ok(Some(TrackOutcome { tree_hash, files }))
    }
}

impl ShadowRepo {
    /// 把未跟踪且超过大小上限的文件写入影子仓 `info/exclude`
    ///
    /// `.gitignore` 与既有 exclude 条目在 `ls-files --others --exclude-standard`
    /// 阶段天然生效；已写入过的条目不重复追加。
    async fn exclude_oversized_untracked(&self) -> Result<(), SnapshotError> {
        const LS_FILES: &str = "git ls-files --others --exclude-standard -z";
        let raw = self
            .git
            .run(&["ls-files", "--others", "--exclude-standard", "-z"])
            .await?;
        let untracked = parse_ls_files_z(&raw, LS_FILES)?;

        let oversized: Vec<String> = untracked
            .into_iter()
            .filter(|path| {
                let meta = std::fs::metadata(self.work_tree.join(path));
                matches!(&meta, Ok(m) if m.is_file() && m.len() > self.max_untracked_bytes)
            })
            .collect();
        if oversized.is_empty() {
            return Ok(());
        }

        let exclude_path = self.git_dir.join("info").join("exclude");
        let existing = std::fs::read_to_string(&exclude_path).map_err(|e| SnapshotError::Io {
            path: exclude_path.display().to_string(),
            cause: e.to_string(),
        })?;
        let known: HashSet<String> = existing
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
            .map(str::to_string)
            .collect();
        let mut content = existing;
        let mut appended = false;
        for path in oversized {
            let line = exclude_pattern(&path);
            if !known.contains(&line) {
                content.push_str(&line);
                content.push('\n');
                appended = true;
            }
        }
        if appended {
            std::fs::write(&exclude_path, content).map_err(|e| SnapshotError::Io {
                path: exclude_path.display().to_string(),
                cause: e.to_string(),
            })?;
        }
        Ok(())
    }

    /// `add -A`：全工作区入 index（未跟踪文件入册、`.gitignore` 天然生效）
    async fn stage_all(&self) -> Result<(), SnapshotError> {
        self.git.run(&["add", "-A"]).await?;
        Ok(())
    }

    /// `write-tree`：由 index 写出基线树，返回树 hash
    async fn write_tree(&self) -> Result<String, SnapshotError> {
        const WRITE_TREE: &str = "git write-tree";
        let raw = self.git.run(&["write-tree"]).await?;
        String::from_utf8(raw)
            .map_err(|_| SnapshotError::Parse {
                command: WRITE_TREE.to_string(),
                cause: "树 hash 不是合法 UTF-8".to_string(),
            })
            .map(|hash| hash.trim().to_string())
    }

    /// `diff-tree -r`：对比上一棵基线树，返回去重排序后的变更文件集
    async fn diff_changed_files(
        &self,
        prev: &str,
        cur: &str,
    ) -> Result<Vec<String>, SnapshotError> {
        let command = format!("git diff-tree -r -z {prev} {cur}");
        let raw = self.git.run(&["diff-tree", "-r", "-z", prev, cur]).await?;
        let mut files = parse_diff_tree_z(&raw, &command)?;
        files.sort();
        files.dedup();
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// 测试辅助：列出某棵树包含的全部文件路径（独立调用 git 验证树内容）
    fn list_tree_files(git_dir: &Path, tree: &str) -> Vec<String> {
        let out = std::process::Command::new("git")
            .arg(format!("--git-dir={}", git_dir.display()))
            .args(["ls-tree", "-r", "--name-only", "-z", tree])
            .stderr(std::process::Stdio::null())
            .output()
            .expect("git ls-tree 执行失败");
        assert!(out.status.success(), "git ls-tree 应成功");
        out.stdout
            .split(|&b| b == b'\0')
            .filter(|c| !c.is_empty())
            .map(|c| String::from_utf8(c.to_vec()).expect("树路径应为 UTF-8"))
            .collect()
    }

    /// 测试辅助：以指定程序名构造快照器（默认真实 git）
    async fn snapshot_with_program(
        program: &str,
        work_tree: &Path,
        snapshot_root: &Path,
        max_untracked_mb: u64,
    ) -> FileSnapshot {
        FileSnapshot::with_git_program(program, work_tree, snapshot_root, max_untracked_mb).await
    }

    /// git 在 PATH 且影子仓初始化成功 → 可用态，影子仓目录结构就位
    #[tokio::test]
    async fn new_enables_when_git_available() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        let snap =
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        assert!(snap.is_enabled());
        assert!(sh.path().join("objects").is_dir());
        assert!(sh.path().join("info").join("exclude").is_file());
    }

    /// git 不在 PATH → 禁用态（不 panic），track 静默跳过返回 Ok(None)
    #[tokio::test]
    async fn new_disables_when_git_missing() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        let snap = snapshot_with_program(
            "fuyao-no-such-git-xyz",
            ws.path(),
            sh.path(),
            DEFAULT_MAX_UNTRACKED_MB,
        )
        .await;
        assert!(!snap.is_enabled());
        let outcome = snap.track(None).await.unwrap();
        assert!(outcome.is_none());
    }

    /// 影子仓初始化失败（快照根被普通文件占据）→ 禁用态
    #[tokio::test]
    async fn new_disables_when_init_fails() {
        let ws = tempdir().unwrap();
        let occupied = ws.path().join("occupied");
        fs::write(&occupied, "不是目录").unwrap();
        let snap =
            snapshot_with_program("git", ws.path(), &occupied, DEFAULT_MAX_UNTRACKED_MB).await;
        assert!(!snap.is_enabled());
    }

    /// 首拍：返回基线树 hash，变更集为空
    #[tokio::test]
    async fn first_track_returns_baseline_and_empty_changes() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        fs::write(ws.path().join("a.txt"), "内容甲").unwrap();
        let snap =
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;

        let outcome = snap.track(None).await.unwrap().expect("首拍应有结果");
        assert_eq!(outcome.files, Vec::<String>::new());
        assert_eq!(
            outcome.tree_hash.len(),
            40,
            "SHA-1 树 hash 应为 40 个十六进制字符"
        );
        assert!(
            outcome.tree_hash.chars().all(|c| c.is_ascii_hexdigit()),
            "树 hash 应为十六进制"
        );
    }

    /// 增量拍：修改、新建、删除三类变更都进变更集
    #[tokio::test]
    async fn track_detects_modify_add_delete() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        fs::write(ws.path().join("modify.txt"), "v1").unwrap();
        fs::write(ws.path().join("delete.txt"), "v1").unwrap();
        let snap =
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        let first = snap.track(None).await.unwrap().unwrap();

        fs::write(ws.path().join("modify.txt"), "v2 中文内容").unwrap();
        fs::write(ws.path().join("create.txt"), "新文件").unwrap();
        fs::remove_file(ws.path().join("delete.txt")).unwrap();

        let second = snap
            .track(Some(&first.tree_hash))
            .await
            .unwrap()
            .expect("增量拍应有结果");
        assert_eq!(
            second.files,
            vec![
                "create.txt".to_string(),
                "delete.txt".to_string(),
                "modify.txt".to_string()
            ]
        );
        assert_ne!(first.tree_hash, second.tree_hash);
    }

    /// 无变化时对比上一棵树，变更集为空
    #[tokio::test]
    async fn track_without_changes_returns_empty_diff() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        fs::write(ws.path().join("a.txt"), "稳定内容").unwrap();
        let snap =
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        let first = snap.track(None).await.unwrap().unwrap();
        let second = snap.track(Some(&first.tree_hash)).await.unwrap().unwrap();
        assert_eq!(second.tree_hash, first.tree_hash);
        assert!(second.files.is_empty());
    }

    /// 未跟踪文件入册：从未在任何仓登记的文件进入基线树
    #[tokio::test]
    async fn untracked_files_enter_snapshot() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        fs::write(ws.path().join("untracked.txt"), "从未登记").unwrap();
        let snap =
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        let outcome = snap.track(None).await.unwrap().unwrap();
        let tree_files = list_tree_files(sh.path(), &outcome.tree_hash);
        assert!(tree_files.contains(&"untracked.txt".to_string()));
    }

    /// `.gitignore` 规则生效：被忽略的文件不进基线树
    #[tokio::test]
    async fn gitignored_files_excluded() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        fs::write(ws.path().join(".gitignore"), "ignored.log\n").unwrap();
        fs::write(ws.path().join("ignored.log"), "应被忽略").unwrap();
        fs::write(ws.path().join("kept.txt"), "应被保留").unwrap();
        let snap =
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        let outcome = snap.track(None).await.unwrap().unwrap();
        let tree_files = list_tree_files(sh.path(), &outcome.tree_hash);
        assert!(!tree_files.contains(&"ignored.log".to_string()));
        assert!(tree_files.contains(&"kept.txt".to_string()));
    }

    /// 未跟踪且超上限的文件不采集：写入影子仓 info/exclude，后续不再入册
    #[tokio::test]
    async fn oversized_untracked_excluded_via_info_exclude() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        // 上限 0 MB：任何非空未跟踪文件都超限
        fs::write(ws.path().join("big file 大.txt"), "超过 0 字节").unwrap();
        let snap = snapshot_with_program("git", ws.path(), sh.path(), 0).await;

        let first = snap.track(None).await.unwrap().unwrap();
        let tree_files = list_tree_files(sh.path(), &first.tree_hash);
        assert!(!tree_files.contains(&"big file 大.txt".to_string()));

        let exclude = fs::read_to_string(sh.path().join("info").join("exclude")).unwrap();
        assert!(
            exclude.contains("/big file 大.txt"),
            "info/exclude 应含锚定排除条目: {exclude}"
        );

        // 追加内容后再次采集：条目已排除，树不变、变更集为空
        fs::write(ws.path().join("big file 大.txt"), "内容变化仍被排除").unwrap();
        let second = snap.track(Some(&first.tree_hash)).await.unwrap().unwrap();
        assert_eq!(second.tree_hash, first.tree_hash);
        assert!(second.files.is_empty());
    }

    /// 大小上限只约束未跟踪文件：已跟踪文件此后增长不受限，修改仍被采集
    #[tokio::test]
    async fn tracked_file_growth_not_size_limited() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        // 上限 0 MB：空文件（0 字节）不超限，首次采集入册
        let file = ws.path().join("grows.txt");
        fs::write(&file, "").unwrap();
        let snap = snapshot_with_program("git", ws.path(), sh.path(), 0).await;
        let first = snap.track(None).await.unwrap().unwrap();
        assert!(list_tree_files(sh.path(), &first.tree_hash).contains(&"grows.txt".to_string()));

        // 已跟踪文件增长到超限：不进排除，修改仍入变更集
        fs::write(&file, "现在超过 0 字节").unwrap();
        let second = snap.track(Some(&first.tree_hash)).await.unwrap().unwrap();
        assert_eq!(second.files, vec!["grows.txt".to_string()]);
    }

    /// 中文路径与含空格路径（目录名与文件名双覆盖）正确采集与对比
    #[tokio::test]
    async fn chinese_and_space_paths_tracked_exactly() {
        let root = tempdir().unwrap();
        // 工作区目录本身含中文与空格
        let ws = root.path().join("工作 区");
        fs::create_dir_all(&ws).unwrap();
        let sh = tempdir().unwrap();
        fs::create_dir_all(ws.join("源码 目录")).unwrap();
        fs::write(ws.join("源码 目录").join("工具 脚本.py"), "v1").unwrap();
        fs::write(ws.join("read me.md"), "v1").unwrap();

        let snap =
            snapshot_with_program("git", ws.as_path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        assert!(snap.is_enabled());
        let first = snap.track(None).await.unwrap().unwrap();

        fs::write(ws.join("源码 目录").join("工具 脚本.py"), "v2").unwrap();
        fs::write(ws.join("read me.md"), "v2").unwrap();
        let second = snap.track(Some(&first.tree_hash)).await.unwrap().unwrap();
        assert_eq!(
            second.files,
            vec![
                "read me.md".to_string(),
                "源码 目录/工具 脚本.py".to_string(),
            ]
        );
    }

    /// 同一实例并发 track：经互斥锁串行，全部成功且结论一致
    #[tokio::test]
    async fn concurrent_tracks_serialized_and_consistent() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        fs::write(ws.path().join("a.txt"), "并发场景").unwrap();
        let snap = Arc::new(
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await,
        );

        let mut handles = Vec::new();
        for _ in 0..8 {
            let snap = Arc::clone(&snap);
            handles.push(tokio::spawn(async move { snap.track(None).await }));
        }
        let mut hashes = Vec::new();
        for handle in handles {
            let outcome = handle
                .await
                .expect("任务不应 panic")
                .expect("track 不应报错");
            hashes.push(outcome.expect("可用态应有结果").tree_hash);
        }
        // 无 index.lock 争用失败，且工作区未变时各次基线树一致
        hashes.dedup();
        assert_eq!(hashes.len(), 1, "并发串行下基线树应一致");
    }

    /// 影子仓不碰用户仓：无 commit、无分支，用户 `.git` 的 HEAD 与状态原封不动
    #[tokio::test]
    async fn user_git_repository_never_touched() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        // 工作区内建真实用户仓并提交一个文件
        let ws_str = ws.path().to_string_lossy().to_string();
        let run_user = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&ws_str)
                .args(args)
                .output()
                .expect("用户仓 git 应可执行");
            assert!(out.status.success(), "用户仓命令应成功: {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run_user(&["init"]);
        run_user(&["config", "user.email", "t@t"]);
        run_user(&["config", "user.name", "t"]);
        fs::write(ws.path().join("user.txt"), "用户文件").unwrap();
        run_user(&["add", "user.txt"]);
        run_user(&["commit", "-m", "init"]);
        let head_before = run_user(&["rev-parse", "HEAD"]);

        let snap =
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        let outcome = snap.track(None).await.unwrap().unwrap();

        // 用户仓 HEAD 与工作区状态原封不动
        assert_eq!(run_user(&["rev-parse", "HEAD"]), head_before);
        assert_eq!(run_user(&["status", "--porcelain"]), "");

        // 影子仓无 commit、无分支（HEAD 未诞生、无任何引用）
        let rev = std::process::Command::new("git")
            .arg(format!("--git-dir={}", sh.path().display()))
            .args(["rev-parse", "HEAD"])
            .stderr(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(!rev.status.success(), "影子仓不应有 HEAD commit");
        let refs = std::process::Command::new("git")
            .arg(format!("--git-dir={}", sh.path().display()))
            .args(["for-each-ref"])
            .output()
            .unwrap();
        assert!(refs.stdout.is_empty(), "影子仓不应有任何引用");

        // 用户仓内容确实被采集进了影子仓基线树
        let tree_files = list_tree_files(sh.path(), &outcome.tree_hash);
        assert!(tree_files.contains(&"user.txt".to_string()));
        assert!(!tree_files.iter().any(|p| p.starts_with(".git/")));
    }
}
