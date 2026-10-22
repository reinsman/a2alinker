//! 适配器的公共接口：runtime 只认识这些类型，不依赖 pi 或 Codex 的具体报文。

mod codex;
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

use crate::{
    config::{AgentKind, Config},
    skills::{SkillSet, copy_directory},
};

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

/// 原生协议统一成四类事件；完整文本与增量文本分开，便于正确更新产物。
#[derive(Debug)]
pub enum AgentEvent {
    OutputDelta { item_id: String, text: String },
    OutputComplete { item_id: String, text: String },
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
    pub config: Arc<Config>,
    pub skills: SkillSet,
    pub home: PathBuf,
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
    context_id: &str,
    native_id: Option<&str>,
) -> Result<Box<dyn AgentSession>> {
    match context.config.agent.kind {
        AgentKind::Pi => Ok(Box::new(
            pi::PiSession::start(context, context_id, native_id).await?,
        )),
        AgentKind::Codex => Ok(Box::new(
            codex::CodexSession::start(context, native_id).await?,
        )),
    }
}

/// 建立实例私有的 agent 配置目录；复制所需文件，不修改用户原始配置。
pub fn prepare_home(config: &Config, skill_digest: &str) -> Result<PathBuf> {
    let home = config
        .storage
        .directory
        .join("home")
        .join(config.agent.kind.as_str())
        .join(config.fingerprint(skill_digest)?);
    std::fs::create_dir_all(&home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
    }
    if let Some(source) = config.effective_native_config()? {
        if source.is_dir() {
            ensure!(
                !home.canonicalize()?.starts_with(source.canonicalize()?),
                "runtime storage must be outside the native configuration directory"
            );
            copy_directory(&source, &home)?;
        } else {
            let filename = match config.agent.kind {
                AgentKind::Pi => "settings.json",
                AgentKind::Codex => "config.toml",
            };
            std::fs::copy(&source, home.join(filename)).context("copy native configuration")?;
        }
    }
    // settings.json 只保存模型选择；自定义 provider 的端点、API 和 key 在 models.json。
    if let Some(source) = config.native_model_config()? {
        std::fs::copy(source, home.join("models.json")).context("copy pi model configuration")?;
    }
    if config.agent.inherit_auth && !home.join("auth.json").exists() {
        // 配置与登录文件使用同一套来源规则，避免指定 PI_CODING_AGENT_DIR 后混用其他账号。
        let source = config.user_agent_directory()?;
        if let Some(source) = source.map(|p| p.join("auth.json")).filter(|p| p.is_file()) {
            std::fs::copy(source, home.join("auth.json"))?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["auth.json", "config.toml", "settings.json", "models.json"] {
            let path = home.join(name);
            if path.exists() {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
    }
    Ok(home)
}

/// 只运行 --version，附带启动超时；真实协议兼容性由后续 create_session 验证。
pub async fn probe(config: &Config) -> Result<String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(config.runtime.startup_timeout_seconds),
        tokio::process::Command::new(config.executable())
            .arg("--version")
            .envs(&config.agent.env)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("agent version probe timed out")?
    .with_context(|| {
        format!(
            "cannot start agent executable '{}'; check agent.executable, agent.env.PATH and the script interpreter (nvm setup: docs/agent-launch.md)",
            config.executable().display()
        )
    })?;
    ensure!(
        output.status.success(),
        "agent --version exited unsuccessfully"
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
