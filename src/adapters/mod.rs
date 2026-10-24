//! 适配器的公共接口：runtime 只认识这些类型，不依赖 pi 或 Codex 的具体报文。

mod codex;
pub(crate) mod execution;
mod launch;
mod pi;
pub mod process;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use execution::{ToolEvent, ToolPhase};

use crate::options::{AgentKind, Options};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Interaction {
    pub id: String,
    pub kind: String,
    pub method: String,
    pub params: Value,
}

#[derive(Debug)]
pub struct InteractionReply {
    pub id: String,
    pub response: Value,
}

/// 原生协议统一成回答、公开思考、工具过程、普通进度和交互事件。
/// 完整文本与增量文本分开，便于正确更新产物。
#[derive(Debug)]
pub enum AgentEvent {
    OutputDelta {
        item_id: String,
        text: String,
    },
    OutputComplete {
        item_id: String,
        text: String,
    },
    Reasoning {
        item_id: String,
        text: String,
        append: bool,
        last_chunk: bool,
    },
    Tool(ToolEvent),
    Progress(String),
    Interaction(Interaction),
}

#[derive(Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Canceled,
}

#[derive(Clone)]
pub struct AdapterContext {
    pub options: Arc<Options>,
}

/// trait 类似其他语言的接口；每种 agent 实现相同的会话操作。
/// async_trait 让异步方法可通过 dyn AgentSession 调用；Send 允许所有权跨线程转移。
#[async_trait]
pub trait AgentSession: Send {
    fn native_id(&self) -> &str;
    fn is_alive(&mut self) -> bool;
    async fn run(
        &mut self,
        prompt: &str,
        events: mpsc::Sender<AgentEvent>,
        replies: mpsc::Receiver<InteractionReply>,
        cancel: CancellationToken,
    ) -> Result<RunOutcome>;
}

/// native_id 为 Some 时恢复已有会话，否则创建新会话；返回值隐藏具体适配器类型。
pub async fn create_session(
    context: &AdapterContext,
    native_id: Option<&str>,
) -> Result<Box<dyn AgentSession>> {
    match context.options.agent.kind {
        AgentKind::Pi => Ok(Box::new(pi::PiSession::start(context, native_id).await?)),
        AgentKind::Codex => Ok(Box::new(
            codex::CodexSession::start(context, native_id).await?,
        )),
    }
}

/// 只运行 --version，附带启动超时；真实协议兼容性由后续 create_session 验证。
pub async fn probe(options: &Options) -> Result<String> {
    let mut command = launch::command(options)?;
    let child = command.arg("--version").spawn().with_context(|| {
        format!(
            "cannot start agent executable '{}'; check --prepare, --executable and PATH",
            options.executable().display()
        )
    })?;
    let _group = launch::ProcessGroup::new(child.id());
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(options.runtime.startup_timeout_seconds),
        child.wait_with_output(),
    )
    .await
    .context("agent version probe timed out")?
    .with_context(|| {
        format!(
                "cannot start agent executable '{}'; check --executable, PATH and the script interpreter (see README.md)",
            options.executable().display()
        )
    })?;
    ensure!(
        output.status.success(),
        "agent --version exited unsuccessfully: {} (check --prepare and --executable)",
        output.status
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// 按原生交互类型检查字段和取值，拒绝把任意 JSON 直接作为审批结果发送给 agent。
pub fn validate_reply(interaction: &Interaction, response: &Value) -> Result<()> {
    match interaction.method.as_str() {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            ensure!(
                matches!(
                    response.get("decision").and_then(Value::as_str),
                    Some("accept" | "decline" | "cancel")
                ),
                "approval response requires decision: accept, decline or cancel"
            );
            ensure!(
                response.as_object().is_some_and(|o| o.len() == 1),
                "unexpected approval response fields"
            );
        }
        "item/tool/requestUserInput" => {
            let answers = response
                .get("answers")
                .and_then(Value::as_object)
                .context("response requires an answers object")?;
            let questions = interaction
                .params
                .get("questions")
                .and_then(Value::as_array)
                .context("invalid user input interaction")?;
            ensure!(
                answers.len() == questions.len(),
                "answer every requested question"
            );
            for question in questions {
                let id = question
                    .get("id")
                    .and_then(Value::as_str)
                    .context("question is missing id")?;
                let a = answers
                    .get(id)
                    .and_then(|a| a.get("answers"))
                    .and_then(Value::as_array)
                    .context("each answer requires an answers array")?;
                ensure!(
                    !a.is_empty() && a.iter().all(Value::is_string),
                    "answers must be nonempty string arrays"
                );
            }
        }
        "extension_ui_request" => {
            match interaction.params.get("method").and_then(Value::as_str) {
                Some("confirm") => ensure!(
                    response.get("confirmed").is_some_and(Value::is_boolean),
                    "confirm requires confirmed: boolean"
                ),
                Some("select" | "input" | "editor") => ensure!(
                    response.get("value").is_some_and(Value::is_string)
                        || response.get("cancelled").and_then(Value::as_bool) == Some(true),
                    "input requires value: string or cancelled: true"
                ),
                _ => anyhow::bail!("unsupported Pi interaction"),
            }
            ensure!(
                response.as_object().is_some_and(|o| o.keys().all(|k| [
                    "confirmed",
                    "value",
                    "cancelled"
                ]
                .contains(&k.as_str()))),
                "unexpected Pi response fields"
            );
        }
        _ => anyhow::bail!("unsupported interaction method"),
    }
    Ok(())
}

pub fn canonical(path: &Path) -> Result<PathBuf> {
    path.canonicalize().context("canonicalize adapter path")
}
