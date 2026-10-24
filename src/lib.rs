//! a2alinker 的库入口：把不同 agent 的原生协议封装成统一的 A2A 服务。
//!
//! 二进制只解析参数，再使用 Options 和 Server 构建服务；库不依赖 CLI 模块。
//! `a2a` 负责网络协议，`runtime` 负责调度，`adapters` 负责进程通信，
//! `storage` 负责持久化；这些职责分别放在独立模块中。

pub mod a2a;
pub mod adapters;
pub mod options;
pub mod runtime;
pub mod server;
pub mod storage;

pub use options::{AgentKind, Options};
pub use server::Server;
