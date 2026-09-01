//! 文件快照引擎：影子 git 仓全工作区采集与恢复
//!
//! 影子仓是独立于用户 `.git` 的快照仓：`--git-dir` 落快照目录、`--work-tree` 指向
//! 用户工作区，只做 plumbing 用法（`add -A` / `write-tree` / `diff-tree` /
//! `ls-tree` / `checkout <tree> -- <files>`）——无 commit、无分支、不写用户 `.git`
//! 的任何东西。
//!
//! 公开面：
//! - [`FileSnapshot::new`]：构造探测——git 在 PATH 且影子仓初始化成功 → 可用态；
//!   失败 → 禁用态（WARN、绝不 panic），后续 [`FileSnapshot::track`] 静默跳过
//! - [`FileSnapshot::track`]：`add -A` → `write-tree` 得基线树 → `diff-tree` 对比
//!   上一棵树得变更文件集（首拍无上一树时变更集为空）
//! - [`FileSnapshot::plan_restore`]：只读分类查询——基线树 × 触碰集 →
//!   「将恢复 / 将删除」两组清单；[`FileSnapshot::restore`] 内部复用同一分类逻辑，
//!   预览与执行口径一致
//! - [`FileSnapshot::restore`]：把工作区恢复到基线树现场——存在者 checkout 回基线
//!   内容、不存在者（快照后新建）删除；幂等可重试，越界路径拒绝
//! - [`FileSnapshot::gc`]：`git gc --prune=7.days` 回收超期松散对象（7 天 TTL）
//!
//! 采集语义：未跟踪文件入册；`.gitignore` 规则生效；未跟踪且超过大小上限的文件
//! 写入影子仓 `info/exclude` 排除。同一实例的 track / restore / gc 经 tokio 互斥锁串行。

mod git;

use git::{
    GitInvoker, exclude_pattern, parse_diff_tree_z, parse_ls_files_z, parse_ls_tree_z,
    probe_shadow_repo,
};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

/// 未跟踪文件入快照的默认大小上限（MB）
pub const DEFAULT_MAX_UNTRACKED_MB: u64 = 2;

/// 单批 checkout 的文件数上限（防止命令行长度溢出）
const RESTORE_BATCH_FILES: usize = 100;

/// 松散对象保留期：超期未被快照引用的对象由 [`FileSnapshot::gc`] 回收
const GC_PRUNE_EXPIRY: &str = "7.days";

/// 单次快照采集结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackOutcome {
    /// 基线树 hash（write-tree 结果，内容寻址）
    pub tree_hash: String,
    /// 与上一棵快照树的变更文件集（工作区相对路径、`/` 分隔）；首拍为空集
    pub files: Vec<String>,
}

/// 恢复计划：基线树对触碰集的只读分类结果
///
/// [`FileSnapshot::plan_restore`] 与 [`FileSnapshot::restore`] 共用同一分类，
/// 预览与执行的口径一致。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RestorePlan {
    /// 存在于基线树、将恢复为基线内容的文件（工作区相对路径，已排序去重）
    pub to_restore: Vec<String>,
    /// 不存在于基线树、将被删除的文件（快照后新建），已排序去重
    pub to_delete: Vec<String>,
}

