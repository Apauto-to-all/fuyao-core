//! 会话管理门面：会话检索 / 浏览 / 元数据编辑 / 回退编排的接口
//!
//! 与 [`crate::App`]（运行时交互门面）平级正交：
//! - [`crate::App`] 管对话的进行（create / send / recv）
//! - [`SessionManager`] 管会话的检索 / 浏览 / 手动编辑（列会话 / 查历史 / 改标题）
//!
//! 两者共享同一份 [`fuyao_session::SessionStore`]（由装配层 [`crate::start`] 注入
//! `Arc` 克隆），各取所需：Engine 写（LLM 流程落库、自动标题生成）、SessionManager
//! 读查询 + 补引擎不做的「用户 / 应用手动编辑」写（如手改标题、回退、派生分支）。
//! 通讯方式为直接异步方法调用——纯存储操作、不涉及 LLM、不需要流式产出，
//! 与 `App::create_session` 返 `SessionId` 同属「管理型同步方法」，不走消息总线。
//!
//! # 回退的文件侧联动
//!
//! [`SessionManager::rollback_session`] 是回退的编排入口：消息侧走存储层
//! `rollback_to` 单事务，文件侧经共享的 [`fuyao_snapshot::FileSnapshot`]（与 Engine
//! 内 ReAct 采集共用同一影子仓句柄）先恢复文件后动 DB——顺序承重不可倒置，
//! 保证失败时消息 / 快照行原封、恢复幂等可安全重试。文件侧执行结论经方法
//! 返回值 [`FileRollbackOutcome`] 交付（恢复 / 删除双清单），不产生事件——回退
//! 是请求-响应型同步原语，调用方即结果的唯一消费者。
//! [`SessionManager::preview_rollback`] 提供只读双轴预览，与执行共用同一套目标
//! 校验与影响计算——同一状态下两者结论一致；预览是建议、执行是权威。
//!
//! 两入口均带 `rollback_files` 参数：`true` 文件随消息联动回退（上述编排）；
//! `false` **仅消息模式**——文件侧全部动作按请求跳过（不查快照行、不恢复文件、
//! 不发 WARN——有意选择非降级），直接走 `rollback_to`。
//! 该段快照行仍随消息在同一事务内删除（账已销）：之后再回退更早的回退点，
//! 被保留的文件不会被恢复或删除。
//!
//! # 写能力边界
//!
//! 本门面只暴露**适合外部编辑**的字段。引擎内核的自动写（create / 压缩后重建
//! system_prompt / 标题异步生成 / destroy_session）是其 ReAct 循环与 session task
//! 生命周期的内生产物，不在此暴露——外部插手会破坏一致性。
//!

use std::sync::Arc;

use fuyao_api::Message;
use fuyao_session::SessionStore;
use fuyao_snapshot::FileSnapshot;

/// 会话管理器：持有会话存储与文件快照句柄，对外提供会话检索 / 浏览 / 元数据编辑 /
/// 回退编排接口
///
/// 与 [`App`](crate::App) 平级正交：
/// - [`App`](crate::App) 管「对话的进行」（create / send / recv）
/// - `SessionManager` 管「会话的检索 / 浏览 / 手动编辑」（列会话 / 查历史 / 改标题）
///
/// 与 Engine 共享同一份 `SessionStore` 与同一个 `FileSnapshot`（Arc 级克隆，
/// 零拷贝共享连接池与影子仓）。
///
/// `Clone` 廉价：三个字段全是共享句柄，clone 仅增引用计数、零拷贝。
/// 供消费方（如适配层在锁内 clone 出 owned 句柄以消除借用穿透 await）按需取用。
#[derive(Clone)]
pub struct SessionManager {
    /// 会话存储句柄（与 Engine 共享同一份，Arc 克隆）
    store: Arc<SessionStore>,
    /// 文件快照器（与 Engine 内 ReAct 采集共享同一影子仓句柄）
    snapshot: FileSnapshot,
}

impl SessionManager {
    /// 由装配层（[`crate::start`]）注入共享句柄构造
    ///
    /// - `store` 与 Engine 共享同一份 `SessionStore`——Arc 克隆仅增引用计数，
    ///   两者指向同一个 `SqlitePool` 连接池
    /// - `snapshot` 与 Engine 共享同一影子仓句柄——回退恢复与 ReAct 采集操作同一份
    ///   快照对象库，经句柄内部互斥串行
    pub fn new(store: Arc<SessionStore>, snapshot: FileSnapshot) -> Self {
        Self { store, snapshot }
    }

    // ── 会话查询 ───────────────────────────────────────────────

