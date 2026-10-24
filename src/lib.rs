//! a2alinker 的库入口：把不同 agent 的原生协议封装成统一的 A2A 服务。
//!
//! 命令行入口位于 src/bin/a2alinker.rs；daemon 可通过 Server::start 管理多个实例。
//! `a2a` 负责网络协议，`runtime` 负责调度，`adapters` 负责进程通信，
//! `storage` 负责持久化；这些职责分别放在独立模块中。

mod a2a;
mod adapters;
pub mod options;
mod runtime;
mod server;
mod storage;

pub use options::{AgentKind, AuthMode, AuthOptions, Options};
pub use server::{Server, ServerHandle};

#[cfg(test)]
#[path = "../tests/internal/mod.rs"]
mod tests;