/// 恢复执行结果
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RestoreOutcome {
    /// 已恢复为基线内容的文件清单
    pub restored: Vec<String>,
    /// 已删除的文件清单（基线树中不存在的快照后新建文件）
    pub deleted: Vec<String>,
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
    /// 删除目标越界：canonicalize 后不在工作区内，拒绝执行
    Escape { path: String },
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
            SnapshotError::Escape { path } => {
                write!(f, "删除目标越界（{path}）：不在工作区内，已拒绝删除")
            }
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
    /// 同一影子仓的 track / restore / gc 串行锁（git index 为单文件，并发写会争用 index.lock）
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

    /// 直接构造禁用态（配置总开关关闭用）
    ///
    /// `[snapshot] enabled = false` 时装配层用本构造器跳过探测，全程等同
    /// 「快照不可用」降级：track 静默跳过、回退降级为仅消息回退、gc 跳过成功。
    pub fn disabled() -> FileSnapshot {
        FileSnapshot { repo: None }
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

    /// 只读分类查询：基线树 × 触碰集 → 「将恢复 / 将删除」两组清单
    ///
    /// - 触碰集中存在于基线树的文件 → 将恢复为基线内容
    /// - 触碰集中不存在于基线树的文件 → 将被删除（快照后新建）
    ///
    /// 禁用态返回 `Ok(None)`；触碰集为空时返回空计划（不触发 git 调用）。
    /// [`FileSnapshot::restore`] 内部复用同一分类逻辑，预览与执行口径一致。
    pub async fn plan_restore(
        &self,
        baseline_tree: &str,
        touched: &[String],
    ) -> Result<Option<RestorePlan>, SnapshotError> {
        let Some(repo) = self.repo.as_ref() else {
            return Ok(None);
        };
        if touched.is_empty() {
            return Ok(Some(RestorePlan::default()));
        }
        let plan = repo.classify_touched(baseline_tree, touched).await?;
        tracing::debug!(
            baseline_tree = %baseline_tree,
            to_restore = plan.to_restore.len(),
            to_delete = plan.to_delete.len(),
            "恢复计划分类完成"
        );
        Ok(Some(plan))
    }

    /// 把工作区恢复到基线树现场
    ///
    /// - 存在于基线树的触碰文件 checkout 回基线内容（含重建被删文件与其父目录），
    ///   单批 ≤ [`RESTORE_BATCH_FILES`] 个文件分批执行
    /// - 不存在于基线树的触碰文件（快照后新建）删除：删除前 canonicalize 并校验
    ///   位于工作区内，越界路径报 [`SnapshotError::Escape`] 整体拒绝
    /// - 幂等：对同一基线树重复 restore 是 no-op、返回清单与首次一致，中途失败后
    ///   可安全重试
    /// - 禁用态返回 `Ok(None)`；触碰集为空时返回空结果
    pub async fn restore(
        &self,
        baseline_tree: &str,
        touched: &[String],
    ) -> Result<Option<RestoreOutcome>, SnapshotError> {
        let Some(repo) = self.repo.as_ref() else {
            return Ok(None);
        };
        if touched.is_empty() {
            return Ok(Some(RestoreOutcome::default()));
        }
        let start = Instant::now();
        // 串行段覆盖分类 → checkout → 删除整个恢复流程
        let _guard = repo.lock.lock().await;

        let plan = repo.classify_touched(baseline_tree, touched).await?;
        repo.checkout_baseline(baseline_tree, &plan.to_restore)
            .await?;
        repo.delete_outside_baseline(&plan.to_delete).await?;

        tracing::info!(
            elapsed_ms = start.elapsed().as_millis() as u64,
            restored = plan.to_restore.len(),
            deleted = plan.to_delete.len(),
            "文件恢复完成"
        );
        Ok(Some(RestoreOutcome {
            restored: plan.to_restore,
            deleted: plan.to_delete,
        }))
    }

    /// 回收影子仓超期松散对象
    ///
    /// `git gc --prune=<GC_PRUNE_EXPIRY>`：超过 7 天未被快照引用的松散对象被清除，
    /// 磁盘占用有界；超期回退点在 gc 后不可再恢复（上层错误信息需明示该事实）。
    /// 禁用态为跳过成功。
    pub async fn gc(&self) -> Result<(), SnapshotError> {
        let Some(repo) = self.repo.as_ref() else {
            return Ok(());
        };
        let start = Instant::now();
        // 与 track / restore 同锁串行，避免 gc 移动对象文件与采集/恢复竞态
        let _guard = repo.lock.lock().await;
        let prune = format!("--prune={GC_PRUNE_EXPIRY}");
        repo.git.run(&["gc", &prune]).await?;
        tracing::info!(
            elapsed_ms = start.elapsed().as_millis() as u64,
            "影子仓 gc 完成"
        );
        Ok(())
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

    /// 把触碰集按「基线树内 / 基线树外」分类为恢复计划
    ///
    /// 基线树内 → 将恢复为基线内容；基线树外 → 将删除（快照后新建）。
    /// 触碰集先去重排序，保证清单顺序确定。
    async fn classify_touched(
        &self,
        baseline_tree: &str,
        touched: &[String],
    ) -> Result<RestorePlan, SnapshotError> {
        const LS_TREE: &str = "git ls-tree -r --name-only -z";
        let raw = self
            .git
            .run(&["ls-tree", "-r", "--name-only", "-z", baseline_tree])
            .await?;
        let in_tree: HashSet<String> = parse_ls_tree_z(&raw, LS_TREE)?.into_iter().collect();

        let mut unique = touched.to_vec();
        unique.sort();
        unique.dedup();
        let mut plan = RestorePlan::default();
        for path in unique {
            if in_tree.contains(&path) {
                plan.to_restore.push(path);
            } else {
                plan.to_delete.push(path);
            }
        }
        Ok(plan)
    }

    /// 按基线树恢复文件内容：`checkout <tree> -- <files>`，单批文件数不超过上限
    ///
    /// checkout 同时写工作区与影子仓 index；影子仓 index 不承载语义，后续 track
    /// 的 `add -A` 会整体重建。空清单不触发任何 git 调用。
    async fn checkout_baseline(&self, tree: &str, files: &[String]) -> Result<(), SnapshotError> {
        for batch in files.chunks(RESTORE_BATCH_FILES) {
            let mut args: Vec<&str> = Vec::with_capacity(3 + batch.len());
            args.extend_from_slice(&["checkout", tree, "--"]);
            args.extend(batch.iter().map(String::as_str));
            self.git.run(&args).await?;
        }
        Ok(())
    }

    /// 删除基线树外的触碰文件（快照后新建）
    ///
    /// 删除前 canonicalize 并校验位于工作区内：越界路径（含解析到工作区外的符号
    /// 链接）报 [`SnapshotError::Escape`] 拒绝执行；文件已不存在则跳过（幂等）。
    async fn delete_outside_baseline(&self, files: &[String]) -> Result<(), SnapshotError> {
        if files.is_empty() {
            return Ok(());
        }
        let work_root = std::fs::canonicalize(&self.work_tree).map_err(|e| SnapshotError::Io {
            path: self.work_tree.display().to_string(),
            cause: e.to_string(),
        })?;
        for rel in files {
            let target = self.work_tree.join(rel);
            // 已不存在的目标无需删除：重复恢复时自然跳过
            if !target.exists() {
                continue;
            }
            let canonical = std::fs::canonicalize(&target).map_err(|e| SnapshotError::Io {
                path: target.display().to_string(),
                cause: e.to_string(),
            })?;
            if !canonical.starts_with(&work_root) {
                return Err(SnapshotError::Escape { path: rel.clone() });
            }
            std::fs::remove_file(&target).map_err(|e| SnapshotError::Io {
                path: target.display().to_string(),
                cause: e.to_string(),
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{FileTime, set_file_mtime};
    use std::fs;
    use std::time::{Duration, SystemTime};
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

    /// 直接构造的禁用态与探测失败降级同形：全程静默跳过，不创建任何目录
    #[tokio::test]
    async fn disabled_constructor_skips_everything() {
        let snap = FileSnapshot::disabled();
        assert!(!snap.is_enabled());
        assert!(snap.track(None).await.unwrap().is_none());
        assert!(snap.gc().await.is_ok());
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

    /// 测试辅助：读取工作区内某文件的内容
    fn read_ws(ws: &Path, rel: &str) -> String {
        fs::read_to_string(ws.join(rel)).unwrap()
    }

    /// 测试辅助：对影子仓直接执行 git 查询命令，断言成功并返回 stdout（去空白）
    fn git_repo_query(git_dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg(format!("--git-dir={}", git_dir.display()))
            .args(args)
            .stderr(std::process::Stdio::null())
            .output()
            .expect("影子仓 git 查询应可执行");
        assert!(out.status.success(), "影子仓查询应成功: {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// 测试辅助：松散对象文件路径（objects/前两位/后三十八位）
    fn object_path(git_dir: &Path, hash: &str) -> PathBuf {
        assert_eq!(hash.len(), 40, "SHA-1 对象 hash 应为 40 字符");
        git_dir.join("objects").join(&hash[..2]).join(&hash[2..])
    }

    /// 测试辅助：伪造松散对象文件的 mtime（对象文件只读，先解除只读再改时间戳）
    #[allow(clippy::permissions_set_readonly_false)] // 只读属性是改时间戳的前置障碍，必须先解除
    fn age_object_file(path: &Path, when: SystemTime) {
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_readonly(false);
        fs::set_permissions(path, perms).unwrap();
        set_file_mtime(path, FileTime::from_system_time(when)).unwrap();
    }

    /// 恢复终态：改 / 增 / 删三类改动全部退回基线现场，触碰集外文件原封不动
    #[tokio::test]
    async fn restore_reverts_modify_add_delete() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        fs::write(ws.path().join("modify.txt"), "v1").unwrap();
        fs::write(ws.path().join("delete.txt"), "v1").unwrap();
        fs::write(ws.path().join("untouched.txt"), "稳定内容").unwrap();
        fs::create_dir_all(ws.path().join("子 目录")).unwrap();
        fs::write(ws.path().join("子 目录").join("deep file.txt"), "v1").unwrap();
        let snap =
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        let first = snap.track(None).await.unwrap().unwrap();

        // 快照后三类改动：修改、新建（含中文与空格路径）、删除（含整目录移除）
        fs::write(ws.path().join("modify.txt"), "v2 改动").unwrap();
        fs::remove_file(ws.path().join("delete.txt")).unwrap();
        fs::remove_dir_all(ws.path().join("子 目录")).unwrap();
        fs::write(ws.path().join("new.txt"), "新建内容").unwrap();
        fs::create_dir_all(ws.path().join("新 目录")).unwrap();
        fs::write(ws.path().join("新 目录").join("新建 文件.txt"), "新建").unwrap();
        let second = snap.track(Some(&first.tree_hash)).await.unwrap().unwrap();

        let outcome = snap
            .restore(&first.tree_hash, &second.files)
            .await
            .unwrap()
            .expect("可用态恢复应有结果");
        assert_eq!(
            outcome.restored,
            vec![
                "delete.txt".to_string(),
                "modify.txt".to_string(),
                "子 目录/deep file.txt".to_string(),
            ]
        );
        assert_eq!(
            outcome.deleted,
            vec!["new.txt".to_string(), "新 目录/新建 文件.txt".to_string(),]
        );

        // 工作区终态：恢复内容、重建被删文件与父目录、清掉新建文件、触碰集外不动
        assert_eq!(read_ws(ws.path(), "modify.txt"), "v1");
        assert_eq!(read_ws(ws.path(), "delete.txt"), "v1");
        assert_eq!(read_ws(ws.path(), "子 目录/deep file.txt"), "v1");
        assert_eq!(read_ws(ws.path(), "untouched.txt"), "稳定内容");
        assert!(!ws.path().join("new.txt").exists());
        assert!(!ws.path().join("新 目录").join("新建 文件.txt").exists());

        // 终态再拍一棵树应与基线树完全一致（内容级等价断言）
        let after = snap.track(None).await.unwrap().unwrap();
        assert_eq!(after.tree_hash, first.tree_hash);
    }

    /// 幂等：对同一基线树重复 restore 是 no-op，清单与首次一致，终态恒定
    #[tokio::test]
    async fn restore_is_idempotent() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        fs::write(ws.path().join("a.txt"), "v1").unwrap();
        let snap =
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        let first = snap.track(None).await.unwrap().unwrap();

        fs::write(ws.path().join("a.txt"), "v2").unwrap();
        fs::write(ws.path().join("new.txt"), "新").unwrap();
        let touched = vec!["a.txt".to_string(), "new.txt".to_string()];

        let first_outcome = snap
            .restore(&first.tree_hash, &touched)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_outcome.restored, vec!["a.txt".to_string()]);
        assert_eq!(first_outcome.deleted, vec!["new.txt".to_string()]);

        // 人工再次改动触碰集内文件：重复 restore 仍回到同一基线现场
        fs::write(ws.path().join("a.txt"), "v3 人工修改").unwrap();
        let second_outcome = snap
            .restore(&first.tree_hash, &touched)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second_outcome, first_outcome);
        assert_eq!(read_ws(ws.path(), "a.txt"), "v1");
        assert!(!ws.path().join("new.txt").exists());
    }

    /// 只读分类：plan_restore 只产出清单不动工作区，与 restore 执行清单口径一致；
    /// 空触碰集直接返回空结论且不触发恢复
    #[tokio::test]
    async fn plan_restore_is_readonly_and_matches_execution() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        fs::write(ws.path().join("a.txt"), "v1").unwrap();
        let snap =
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        let first = snap.track(None).await.unwrap().unwrap();

        fs::write(ws.path().join("a.txt"), "v2").unwrap();
        fs::write(ws.path().join("n.txt"), "新").unwrap();
        let touched = vec!["a.txt".to_string(), "n.txt".to_string()];

        let plan = snap
            .plan_restore(&first.tree_hash, &touched)
            .await
            .unwrap()
            .expect("可用态分类应有结果");
        assert_eq!(plan.to_restore, vec!["a.txt".to_string()]);
        assert_eq!(plan.to_delete, vec!["n.txt".to_string()]);
        // 只读语义：工作区保持改动后的现场
        assert_eq!(read_ws(ws.path(), "a.txt"), "v2");
        assert!(ws.path().join("n.txt").exists());

        // 空触碰集：空计划、空恢复，且不触碰工作区
        let empty_plan = snap.plan_restore(&first.tree_hash, &[]).await.unwrap();
        assert_eq!(empty_plan, Some(RestorePlan::default()));
        let empty_outcome = snap.restore(&first.tree_hash, &[]).await.unwrap();
        assert_eq!(empty_outcome, Some(RestoreOutcome::default()));
        assert_eq!(read_ws(ws.path(), "a.txt"), "v2");
        assert!(ws.path().join("n.txt").exists());

        // 执行清单与预览清单一致
        let outcome = snap.restore(&first.tree_hash, &touched).await.unwrap();
        assert_eq!(
            outcome,
            Some(RestoreOutcome {
                restored: plan.to_restore,
                deleted: plan.to_delete,
            })
        );
    }

    /// 越界护栏：删除目标解析到工作区外时整体拒绝，外部文件原封保留
    #[tokio::test]
    async fn restore_rejects_path_outside_worktree() {
        let root = tempdir().unwrap();
        let ws = root.path().join("工作 区");
        fs::create_dir_all(&ws).unwrap();
        let sh = tempdir().unwrap();
        fs::write(ws.join("base.txt"), "基线").unwrap();
        let snap =
            snapshot_with_program("git", ws.as_path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        let first = snap.track(None).await.unwrap().unwrap();

        let outside = root.path().join("外部 文件.txt");
        fs::write(&outside, "工作区外的文件").unwrap();
        let touched = vec!["../外部 文件.txt".to_string()];

        let err = snap.restore(&first.tree_hash, &touched).await.unwrap_err();
        assert!(
            matches!(err, SnapshotError::Escape { .. }),
            "越界删除应被拒绝，实际: {err}"
        );
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "工作区外的文件",
            "被拒绝的外部文件不应被动过"
        );
    }

    /// 分批恢复：超过单批上限（100）的文件跨批全部恢复，终态与基线一致
    #[tokio::test]
    async fn restore_recreates_batch_of_150_files() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        let names: Vec<String> = (0..150).map(|i| format!("f{i:03}.txt")).collect();
        for name in &names {
            fs::write(ws.path().join(name), "v1").unwrap();
        }
        let snap =
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        let first = snap.track(None).await.unwrap().unwrap();

        for name in &names {
            fs::remove_file(ws.path().join(name)).unwrap();
        }
        let outcome = snap
            .restore(&first.tree_hash, &names)
            .await
            .unwrap()
            .expect("分批恢复应有结果");
        assert_eq!(outcome.restored.len(), 150);
        for name in &names {
            assert_eq!(
                read_ws(ws.path(), name),
                "v1",
                "文件 {name} 应恢复为基线内容"
            );
        }

        let after = snap.track(None).await.unwrap().unwrap();
        assert_eq!(after.tree_hash, first.tree_hash);
    }

    /// gc 生命周期：超 7 天的松散对象被回收、对应基线树不再可恢复；新鲜对象保留可恢复
    #[tokio::test]
    async fn gc_prunes_expired_objects_and_keeps_fresh() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        fs::write(ws.path().join("a.txt"), "v1").unwrap();
        let snap =
            snapshot_with_program("git", ws.path(), sh.path(), DEFAULT_MAX_UNTRACKED_MB).await;
        let first = snap.track(None).await.unwrap().unwrap();

        fs::write(ws.path().join("a.txt"), "v2").unwrap();
        let second = snap.track(Some(&first.tree_hash)).await.unwrap().unwrap();

        // gc 前两棵基线树都可恢复
        snap.restore(&first.tree_hash, &second.files)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read_ws(ws.path(), "a.txt"), "v1");
        snap.restore(&second.tree_hash, &second.files)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read_ws(ws.path(), "a.txt"), "v2");

        // 把首棵树与其 v1 blob 的 mtime 伪造到 8 天前（超出 7 天保留期）
        let v1_blob = git_repo_query(
            sh.path(),
            &["rev-parse", &format!("{}:a.txt", first.tree_hash)],
        );
        let expired = SystemTime::now() - Duration::from_secs(8 * 24 * 3600);
        age_object_file(&object_path(sh.path(), &first.tree_hash), expired);
        age_object_file(&object_path(sh.path(), &v1_blob), expired);

        snap.gc().await.unwrap();

        // 超期基线树已回收：恢复报错（对象不复存在）
        let expired_restore = snap.restore(&first.tree_hash, &second.files).await;
        assert!(expired_restore.is_err(), "超期基线树不应再可恢复");

        // 新鲜基线树不受影响：照常恢复到 v2
        snap.restore(&second.tree_hash, &second.files)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read_ws(ws.path(), "a.txt"), "v2");
    }

    /// 禁用态：分类、恢复返回 Ok(None)，gc 跳过成功，全程不 panic
    #[tokio::test]
    async fn disabled_snapshot_restore_plan_and_gc_are_noops() {
        let ws = tempdir().unwrap();
        let sh = tempdir().unwrap();
        let snap = snapshot_with_program(
            "fuyao-no-such-git-xyz",
            ws.path(),
            sh.path(),
            DEFAULT_MAX_UNTRACKED_MB,
        )
        .await;
        let touched = vec!["a.txt".to_string()];
        assert_eq!(
            snap.plan_restore("0000000000000000000000000000000000000000", &touched)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            snap.restore("0000000000000000000000000000000000000000", &touched)
                .await
                .unwrap(),
            None
        );
        snap.gc().await.unwrap();
    }
}