    /// 列举历史会话（分页，按最近活动时间倒序，只含主会话）
    ///
    /// 返回的会话按 `last_active_at` 倒序——用户刚交互的会话排最前。
    /// 只返回顶层会话（`parent_session_id` 为空）；子会话经
    /// [`list_child_sessions`](Self::list_child_sessions) 按父列举。`total` 与
    /// `items` 同语义（只计主会话）。
    ///
    /// # 参数
    /// - `workspace_filter`：传 `Some(path)` 只列该工作目录的会话；`None` 列全部（含无 workspace 的）
    /// - `limit` / `offset`：分页，单页条数与偏移量
    pub async fn list_sessions(
        &self,
        workspace_filter: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<fuyao_api::SessionPage, fuyao_session::SessionError> {
        // items 与 total 是两条独立查询（list + count），同一 workspace_filter。
        // 极端情况下两次查询之间有新会话插入会致二者差一两条——对「列表分页给 UI 算页数」
        // 是可接受的弱一致（过时的 1-2 条不影响翻页体验）。
        let items = self.store.list_all(workspace_filter, limit, offset).await?;
        let total = self.store.count_with_filter(workspace_filter).await?;
        Ok(fuyao_api::SessionPage {
            items,
            total,
            limit,
            offset,
        })
    }

    /// 列举主会话下的全部子会话（全量、不分页、按创建序）
    ///
    /// 透传 [`SessionStore::list_child_sessions`](fuyao_session::SessionStore::list_child_sessions)：
    /// 返回该父会话派生的全部子会话（子代理 / 后台任务），按 `started_at` 升序（先派生的
    /// 排前面）。全量返回、无分页信封——子会话数由单次任务派生的子代理数决定，天然有限。
    ///
    /// 行的 `parent_session_id` 恒指向 `parent_id`；父不存在或无子会话时返回空列表。
    ///
    /// # 参数
    /// - `parent_id`：父会话（主会话）id
    pub async fn list_child_sessions(
        &self,
        parent_id: &str,
    ) -> Result<Vec<fuyao_api::Session>, fuyao_session::SessionError> {
        self.store.list_child_sessions(parent_id).await
    }

    /// 获取单个会话的最新元数据（纯元数据，不含消息）
    ///
    /// 单行主键直读，返回 DB 当前值——token 累计四项与 `total_cost`（落库 assistant
    /// 消息时由 `insert_message` 事务内原子累加）、`last_active_at`（消息落库时同
    /// 事务刷新）均为 DB 派生字段。落库先于事件发送，消费方在收到事件的时点经此
    /// 拉取即可拿到含该事件效果的最新读数——以 DB 为唯一真相源对齐内存快照，无需
    /// 在应用侧复刻累计规则。
    ///
    /// # 返回
    /// - `Ok(Some(session))`：命中
    /// - `Ok(None)`：session_id 在数据库中不存在
    pub async fn get_session(
        &self,
        session_id: &str,
    ) -> Result<Option<fuyao_api::Session>, fuyao_session::SessionError> {
        self.store.get(session_id).await
    }

    // ── 会话元数据编辑（面向二次开发应用）──────────────────────

    /// 更新会话标题
    ///
    /// 供二次开发应用手动改名（如 TUI 里用户重命名会话、CLI 批量改标题）。与引擎的
    /// 自动标题生成（`react/turn.rs` fire-and-forget spawn）正交：引擎只产出默认标题，
    /// 本方法供应用 / 用户覆盖；两者最终都落同一个局部 UPDATE，最后一个写生效，单字段原子。
    ///
    /// # 标题实体不变量
    ///
    /// 标题合法性是 Session 实体的不变量，入口校验并规范化后落库：
    /// - trim 后非空（纯空白标题拒绝，杜绝空白串原样入库）
    /// - trim 后按字符数计（多字节安全，与自动标题生成同口径）不超过
    ///   `[session.title] max_len`
    ///
    /// 上限读全局配置 [`fuyao_api::get_config`](fuyao_api::get_config) 的
    /// `session.title.max_len`——与自动标题生成的截断上限是同一份配置来源，天然单源；
    /// 错误变体携带 max_len 真值供调用方展示。引擎内部的自动标题（占位 / LLM 生成）
    /// 不经过本方法（直写存储层，自带截断），不受本校验影响。校验先于存储访问：
    /// 非法标题不触碰 DB。
    ///
    /// # 返回
    /// - `Ok(())`：标题已更新（落库的是 trim 后的规范化标题）
    /// - `Err(SessionError::InvalidTitle { max_len })`：标题 trim 后为空或超过 max_len
    /// - `Err(SessionError::NotFound)`：session_id 在数据库中不存在
    pub async fn update_title(
        &self,
        session_id: &str,
        new_title: &str,
    ) -> Result<(), fuyao_session::SessionError> {
        // 实体不变量：规范化（trim）后判空、按字符数判长，上限与自动标题生成同源
        let title = new_title.trim();
        let max_len = fuyao_api::get_config().session.title.max_len;
        if title.is_empty() || title.chars().count() > max_len {
            return Err(fuyao_session::SessionError::InvalidTitle { max_len });
        }
        self.store
            .update_session(session_id, Some(title), None)
            .await
    }

    // ── 会话删除 ───────────────────────────────────────────────

    /// 删除会话（cascade 删该会话及其全部子会话，含各自的消息 + 任务列表）
    ///
    /// 透传 [`SessionStore::delete`](fuyao_session::SessionStore::delete)：单事务内删
    /// todos + messages + sessions，三者要么全删要么全留。
    ///
    /// # 返回
    /// - `Ok(true)`：会话存在，删除成功
    /// - `Ok(false)`：会话不存在（无 session 行被删，但该 id 组内的子会话与残留 todos / messages 仍被清理）
    pub async fn delete_session(
        &self,
        session_id: &str,
    ) -> Result<bool, fuyao_session::SessionError> {
        self.store.delete(session_id).await
    }

    // ── 会话回退（消息 + 文件联动编排）────────────────────────

    /// 回退的目标校验 + 文件侧影响计算（预览与执行共用的内部函数）
    ///
    /// 四步：
    /// 1. 池上只读校验目标（与 `rollback_to` 事务内同一判定函数）——两个入口、
    ///    两种模式的目标约束永远同一套
    /// 2. 仅消息模式（`rollback_files = false`）直接返回 [`FileImpact::Skipped`]
    ///    （不查行、不判可用性——用户有意跳过文件侧，与快照状态无关）
    /// 3. 快照禁用态直接返回 [`FileImpact::Unavailable`]（不查行）
    /// 4. 查 `msg_seq >= target_seq` 的快照行：空返回 [`FileImpact::Empty`]；
    ///    非空取首行 `tree_hash` 为基线树、各行 `files` 并集为触碰集
    async fn resolve_file_impact(
        &self,
        session_id: &str,
        target_seq: i64,
        rollback_files: bool,
    ) -> Result<FileImpact, RollbackError> {
        // 目标校验先行：非法目标在预览与执行两个入口、两种模式下报同一错误
        self.store
            .validate_cut_target(session_id, target_seq)
            .await
            .map_err(RollbackError::Store)?;
        if !rollback_files {
            return Ok(FileImpact::Skipped);
        }
        if !self.snapshot.is_enabled() {
            return Ok(FileImpact::Unavailable);
        }
        let rows = self
            .store
            .list_file_snapshots_from(session_id, target_seq)
            .await
            .map_err(RollbackError::Store)?;
        let Some(first) = rows.first() else {
            return Ok(FileImpact::Empty);
        };
        let baseline_tree = first.tree_hash.clone();
        let mut touched: Vec<String> = rows.iter().flat_map(|r| r.files.iter().cloned()).collect();
        touched.sort();
        touched.dedup();
        Ok(FileImpact::Touch {
            baseline_tree,
            touched,
        })
    }

    /// 把会话回退到目标消息之前（消息侧删行 + 文件侧恢复联动）
    ///
    /// 三步编排，顺序承重不可倒置：
    /// 1. 查行：`msg_seq >= target_seq` 的快照行（经 [`Self::resolve_file_impact`]
    ///    与预览共用校验与影响计算；空触碰集跳文件侧）
    /// 2. 先恢复文件：基线树 = 首行 `tree_hash`、触碰集 = 各行 `files` 并集——
    ///    存在于基线树的 checkout 回基线内容、快照后新建的删除
    /// 3. 后动 DB：`rollback_to` 单事务删消息 + 同谓词删快照行 + 重算元数据
    ///
    /// 顺序语义：先恢复后动 DB——恢复失败则整个回退中止（消息 / 快照行原封），
    /// 恢复幂等（重复 checkout 同一基线树是 no-op），可安全重试；若倒置会出现
    /// 「消息退了文件没退」且不可重试的半截态。
    ///
    /// # 文件侧模式（`rollback_files` 参数）
    ///
    /// - `true`：文件随消息联动回退，即上述三步编排
    /// - `false`：**仅消息模式**——文件侧全部动作按请求跳过（不查快照行、不恢复
    ///   文件、不发 WARN——用户有意选择保留文件现场，
    ///   不是降级），直接走 `rollback_to`
    ///
    /// 仅消息模式的语义代价：该段快照行仍随消息在同一事务内删除（账已销）——
    /// 之后再回退更早的回退点，这些被保留的文件不会被恢复或删除；AI 产出的
    /// 文件改动自本次起脱离回退账本，由用户自行处置。
    ///
    /// 快照不可用（配置关闭 / git 缺失）：联动模式下 WARN 降级为仅消息回退，
    /// 返回值明示「文件未回退」——消息侧照常，调用方据返回值向用户交代文件
    /// 现场未动；仅消息模式下快照状态无关，恒返回
    /// [`FileRollbackOutcome::Skipped`]。
    ///
    /// # 运行态责任边界
    ///
    /// 编排前**不校验该 session 是否有活跃 turn**——若 turn 正在运行，其后续落库
    /// 与回退结果竞争。需要安全回退的调用方应先经运行时门面
    /// [`App::stop_session`](crate::App::stop_session) 屏障停 turn 再回退——
    /// 「先停后滚」的顺序是回退安全性的承重前提，不可倒置。
    ///
    /// # 返回
    /// - [`FileRollbackOutcome::Restored`]：文件侧已联动（清单可能为空——目标后
    ///   无触碰集或触碰文件都已回到基线态）；剩余消息与重算后的会话元数据经
    ///   既有读路径获取（`list_messages` / `get_session`）
    /// - [`FileRollbackOutcome::Skipped`]：仅消息模式，文件侧按请求跳过、
    ///   文件现场原封保留
    /// - [`FileRollbackOutcome::Unavailable`]：快照不可用，仅消息回退，文件未回退
    ///
    /// # 错误
    /// - [`RollbackError::Store`]：目标校验 / 存储读写失败（含
    ///   [`fuyao_session::SessionError::NotFound`]：session_id 不存在或 target_seq
    ///   无对应消息；`InvalidCutTarget`：目标非 user 且非 compaction——
    ///   两种模式共用同一校验，报错口径一致）
    /// - [`RollbackError::Restore`]：文件恢复失败——整个回退已中止（消息 / 快照行
    ///   原封），恢复幂等、可安全重试；仅消息模式不触碰文件，不产生此错误
    pub async fn rollback_session(
        &self,
        session_id: &str,
        target_seq: i64,
        rollback_files: bool,
    ) -> Result<FileRollbackOutcome, RollbackError> {
        // ① 查行（含目标校验，与预览共用；仅消息模式在此短路为 Skipped）
        let impact = self
            .resolve_file_impact(session_id, target_seq, rollback_files)
            .await?;

        // ② 先恢复文件：按请求跳过直接进入 DB 阶段；不可用 WARN 降级；
        //    有触碰集才执行恢复
        let outcome = match impact {
            FileImpact::Skipped => FileRollbackOutcome::Skipped,
            FileImpact::Unavailable => {
                tracing::warn!(
                    session_id = session_id,
                    target_seq = target_seq,
                    "快照不可用，回退降级为仅消息回退（文件未回退）"
                );
                FileRollbackOutcome::Unavailable
            }
            FileImpact::Empty => FileRollbackOutcome::Restored {
                restored: Vec::new(),
                deleted: Vec::new(),
            },
            FileImpact::Touch {
                baseline_tree,
                touched,
            } => {
                let restored = self
                    .snapshot
                    .restore(&baseline_tree, &touched)
                    .await
                    // Ok(None) 只在禁用态出现，resolve 已排除；防御性按降级处理
                    .map_err(|cause| RollbackError::Restore {
                        cause,
                        session_id: session_id.to_string(),
                        target_seq,
                    })?
                    .unwrap_or_default();
                FileRollbackOutcome::Restored {
                    restored: restored.restored,
                    deleted: restored.deleted,
                }
            }
        };

        // ③ 后动 DB：单事务删消息 + 同谓词删快照行 + 重算元数据
        self.store
            .rollback_to(session_id, target_seq)
            .await
            .map_err(RollbackError::Store)?;

        Ok(outcome)
    }

    /// 回退预览（只读双轴报告：将删消息 + 文件影响）
    ///
    /// 上层应用执行回退前的影响面查询：消息侧给出 `seq >= target` 的将删消息
    /// 清单（复用既有全量读路径过滤，seq 正序）；文件侧给出对基线树的只读分类
    /// ——将恢复 / 将删除两组清单（由快照器的 [`FileSnapshot::plan_restore`]
    /// 提供，restore 内部复用同一分类，预览与执行口径一致）。
    ///
    /// 与 [`SessionManager::rollback_session`] 共用 [`Self::resolve_file_impact`]
    /// （同一目标校验 + 同一影响计算），同一参数、同一状态下两者结论必然一致；
    /// 预览是建议、执行是权威——预览与执行之间状态可能漂移，执行的返回值
    /// 才是真实结论。本方法纯只读：不动消息、不动文件、不动快照行。
    ///
    /// # 文件侧模式（`rollback_files` 参数，与执行同参）
    ///
    /// - `true`：快照可用时给出「将恢复 / 将删除」分类清单；快照不可用时文件侧
    ///   诚实标注 [`FilesPreview::Unavailable`]——不展示虚假的文件影响清单
    ///   （执行时同样降级为仅消息回退）
    /// - `false`：仅消息模式，文件侧标注 [`FilesPreview::Skipped`]（「按请求
    ///   跳过」）——与 [`FilesPreview::Unavailable`]（快照不可用的降级）是两个
    ///   可辨的状态：前者是用户有意选择保留文件现场，后者是环境缺能力
    ///
    /// # 返回
    /// - [`RollbackPreview::messages_to_delete`]：将删消息清单（含目标本身；
    ///   空清单不可能出现——目标消息本身总在删除范围；两种模式下同口径）
    /// - [`RollbackPreview::files`]：文件影响（可用 = 分类清单；不可用 = 降级
    ///   标注；仅消息模式 = 按请求跳过标注）
    ///
    /// # 错误
    /// 同 [`SessionManager::rollback_session`] 的 `Store` / `Plan` 两类（仅消息
    /// 模式不触碰快照器，不产生 `Plan` 类）。
    pub async fn preview_rollback(
        &self,
        session_id: &str,
        target_seq: i64,
        rollback_files: bool,
    ) -> Result<RollbackPreview, RollbackError> {
        // 文件侧：共用校验 + 影响计算（仅消息模式在此短路为 Skipped）
        let impact = self
            .resolve_file_impact(session_id, target_seq, rollback_files)
            .await?;
        let files = match impact {
            FileImpact::Skipped => FilesPreview::Skipped,
            FileImpact::Unavailable => FilesPreview::Unavailable,
            FileImpact::Empty => FilesPreview::Plan {
                to_restore: Vec::new(),
                to_delete: Vec::new(),
            },
            FileImpact::Touch {
                baseline_tree,
                touched,
            } => {
                let plan = self
                    .snapshot
                    .plan_restore(&baseline_tree, &touched)
                    .await
                    .map_err(RollbackError::Plan)?;
                // Ok(None) 只在禁用态出现，resolve 已排除；防御性按不可用标注
                plan.map_or(FilesPreview::Unavailable, |plan| FilesPreview::Plan {
                    to_restore: plan.to_restore,
                    to_delete: plan.to_delete,
                })
            }
        };

        // 消息侧：复用既有全量读路径过滤 seq >= target（seq 正序）
        let messages_to_delete = self
            .store
            .load_full_history(session_id)
            .await
            .map_err(RollbackError::Store)?
            .into_iter()
            .filter(|m| m.seq >= target_seq)
            .collect();

        Ok(RollbackPreview {
            messages_to_delete,
            files,
        })
    }

    // ── 会话派生（fork）──────────────────────────────────────────

    /// 把会话派生到目标消息之前（复制目标之前的全部消息到新独立会话，源会话不动）
    ///
    /// 复用存储层单事务原子执行体 [`SessionStore::fork_to`](fuyao_session::SessionStore::fork_to)
    /// （建新会话行 + 复制 `seq < target` 的消息 + 按复制结果聚合重算分支元数据），
    /// 返回新会话 id。分支的后续状态经既有读路径获取：`list_messages` 浏览分支消息，
    /// `get_session` 看分支元数据；继续对话经运行时门面
    /// [`App::resume_session`](crate::App::resume_session) 激活分支。
    ///
    /// # 切割语义
    ///
    /// 目标必须是 user 消息或 compaction 消息（assistant / tool 中间态拒绝）；
    /// 分支 = 目标消息之前的全部消息（含压缩前旧消息与更早的压缩边界）。
    /// 同一目标点上，回退删源会话的目标及其后消息，派生把目标之前的消息落成
    /// 新分支、源会话原样不动——分支消息集即回退后源会话的剩余消息集，
    /// 派生是回退的非破坏版本。
    ///
    /// # 运行态责任边界
    ///
    /// 派生无需先停 turn：消息 seq 单调递增且插入后不改，活跃 turn 的落库只发生
    /// 在目标之后，分支快照（`seq < target`）不受影响。
    ///
    /// # 错误
    /// - [`fuyao_session::SessionError::NotFound`]：session_id 不存在，或 target_seq
    ///   在该 session 中无对应消息
    /// - [`fuyao_session::SessionError::InvalidCutTarget`]：目标非 user 且非
    ///   compaction（assistant / tool 中间态）
    pub async fn fork_session(
        &self,
        session_id: &str,
        target_seq: i64,
    ) -> Result<String, fuyao_session::SessionError> {
        self.store.fork_to(session_id, target_seq).await
    }

    // ── 消息查询 ───────────────────────────────────────────────

    /// 默认分页大小（每页消息条数）
    ///
    /// 向上滚动加载历史的常见档位：既不因每页过少而频繁请求，也不因过多撑爆渲染。
    /// 调用方可经 `limit` 参数覆盖此默认值。
    const DEFAULT_MESSAGE_PAGE_SIZE: i64 = 50;

    /// 游标分页加载历史消息（给人看的历史浏览，seq 倒序）
    ///
    /// 打开会话先看最新一页，向上滚动加载更早消息。与给 LLM 构造请求的可见窗口
    /// （`SessionStore::load_visible_messages`）是正交两条路径，互不影响。
    ///
    /// # 游标分页（不用 OFFSET）
    ///
    /// 消息是持续追加的流，OFFSET 基于「位置」分页，新消息插入会让整页内容向后漂移、
    /// 重复或遗漏。本接口基于消息的稳定标识 `seq`（单调递增、插入后永不改）做游标分页：
    ///
    /// - `before_seq = None`：从最新一条开始（第一页）
    /// - `before_seq = Some(N)`：取 `seq < N` 的更早一页，锚点本身不含
    ///
    /// 返回 [`MessagePage`](fuyao_api::MessagePage) 信封：`has_more` 判是否还有更早页，
    /// `next_cursor` 给翻页锚点（本页最旧消息的 seq，直接回传作下次 `before_seq`）。
    /// 游标分页不提供总数——消息是持续追加的流，total 会在新消息到达时过时、误导前端。
    ///
    /// # 参数
    ///
    /// - `session_id`：会话 ID
    /// - `before_seq`：游标锚点。`None` 取第一页（最新），`Some(N)` 向前翻（取 seq < N）
    /// - `limit`：每页条数。`None` 用 [`DEFAULT_MESSAGE_PAGE_SIZE`](Self::DEFAULT_MESSAGE_PAGE_SIZE)
    ///
    /// # 压缩消息处理
    ///
    /// compaction 消息（摘要）当作对话流里的一个普通节点正常显示，不过滤——
    /// 全部消息（普通 + 压缩）按 seq 倒序一起分页。
    pub async fn list_messages(
        &self,
        session_id: &str,
        before_seq: Option<i64>,
        limit: Option<i64>,
    ) -> Result<fuyao_api::MessagePage, fuyao_session::SessionError> {
        let limit = limit.unwrap_or(Self::DEFAULT_MESSAGE_PAGE_SIZE).max(1);
        let messages = self
            .store
            .list_messages_before(session_id, before_seq, limit)
            .await?;

        // has_more 由「本页条数 == limit」推导（满页才可能还有更多）。
        // next_cursor 取 messages（seq 倒序）末条——即本页最旧一条的 seq，作下次 before_seq。
        let has_more = messages.len() as i64 == limit;
        let next_cursor = if has_more {
            messages.last().map(|m| m.seq)
        } else {
            None
        };
        Ok(fuyao_api::MessagePage {
            items: messages,
            has_more,
            next_cursor,
        })
    }

    /// 历史消息投影成事件流（历史回放，seq 正序）
    ///
    /// 与 [`list_messages`](Self::list_messages) 同源取数（同游标、同分页），但把存储
    /// [`Message`] 投影成与实时流同构的 [`OutputEvent`]——前端历史回放与实时流共用一套
    /// 渲染逻辑，无需区分数据来源。转换由 [`fuyao_core::messages_to_events`] 承担
    /// （含 tool_calls 嵌套 → 扁平的逆向、seq 倒序翻正序），映射知识归 core 的
    /// history 模块——与「事件 → Message 落库」的正向映射同居一处，双向单点同步。
    ///
    /// 返回 [`EventPage`](fuyao_api::EventPage) 信封：`has_more` / `next_cursor` 直接复用
    /// [`list_messages`](Self::list_messages) 的推导（同源同游标），仅把 `items` 投影成
    /// `events`。游标分页不提供总数——消息是持续追加的流，total 会在新消息到达时过时。
    ///
    /// # 参数
    /// 同 [`list_messages`](Self::list_messages)。
    pub async fn list_events(
        &self,
        session_id: &str,
        before_seq: Option<i64>,
        limit: Option<i64>,
    ) -> Result<fuyao_api::EventPage, fuyao_session::SessionError> {
        // 复用 list_messages 的取数 + 游标推导，避免两处重复实现 limit/has_more/cursor 逻辑。
        let page = self.list_messages(session_id, before_seq, limit).await?;
        let events = fuyao_core::messages_to_events(page.items);
        Ok(fuyao_api::EventPage {
            events,
            has_more: page.has_more,
            next_cursor: page.next_cursor,
        })
    }
}

/// 回退文件侧影响（预览与执行共用的中间计算结果）
///
/// 由 [`SessionManager::resolve_file_impact`] 产出：预览据此标注文件影响面，
/// 执行据此编排恢复——同一状态下两条入口拿到同一份结论，口径一致有构造性保证。
enum FileImpact {
    /// 仅消息模式（rollback_files = false）：文件侧按请求跳过——有意选择非降级，
    /// 不查快照行、不判可用性，与快照状态无关
    Skipped,
    /// 快照不可用（配置关闭 / git 缺失）：文件侧无法联动
    Unavailable,
    /// 目标后无快照行（回退点之后没有工具批）：文件侧无事可做
    Empty,
    /// 有快照行：`baseline_tree` 为首行基线树，`touched` 为各行 files 的
    /// 排序去重并集（恢复的触碰集）
    Touch {
        baseline_tree: String,
        touched: Vec<String>,
    },
}

/// 回退执行结果（文件侧联动结论 + 双清单）
///
/// [`SessionManager::rollback_session`] 的返回值：消息侧恒已回退（或整体失败
/// 上抛），文件侧的结论由此类型明示——快照不可用时明确告知「文件未回退」，
/// 调用方据比向用户交代文件现场状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRollbackOutcome {
    /// 文件侧已联动恢复：清单为恢复（回到回退点内容）与删除（回退点后新建）
    /// 两组工作区相对路径，已排序去重；可能为空（目标后无触碰集）
    Restored {
        /// 已恢复为回退点内容的文件清单
        restored: Vec<String>,
        /// 已删除的文件清单（回退点之后新建）
        deleted: Vec<String>,
    },
    /// 仅消息模式（`rollback_files = false`）：文件侧按请求跳过，文件现场原封
    /// 保留。语义代价：该段快照行已随消息删除（账已销）——之后再回退更早的
    /// 回退点，被保留的文件不会被恢复或删除
    Skipped,
    /// 快照不可用（配置关闭 / git 缺失）：仅消息回退，文件未回退
    Unavailable,
}

