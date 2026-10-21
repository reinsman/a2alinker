//! a2alinker 的库入口：把不同 agent 的原生协议封装成统一的 A2A 服务。
//!
//! 建议先读 `main.rs`、`cli`，再读 `runtime` 的任务流程，最后看具体适配器。
//! `a2a` 负责网络协议，`runtime` 负责调度，`adapters` 负责进程通信，
//! `storage` 负责持久化；这些职责分别放在独立模块中。

pub mod a2a;
pub mod adapters;
pub mod cli;
pub mod config;
pub mod runtime;
pub mod skills;
pub mod storage;
