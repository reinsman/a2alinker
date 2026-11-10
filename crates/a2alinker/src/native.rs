//! 无 HTTP 的原生执行 API，供 reind 在自己的进程内复用适配器。
//!
//! 调用 create_session 会直接启动原生子进程并由返回会话持有管道；
//! 不调用 Server::start，不绑定 HTTP 端口，也不创建 A2A Gateway。
//! 会话缓存、事件持久化、审批回复和全局关闭由宿主 reind 管理。
//! 宿主确认执行停止前应调用 AgentSession::shutdown，等待进程清理；Drop 仅兜底。
pub use crate::adapters::execution::redact_fields;
pub use crate::adapters::{
    AdapterContext, AgentEvent, AgentSession, Interaction, InteractionReply, RunOutcome, ToolEvent,
    ToolPhase, create_session, validate_reply,
};