/// 回退预览（只读双轴报告）
///
/// [`SessionManager::preview_rollback`] 的返回值：执行前的建议性影响面——
/// 预览是建议、执行是权威，两者之间状态可能漂移，真实结果以执行返回值
/// [`FileRollbackOutcome`] 为准。
#[derive(Debug, Clone)]
pub struct RollbackPreview {
    /// 将删除的消息（`seq >= target`，seq 正序，含目标本身）
    pub messages_to_delete: Vec<Message>,
    /// 文件侧影响（不可用时诚实标注）
    pub files: FilesPreview,
}

/// 文件侧预览结论（对基线树的只读分类 / 跳过与降级标注）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesPreview {
    /// 快照可用：触碰集对基线树的「将恢复 / 将删除」分类清单（可能为空）
    Plan {
        /// 将恢复为回退点内容的文件清单
        to_restore: Vec<String>,
        /// 将删除的文件清单（回退点之后新建）
        to_delete: Vec<String>,
    },
    /// 仅消息模式（`rollback_files = false`）：文件侧按请求跳过——用户有意
    /// 选择保留文件现场（执行时文件零动作、无文件事件）；与
    /// [`FilesPreview::Unavailable`] 的降级语义可区分
    Skipped,
    /// 快照不可用：文件侧无法预览（执行时同样降级为仅消息回退）
    Unavailable,
}

