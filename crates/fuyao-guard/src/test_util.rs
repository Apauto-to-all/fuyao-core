//! guard 测试共享 fixture
//!
//! 提供驱动 LoopGuard 检测链路的事件构造器与 SessionSender 通道夹具，
//! 供本 crate 单元测试与集成测试（tests/）共用。模块以 `#[doc(hidden)] pub`
//! 暴露：集成测试编译 crate 时 `cfg(test)` 模块不存在，这是单测与集成测试共享
//! 同一实现的唯一通道；文档隐藏表明它不属于公开 API 契约。
//!
//! 夹具实现归属 fuyao-hooks 的 test_util（事件与 sender 均为 fuyao-api /
//! fuyao-hooks 类型），此处整体转发——同一实现单点维护。

pub use fuyao_hooks::test_util::{make_chunk, make_sender, make_tool_call, make_tool_result};
