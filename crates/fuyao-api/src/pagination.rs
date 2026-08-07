//! 分页信封类型
//!
//! 三种分页场景的工业级返回结构,由 app 层(SessionManager)组装、store 层提供原子数据:
//! - [`SessionPage`]——会话列表的 OFFSET 分页,带总数(前端算总页数用)
//! - [`MessagePage`]——消息历史的游标分页(裸 Message,带下一页游标)
//! - [`EventPage`]——消息历史的游标分页(投影成 OutputEvent,带下一页游标)
//!
//! [`MessagePage`] 与 [`EventPage`] 同源取数、同游标语义,仅承载类型不同:
//! 前者给需要原始 Message 的场景,后者给前端历史回放(与实时流同构)。两者均不提供总数——
//! 见下方「为什么两种分页形态不同」。
//!
//! # 为什么两种分页形态不同
//!
//! 会话列表是「相对静态的有限集合」(用户历史会话,增长缓慢),OFFSET 分页天然配总数——
//! 前端要显示「共 N 条 / 第 x 页」,total 是必需的。
//!
//! 消息历史是「持续追加的流」,游标分页天然配 cursor 而非总数——流的总数一直在变,
//! 给一个 total 会在新消息到达时立刻过时、误导前端。游标分页的正确语义是「翻到没有就停」:
//! `has_more` 判边界,`next_cursor` 给锚点,不需要 total。

use crate::Message;
use crate::OutputEvent;
use crate::Session;

/// OFFSET 分页结果(会话列表用,带总数)
///
/// 由 [`crate::session_types::Session`] 列表 + 分页元信息组成。`total` 是过滤后的总数
/// (含 workspace 过滤),与 `items` 同源查询,供前端计算总页数。`limit` / `offset` 回显
/// 本次分页参数,前端据此判断当前页位。
///
/// # 弱一致说明
///
/// `items` 与 `total` 是两条独立查询,极端情况下两次查询之间有新会话插入会导致二者
/// 差一两条。这对「列表分页给 UI 算页数」是可接受的——过时的 1-2 条不影响翻页体验。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionPage {
    /// 当前页的会话列表
    pub items: Vec<Session>,
    /// 过滤后的总会话数(前端算总页数用)
    pub total: i64,
    /// 本次分页单页条数(回显)
    pub limit: i64,
    /// 本次分页偏移量(回显,前端据此判断当前页位)
    pub offset: i64,
}

/// 游标分页结果(裸消息,带下一页游标)
///
/// 由 [`Message`] 列表 + 翻页信号组成。与 [`EventPage`] 同源取数、同游标语义,仅承载类型
/// 不同:本类型给需要原始 Message 的场景, [`EventPage`] 给前端历史回放(投影成 OutputEvent,
/// 与实时流同构)。游标分页不提供总数——消息是持续追加的流, total 会在新消息到达时过时。
///
/// # 翻页方式
///
/// - `has_more == false` 或 `next_cursor == None`:已到最早,无更多历史
/// - `has_more == true`:`next_cursor` 是本页最旧一条消息的 seq,前端直接回传作下一次
///   请求的 `before_seq` 即可取更早一页
///
/// # 推导依据
///
/// `has_more` 由「本页返回条数 == 请求 limit」推导(满页才可能还有更多);`next_cursor`
/// 在 `has_more` 为真时取本页最旧消息的 seq。二者由 app 层从 store 取回的 `Vec<Message>`
/// 推导, store 层不感知游标语义。
///
/// **整数倍边界**:当总条数恰为 limit 的整数倍时,最后一页仍判 `has_more = true`(满页),
/// 消费者翻下一页会拿到空列表才确认到底。这是 len==limit 推导法的固有保守行为,避免漏取
/// 最后一页——代价是多一次空请求,对历史浏览可接受。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessagePage {
    /// 当前页的消息列表(seq 倒序,新 → 旧;与 store 取回顺序一致,不翻正序)
    pub items: Vec<Message>,
    /// 是否还有更早的历史页
    pub has_more: bool,
    /// 下一页游标(本页最旧消息的 seq);`has_more == false` 时为 `None`。
    /// 翻页时直接回传作 `before_seq`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<i64>,
}

/// 游标分页结果(投影成事件,带下一页游标)
///
/// 由 [`OutputEvent`] 列表 + 翻页信号组成。游标分页不提供总数——消息是持续追加的流,
/// total 会在新消息到达时过时;正确语义是「翻到没有就停」:`has_more` 判是否还有更早页,
/// `next_cursor` 给翻页锚点。
///
/// # 翻页方式
///
/// - `has_more == false` 或 `next_cursor == None`:已到最早,无更多历史
/// - `has_more == true`:`next_cursor` 是本页最旧一条消息的 seq,前端直接回传作下一次
///   请求的 `before_seq` 即可取更早一页
///
/// # 推导依据
///
/// 与 [`MessagePage`] 完全一致(has_more / next_cursor 直接复用 list_messages 的推导,
/// 仅把 items 投影成 events),含整数倍边界的保守行为,详见 [`MessagePage`] 的推导依据。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventPage {
    /// 当前页的事件列表(seq 正序,旧 → 新)
    pub events: Vec<OutputEvent>,
    /// 是否还有更早的历史页
    pub has_more: bool,
    /// 下一页游标(本页最旧消息的 seq);`has_more == false` 时为 `None`。
    /// 翻页时直接回传作 `before_seq`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<i64>,
}