/// 回退门面错误（预览与执行共用）
#[derive(Debug, thiserror::Error)]
pub enum RollbackError {
    /// 目标校验 / 存储读写失败
    #[error("存储访问失败: {0}")]
    Store(#[from] fuyao_session::SessionError),
    /// 只读预览的快照分类失败
    #[error("文件影响预览失败: {0}")]
    Plan(#[from] fuyao_snapshot::SnapshotError),
    /// 文件恢复失败：整个回退已中止（消息与快照行未改动），恢复幂等、可安全重试
    #[error("文件恢复失败，回退已整体中止（消息与快照行未改动，可安全重试）: {cause}")]
    Restore {
        /// 快照器报错原因
        cause: fuyao_snapshot::SnapshotError,
        /// 回退的会话
        session_id: String,
        /// 回退目标消息 seq
        target_seq: i64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyao_session::SessionError;
    use fuyao_snapshot::DEFAULT_MAX_UNTRACKED_MB;
    use rstest::rstest;

    /// 构造临时 SQLite 上的 SessionManager + 一个已建会话
    ///
    /// std::mem::forget(dir) 放弃 TempDir 自动清理——async 测试跨 await 持有路径，
    /// TempDir 提前 drop 会删掉 db 文件；临时目录由系统重启时清理。
    /// 未调 set_config，get_config 返回 default（max_len = 80）。
    /// 快照器恒为禁用态（本组测试只覆盖标题 / 查询路径）。
    async fn manager_with_session() -> (SessionManager, String) {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let store = Arc::new(
            SessionStore::new(dir.path().join("test.db"))
                .await
                .expect("构造 SessionStore 失败"),
        );
        std::mem::forget(dir);
        let session = store
            .create_session(None, None, None)
            .await
            .expect("建会话失败");
        (
            SessionManager::new(store, FileSnapshot::disabled()),
            session.id,
        )
    }

    /// 空串与纯空白标题被拒，错误携带配置的 max_len 真值
    #[rstest]
    #[case::empty("")]
    #[case::spaces("   ")]
    #[case::mixed_whitespace(" \t\r\n ")]
    #[tokio::test]
    async fn update_title_rejects_blank(#[case] title: &str) {
        let (manager, session_id) = manager_with_session().await;
        let err = manager
            .update_title(&session_id, title)
            .await
            .expect_err("空白标题应被拒");
        assert!(
            matches!(err, SessionError::InvalidTitle { max_len: 80 }),
            "实际错误：{err:?}"
        );
    }

    /// 超过 max_len 的标题被拒（按字符数计，多字节字符各计 1），恰好等于 max_len 的边界通过
    #[rstest]
    #[case::ascii_over("a", 81)]
    #[case::multibyte_over("话", 81)]
    #[tokio::test]
    async fn update_title_rejects_over_max_len(#[case] ch: &str, #[case] len: usize) {
        let (manager, session_id) = manager_with_session().await;
        let over = ch.repeat(len);
        let err = manager
            .update_title(&session_id, &over)
            .await
            .expect_err("超长标题应被拒");
        assert!(
            matches!(err, SessionError::InvalidTitle { max_len: 80 }),
            "实际错误：{err:?}"
        );
    }

    /// 恰好等于 max_len 的边界标题通过且原样落库（默认 max_len = 80，多字节各计 1）
    #[tokio::test]
    async fn update_title_accepts_exactly_max_len() {
        let (manager, session_id) = manager_with_session().await;
        let exact = "话".repeat(80);
        manager
            .update_title(&session_id, &exact)
            .await
            .expect("恰好 max_len 的标题应通过");
        let stored = manager
            .get_session(&session_id)
            .await
            .expect("读会话失败")
            .expect("会话应存在");
        assert_eq!(stored.title.as_deref(), Some(exact.as_str()));
    }

    /// 合法标题通过且落库的是 trim 后的规范化值
    #[tokio::test]
    async fn update_title_stores_trimmed_title() {
        let (manager, session_id) = manager_with_session().await;
        manager
            .update_title(&session_id, "  新标题  ")
            .await
            .expect("合法标题应通过");
        let stored = manager
            .get_session(&session_id)
            .await
            .expect("读会话失败")
            .expect("会话应存在");
        assert_eq!(stored.title.as_deref(), Some("新标题"));
    }

    /// 非法标题在校验即被拒，不产生存储访问——不存在的会话 id 同样报标题错误
    #[tokio::test]
    async fn update_title_validates_before_store_access() {
        let (manager, _session_id) = manager_with_session().await;
        let err = manager
            .update_title("no-such-session", "   ")
            .await
            .expect_err("空白标题应在校验层被拒");
        assert!(
            matches!(err, SessionError::InvalidTitle { max_len: 80 }),
            "实际错误：{err:?}"
        );
    }

    // ===== 回退编排 + 只读预览 =====

    /// 回退测试台：临时工作区（真实影子仓）+ 临时 SQLite + 已建会话
    struct RollbackFixture {
        manager: SessionManager,
        session_id: String,
        /// 工作区目录（断言文件终态用）
        worktree: std::path::PathBuf,
    }

    /// 落一条消息，返回其 seq
    async fn insert_message(store: &SessionStore, sid: &str, msg: Message) -> i64 {
        let mut msg = msg;
        store.insert_message(sid, &mut msg).await.unwrap()
    }

    /// 构造带真实影子仓的回退测试台
    ///
    /// std::mem::forget 放弃 TempDir 自动清理——async 测试跨 await 持有路径。
    async fn rollback_fixture() -> RollbackFixture {
        let db_dir = tempfile::tempdir().expect("创建临时目录失败");
        let store = Arc::new(
            SessionStore::new(db_dir.path().join("test.db"))
                .await
                .expect("构造 SessionStore 失败"),
        );
        std::mem::forget(db_dir);
        let ws = tempfile::tempdir().expect("创建工作区失败");
        let worktree = ws.path().to_path_buf();
        std::mem::forget(ws);
        let shadow = tempfile::tempdir().expect("创建影子仓目录失败");
        let shadow_root = shadow.path().to_path_buf();
        std::mem::forget(shadow);
        let snapshot = FileSnapshot::new(&worktree, &shadow_root, DEFAULT_MAX_UNTRACKED_MB).await;
        assert!(snapshot.is_enabled(), "测试前提：git 在 PATH，影子仓可用");

        let session = store
            .create_session(None, None, None)
            .await
            .expect("建会话失败");
        RollbackFixture {
            manager: SessionManager::new(store, snapshot),
            session_id: session.id,
            worktree,
        }
    }

    /// 模拟一个工具批：assistant 消息落库（快照行锚点）→ 基线采集落行 → 施加
    /// 一批文件改动（模拟工具执行效果），返回 (assistant seq, 基线树)
    async fn simulate_tool_batch(
        fx: &RollbackFixture,
        prev_tree: Option<&str>,
        mutations: &[(&str, &str)],
    ) -> (i64, String) {
        let seq = insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::assistant(Some("调工具".to_string())),
        )
        .await;
        let outcome = fx
            .manager
            .snapshot
            .track(prev_tree)
            .await
            .expect("采集应成功")
            .expect("可用态应有结果");
        fx.manager
            .store
            .insert_file_snapshot(&fx.session_id, seq, &outcome.tree_hash, &outcome.files)
            .await
            .unwrap();
        for (rel, content) in mutations {
            let path = fx.worktree.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }
        (seq, outcome.tree_hash)
    }

    /// 端到端：预览双轴清单正确 → 回退文件终态与消息终态一致 → 事件载荷与预览
    /// 结论一致 → 快照行清理
    ///
    /// 会话结构：u1 → 批1（改 a.txt）→ u2 → 批2（新建 n.txt）→ u3 → 批3（再改
    /// a.txt）→ a4 纯回复。回退到 u2：窗口行 = 批2 / 批3 的行，基线树 = 批2 执行
    /// 前的树，触碰集 = 两行 files 并集——a.txt 恢复为基线内容、n.txt（基线树
    /// 之后新建）删除。
    #[tokio::test]
    async fn rollback_restores_files_deletes_messages_and_emits_event() {
        let fx = rollback_fixture().await;
        std::fs::write(fx.worktree.join("a.txt"), "v1").unwrap();
        std::fs::write(fx.worktree.join("b.txt"), "v1").unwrap();
        insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u1".into()),
        )
        .await;
        let (_, tree1) = simulate_tool_batch(&fx, None, &[("a.txt", "v2 批1修改")]).await;
        let u2 = insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u2".into()),
        )
        .await;
        let (_, tree2) = simulate_tool_batch(&fx, Some(&tree1), &[("n.txt", "新建")]).await;
        insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u3".into()),
        )
        .await;
        simulate_tool_batch(&fx, Some(&tree2), &[("a.txt", "v3 批3修改")]).await;
        insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::assistant(Some("a4".into())),
        )
        .await;

        // 预览：将删消息 = u2 及其后（u2 / a2 / u3 / a3 / a4，seq 正序）；
        // 文件影响 = a.txt 恢复为基线内容、n.txt（基线树外）删除
        let preview = fx
            .manager
            .preview_rollback(&fx.session_id, u2, true)
            .await
            .expect("预览应成功");
        let seqs: Vec<i64> = preview.messages_to_delete.iter().map(|m| m.seq).collect();
        assert_eq!(seqs.len(), 5, "u2 与其后共 5 条");
        assert_eq!(seqs[0], u2, "seq 正序，首条即目标");
        assert_eq!(
            preview.files,
            FilesPreview::Plan {
                to_restore: vec!["a.txt".to_string()],
                to_delete: vec!["n.txt".to_string()],
            },
            "预览文件影响：窗口触碰集对基线树的分类"
        );

        // 执行：文件终态与消息终态一致
        let outcome = fx
            .manager
            .rollback_session(&fx.session_id, u2, true)
            .await
            .expect("回退应成功");
        assert_eq!(
            outcome,
            FileRollbackOutcome::Restored {
                restored: vec!["a.txt".to_string()],
                deleted: vec!["n.txt".to_string()],
            }
        );
        assert_eq!(
            std::fs::read_to_string(fx.worktree.join("a.txt")).unwrap(),
            "v2 批1修改",
            "批3 的再修改应回到基线（批2 执行前）内容"
        );
        assert!(
            !fx.worktree.join("n.txt").exists(),
            "基线树之后新建的文件应被删除"
        );
        assert_eq!(
            std::fs::read_to_string(fx.worktree.join("b.txt")).unwrap(),
            "v1",
            "触碰集外文件原封不动"
        );
        // 消息侧：只剩 u1 / a1
        let full = fx
            .manager
            .store
            .load_full_history(&fx.session_id)
            .await
            .unwrap();
        assert_eq!(full.len(), 2, "u2 及其后消息已删");
        // 快照行同谓词清理
        let rows = fx
            .manager
            .store
            .list_file_snapshots_from(&fx.session_id, 0)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "只剩批1的快照行");
    }

    /// 模拟 turn 收尾补拍：最终回复 assistant 消息落库 → 以它为锚点再采一行
    /// （files = 自最新一行以来的差异 = 终批工具的变更窗口），不施加文件改动。
    /// 返回 (锚点 seq, 收尾行基线树)
    async fn simulate_turn_close(fx: &RollbackFixture, prev_tree: Option<&str>) -> (i64, String) {
        let seq = insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::assistant(Some("最终回复".to_string())),
        )
        .await;
        let outcome = fx
            .manager
            .snapshot
            .track(prev_tree)
            .await
            .expect("采集应成功")
            .expect("可用态应有结果");
        fx.manager
            .store
            .insert_file_snapshot(&fx.session_id, seq, &outcome.tree_hash, &outcome.files)
            .await
            .unwrap();
        (seq, outcome.tree_hash)
    }

    /// 触碰集口径：窗口内各行的记录差异全量进入触碰集——批边界行记批间增量，
    /// 收尾行记终批变更窗口。回退刚结束的 turn 时，终批的净新建被删、净修改被复原
    #[tokio::test]
    async fn rollback_touch_set_covers_turn_final_batch_diffs() {
        let fx = rollback_fixture().await;
        std::fs::write(fx.worktree.join("a.txt"), "v1").unwrap();
        insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u1".into()),
        )
        .await;
        // turn1：批1 改 a.txt → 收尾行（锚定最终回复）承载批1 的效果
        let (_, tree1) = simulate_tool_batch(&fx, None, &[("a.txt", "v2")]).await;
        let (_, close1_tree) = simulate_turn_close(&fx, Some(&tree1)).await;
        // turn2（刚结束的 turn）：终批既再改 a.txt 又新建 z.txt → 收尾行承载终批全部变更
        let u2 = insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u2".into()),
        )
        .await;
        let (_, tree2) = simulate_tool_batch(
            &fx,
            Some(&close1_tree),
            &[("a.txt", "v3 终批修改"), ("z.txt", "终批新建")],
        )
        .await;
        simulate_turn_close(&fx, Some(&tree2)).await;

        let outcome = fx
            .manager
            .rollback_session(&fx.session_id, u2, true)
            .await
            .expect("回退应成功");
        assert_eq!(
            outcome,
            FileRollbackOutcome::Restored {
                restored: vec!["a.txt".to_string()],
                deleted: vec!["z.txt".to_string()],
            }
        );
        assert_eq!(
            std::fs::read_to_string(fx.worktree.join("a.txt")).unwrap(),
            "v2",
            "终批的再修改应回到基线（终批执行前）内容"
        );
        assert!(
            !fx.worktree.join("z.txt").exists(),
            "终批的净新建变更由收尾行承载，回退时删除"
        );
    }

    /// 目标后无工具批（纯对话）：文件侧零动作（空清单联动），消息照常回退
    #[tokio::test]
    async fn rollback_without_snapshots_skips_file_side() {
        let fx = rollback_fixture().await;
        insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u1".into()),
        )
        .await;
        let u2 = insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u2".into()),
        )
        .await;
        insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::assistant(Some("a2".into())),
        )
        .await;

        // 预览：文件侧可用但为空计划
        let preview = fx
            .manager
            .preview_rollback(&fx.session_id, u2, true)
            .await
            .expect("预览应成功");
        assert_eq!(
            preview.files,
            FilesPreview::Plan {
                to_restore: vec![],
                to_delete: vec![],
            }
        );

        // 执行：空清单联动
        let outcome = fx
            .manager
            .rollback_session(&fx.session_id, u2, true)
            .await
            .expect("回退应成功");
        assert_eq!(
            outcome,
            FileRollbackOutcome::Restored {
                restored: vec![],
                deleted: vec![],
            }
        );
        let full = fx
            .manager
            .store
            .load_full_history(&fx.session_id)
            .await
            .unwrap();
        assert_eq!(full.len(), 1, "消息照常回退");
    }

    /// 快照禁用态：预览诚实标注不可用，回退降级为仅消息（明示文件未回退）
    #[tokio::test]
    async fn disabled_snapshot_degrades_to_message_only_rollback() {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let store = Arc::new(
            SessionStore::new(dir.path().join("test.db"))
                .await
                .expect("构造 SessionStore 失败"),
        );
        std::mem::forget(dir);
        let ws = tempfile::tempdir().expect("创建工作区失败");
        let worktree = ws.path().to_path_buf();
        std::fs::write(worktree.join("a.txt"), "现场").unwrap();
        std::mem::forget(ws);
        let session = store.create_session(None, None, None).await.unwrap();
        // 历史上落过快照行（禁用态不查行，直接降级）
        let u1 = {
            let mut m = Message::user("u1".into());
            store.insert_message(&session.id, &mut m).await.unwrap()
        };
        {
            let mut m = Message::assistant(Some("a1".into()));
            let seq = store.insert_message(&session.id, &mut m).await.unwrap();
            store
                .insert_file_snapshot(&session.id, seq, "tree_x", &["a.txt".to_string()])
                .await
                .unwrap();
        }
        let manager = SessionManager::new(store, FileSnapshot::disabled());

        let preview = manager
            .preview_rollback(&session.id, u1, true)
            .await
            .expect("预览应成功");
        assert_eq!(preview.files, FilesPreview::Unavailable);

        let outcome = manager
            .rollback_session(&session.id, u1, true)
            .await
            .expect("消息回退照常");
        assert_eq!(outcome, FileRollbackOutcome::Unavailable);
        assert_eq!(
            std::fs::read_to_string(worktree.join("a.txt")).unwrap(),
            "现场",
            "文件现场原封不动"
        );
    }

    /// 恢复失败：整个回退中止——消息 / 快照行原封，错误明示可重试
    #[tokio::test]
    async fn restore_failure_aborts_whole_rollback() {
        let fx = rollback_fixture().await;
        insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u1".into()),
        )
        .await;
        // 落一行指向不存在基线树的快照（影子仓没有该对象，恢复必然失败）
        let u2 = insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u2".into()),
        )
        .await;
        {
            let mut m = Message::assistant(Some("a2".into()));
            let seq = fx
                .manager
                .store
                .insert_message(&fx.session_id, &mut m)
                .await
                .unwrap();
            fx.manager
                .store
                .insert_file_snapshot(
                    &fx.session_id,
                    seq,
                    "0000000000000000000000000000000000000000",
                    &["a.txt".to_string()],
                )
                .await
                .unwrap();
        }

        let err = fx
            .manager
            .rollback_session(&fx.session_id, u2, true)
            .await
            .expect_err("基线树不存在，恢复应失败");
        assert!(
            matches!(err, RollbackError::Restore { .. }),
            "应为 Restore 错误，实际：{err:?}"
        );
        assert!(
            err.to_string().contains("可安全重试"),
            "错误信息应明示可重试：{err}"
        );
        // 消息与快照行原封
        let full = fx
            .manager
            .store
            .load_full_history(&fx.session_id)
            .await
            .unwrap();
        assert_eq!(full.len(), 3, "回退中止，消息原封");
        let rows = fx
            .manager
            .store
            .list_file_snapshots_from(&fx.session_id, 0)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "快照行原封");
    }

    /// 预览与执行共用目标校验：非法目标（assistant 中间态）两个入口报同一错误
    #[tokio::test]
    async fn preview_and_rollback_share_target_validation() {
        let fx = rollback_fixture().await;
        insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u1".into()),
        )
        .await;
        let a1 = insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::assistant(Some("a1".into())),
        )
        .await;

        let preview_err = fx
            .manager
            .preview_rollback(&fx.session_id, a1, true)
            .await
            .expect_err("assistant 目标应被预览拒绝");
        let rollback_err = fx
            .manager
            .rollback_session(&fx.session_id, a1, true)
            .await
            .expect_err("assistant 目标应被回退拒绝");
        assert!(
            matches!(
                (&preview_err, &rollback_err),
                (
                    RollbackError::Store(SessionError::InvalidCutTarget(_)),
                    RollbackError::Store(SessionError::InvalidCutTarget(_))
                )
            ),
            "两个入口应报同一 InvalidCutTarget：{preview_err:?} vs {rollback_err:?}"
        );
        // 消息原封
        let full = fx
            .manager
            .store
            .load_full_history(&fx.session_id)
            .await
            .unwrap();
        assert_eq!(full.len(), 2);
    }

    /// 预览是纯只读：预览后消息 / 文件 / 快照行全部原封
    #[tokio::test]
    async fn preview_is_readonly() {
        let fx = rollback_fixture().await;
        std::fs::write(fx.worktree.join("a.txt"), "v1").unwrap();
        insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u1".into()),
        )
        .await;
        let u2 = insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u2".into()),
        )
        .await;
        simulate_tool_batch(&fx, None, &[("a.txt", "v2")]).await;

        let _preview = fx
            .manager
            .preview_rollback(&fx.session_id, u2, true)
            .await
            .expect("预览应成功");
        assert_eq!(
            std::fs::read_to_string(fx.worktree.join("a.txt")).unwrap(),
            "v2",
            "预览不动文件现场"
        );
        let full = fx
            .manager
            .store
            .load_full_history(&fx.session_id)
            .await
            .unwrap();
        assert_eq!(full.len(), 3, "预览不动消息");
        let rows = fx
            .manager
            .store
            .list_file_snapshots_from(&fx.session_id, 0)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "预览不动快照行");
    }

    /// 仅消息模式（rollback_files = false）：文件侧按请求跳过——文件原封不动、
    /// 消息与快照行照删、无文件事件；预览文件侧标注「按请求跳过」且与联动模式
    /// 的分类清单在同状态下可区分。之后再联动回退更早回退点，被保留的文件不被
    /// 恢复或删除（该段快照行已随消息删除，账已销）。
    ///
    /// 会话结构：u1 → 批1（改 a.txt）→ u2 → 批2（再改 a.txt、新建 n.txt）→
    /// 收尾行（承载批2 的变更窗口）。仅消息回退到 u2：a.txt 保留批2 内容、
    /// n.txt 保留在盘上，u2 及其后消息与批2 / 收尾的快照行删除。
    #[tokio::test]
    async fn messages_only_rollback_preserves_files_and_clears_ledger() {
        let fx = rollback_fixture().await;
        std::fs::write(fx.worktree.join("a.txt"), "v1").unwrap();
        let u1 = insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u1".into()),
        )
        .await;
        let (_, tree1) = simulate_tool_batch(&fx, None, &[("a.txt", "v2 批1修改")]).await;
        let u2 = insert_message(
            &fx.manager.store,
            &fx.session_id,
            Message::user("u2".into()),
        )
        .await;
        let (_, tree2) = simulate_tool_batch(
            &fx,
            Some(&tree1),
            &[("a.txt", "v3 批2修改"), ("n.txt", "批2新建")],
        )
        .await;
        simulate_turn_close(&fx, Some(&tree2)).await;

        // 预览两模式口径：同状态下仅消息模式标注 Skipped（按请求跳过），联动模式
        // 给出真实分类清单——两个状态可辨；消息侧两模式同口径
        let preview_skip = fx
            .manager
            .preview_rollback(&fx.session_id, u2, false)
            .await
            .expect("预览应成功");
        assert_eq!(preview_skip.files, FilesPreview::Skipped);
        assert_eq!(
            preview_skip.messages_to_delete.len(),
            3,
            "消息侧照常：u2 与其后（u2 / a2 / 收尾 a3）"
        );
        let preview_linked = fx
            .manager
            .preview_rollback(&fx.session_id, u2, true)
            .await
            .expect("预览应成功");
        assert_eq!(
            preview_linked.files,
            FilesPreview::Plan {
                to_restore: vec!["a.txt".to_string()],
                to_delete: vec!["n.txt".to_string()],
            },
            "联动模式预览不受仅消息预览影响，两状态可区分"
        );

        // 执行（仅消息）：文件原封、消息与快照行照删
        let outcome = fx
            .manager
            .rollback_session(&fx.session_id, u2, false)
            .await
            .expect("回退应成功");
        assert_eq!(outcome, FileRollbackOutcome::Skipped);
        assert_eq!(
            std::fs::read_to_string(fx.worktree.join("a.txt")).unwrap(),
            "v3 批2修改",
            "被保留的修改原样在盘上"
        );
        assert_eq!(
            std::fs::read_to_string(fx.worktree.join("n.txt")).unwrap(),
            "批2新建",
            "被保留的新建文件原样在盘上"
        );
        let full = fx
            .manager
            .store
            .load_full_history(&fx.session_id)
            .await
            .unwrap();
        assert_eq!(full.len(), 2, "消息侧照常回退（u1 / a1 保留）");
        // 快照行同谓词随 rollback_to 事务删除：只剩批1 的行
        let rows = fx
            .manager
            .store
            .list_file_snapshots_from(&fx.session_id, 0)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "仅剩批1的快照行");

        // 语义代价：之后再联动回退更早回退点（u1），被保留的文件不被恢复或删除
        // ——批2 的账已销，剩余触碰集（批1 首行，files 为空）不覆盖这些变更
        let outcome_early = fx
            .manager
            .rollback_session(&fx.session_id, u1, true)
            .await
            .expect("回退应成功");
        assert_eq!(
            outcome_early,
            FileRollbackOutcome::Restored {
                restored: vec![],
                deleted: vec![],
            }
        );
        assert_eq!(
            std::fs::read_to_string(fx.worktree.join("a.txt")).unwrap(),
            "v3 批2修改",
            "账已销：被保留的文件不会被恢复"
        );
        assert!(
            fx.worktree.join("n.txt").exists(),
            "账已销：被保留的新建文件不会被删除"
        );
    }

    /// 仅消息模式与快照状态无关：快照禁用 + rollback_files = false 时文件侧仍是
    /// 「按请求跳过」（Skipped）而非「不可用」降级（Unavailable）——有意选择
    /// 优先于环境能力标注；快照行照旧随事务删除、文件现场原封
    #[tokio::test]
    async fn messages_only_mode_marks_skipped_even_when_snapshot_disabled() {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let store = Arc::new(
            SessionStore::new(dir.path().join("test.db"))
                .await
                .expect("构造 SessionStore 失败"),
        );
        std::mem::forget(dir);
        let ws = tempfile::tempdir().expect("创建工作区失败");
        let worktree = ws.path().to_path_buf();
        std::fs::write(worktree.join("a.txt"), "现场").unwrap();
        std::mem::forget(ws);
        let session = store.create_session(None, None, None).await.unwrap();
        let u1 = {
            let mut m = Message::user("u1".into());
            store.insert_message(&session.id, &mut m).await.unwrap()
        };
        {
            let mut m = Message::assistant(Some("a1".into()));
            let seq = store.insert_message(&session.id, &mut m).await.unwrap();
            store
                .insert_file_snapshot(&session.id, seq, "tree_x", &["a.txt".to_string()])
                .await
                .unwrap();
        }
        let manager = SessionManager::new(store.clone(), FileSnapshot::disabled());

        // 禁用态 + 仅消息：标注是 Skipped（有意选择）而非 Unavailable（降级）
        let preview = manager
            .preview_rollback(&session.id, u1, false)
            .await
            .expect("预览应成功");
        assert_eq!(preview.files, FilesPreview::Skipped);

        let outcome = manager
            .rollback_session(&session.id, u1, false)
            .await
            .expect("消息回退照常");
        assert_eq!(outcome, FileRollbackOutcome::Skipped);
        assert_eq!(
            std::fs::read_to_string(worktree.join("a.txt")).unwrap(),
            "现场",
            "文件现场原封不动"
        );
        let rows = store
            .list_file_snapshots_from(&session.id, 0)
            .await
            .unwrap();
        assert!(rows.is_empty(), "快照行照旧随事务删除");
    }
}
