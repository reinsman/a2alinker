//! pi 的 RPC 适配器：会话以 JSONL 文件保存，事件通过标准输入/输出传输。

use std::time::Duration;

use anyhow::{Result, bail, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;

use super::{
    AdapterContext, AgentEvent, AgentSession, Interaction, InteractionReply, RunOutcome,
    process::JsonlProcess,
};

pub struct PiSession {
    process: JsonlProcess,
    native_id: String,
    cancel_seconds: u64,
    message_count: u64,
}

impl PiSession {
    /// 创建/恢复会话并验证技能；此方法只握手，不向模型发送 prompt。
    pub async fn start(
        context: &AdapterContext,
        context_id: &str,
        native: Option<&str>,
    ) -> Result<Self> {
        let session_dir = context.config.storage.directory.join("sessions");
        std::fs::create_dir_all(&session_dir)?;
        // 外部 contextId 先哈希成文件名，避免客户端通过 ../ 等内容控制会话文件路径。
        let path = native.map(std::path::PathBuf::from).unwrap_or_else(|| {
            session_dir.join(format!("{:x}.jsonl", Sha256::digest(context_id.as_bytes())))
        });
        if native.is_some() {
            ensure!(
                path.is_file(),
                "Pi session file is missing; refusing to silently reset context"
            );
        }
        let mut args: Vec<String> = vec![
            "--mode".into(),
            "rpc".into(),
            "--session".into(),
            path.to_string_lossy().into(),
        ];
        if !context.config.skills.inherit_user {
            args.push("--no-skills".into());
        }
        if !context.config.agent.allow_extensions {
            args.push("--no-extensions".into());
        }
        for skill in &context.skills.skills {
            args.extend(["--skill".into(), skill.path.to_string_lossy().into()]);
        }
        if let Some(model) = &context.config.agent.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(provider) = &context.config.agent.provider {
            args.extend(["--provider".into(), provider.clone()]);
        }
        args.extend(context.config.agent.args.clone());
        let mut process = JsonlProcess::spawn(context, &args)?;
        let state = process.request("get_state", json!({}), true).await?;
        if !context.skills.skills.is_empty() {
            let commands = process.request("get_commands", json!({}), true).await?;
            for skill in &context.skills.skills {
                ensure!(
                    commands["commands"]
                        .as_array()
                        .is_some_and(|commands| commands
                            .iter()
                            .any(|command| command["source"] == "skill"
                                && command["name"] == format!("skill:{}", skill.name))),
                    "Pi did not load selected skill {}",
                    skill.name
                );
            }
        }
        let native_id = state
            .get("sessionFile")
            .and_then(Value::as_str)
            .unwrap_or(path.to_str().unwrap())
            .to_string();
        Ok(Self {
            process,
            native_id,
            cancel_seconds: context.config.runtime.cancel_timeout_seconds,
            message_count: 0,
        })
    }
}

#[async_trait]
impl AgentSession for PiSession {
    fn native_id(&self) -> &str {
        &self.native_id
    }
    fn is_alive(&mut self) -> bool {
        self.process.is_alive()
    }

    async fn run(
        &mut self,
        prompt: &str,
        events: mpsc::Sender<AgentEvent>,
        mut replies: mpsc::Receiver<InteractionReply>,
        cancel: CancellationToken,
    ) -> Result<RunOutcome> {
        // prompt 的响应只表示命令被接受；必须继续消费事件才能知道任务何时结束。
        self.process
            .request("prompt", json!({"message": prompt}), true)
            .await?;
        let mut cancelling = false;
        let mut deadline = Instant::now() + Duration::from_secs(86400 * 365);
        let mut last_error = None;
        let mut item_id = String::new();
        let mut pending: Option<String> = None;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled(), if !cancelling => {
                    cancelling = true;
                    deadline = Instant::now() + Duration::from_secs(self.cancel_seconds);
                    self.process.send(json!({"type":"clear_queue"})).await?;
                    self.process.send(json!({"type":"abort"})).await?;
                }
                _ = tokio::time::sleep_until(deadline), if cancelling => {
                    self.process.terminate();
                    return Ok(RunOutcome::Canceled);
                }
                Some(reply) = replies.recv(), if pending.is_some() && !cancelling => {
                    ensure!(pending.as_deref() == Some(&reply.id), "interaction reply id mismatch");
                    let mut response = reply.response.as_object().cloned().unwrap_or_default();
                    response.insert("type".into(), json!("extension_ui_response"));
                    response.insert("id".into(), json!(reply.id));
                    self.process.send(Value::Object(response)).await?;
                    pending = None;
                }
                record = self.process.next() => {
                    let record = record?;
                    match record.get("type").and_then(Value::as_str).unwrap_or("") {
                        "message_start" if record["message"]["role"] == "assistant" => {
                            self.message_count += 1;
                            item_id = format!("pi-{}", self.message_count);
                        }
                        "message_update" if record["assistantMessageEvent"]["type"] == "text_delta" => {
                            if let Some(text) = record["assistantMessageEvent"]["delta"].as_str() {
                                events.send(AgentEvent::OutputDelta { item_id: item_id.clone(), text: text.into() }).await?;
                            }
                        }
                        "message_end" if record["message"]["role"] == "assistant" => {
                            let message = &record["message"];
                            let text = message["content"].as_array().map(|parts| parts.iter().filter(|p| p["type"] == "text")
                                .filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_default();
                            last_error = if message["stopReason"] == "error" { Some(message["errorMessage"].as_str().unwrap_or("Pi model error").to_string()) } else { None };
                            events.send(AgentEvent::OutputComplete { item_id: item_id.clone(), text }).await?;
                        }
                        "tool_execution_start" => {
                            events.send(AgentEvent::Progress(format!("Running tool: {}", record["toolName"].as_str().unwrap_or("tool")))).await?;
                        }
                        // 等待包括自动重试在内的整轮工作结束，不能把中间 agent_end 当终态。
                        "agent_settled" => {
                            if cancelling || record["aborted"] == true { return Ok(RunOutcome::Canceled); }
                            if let Some(error) = last_error { bail!("{error}"); }
                            return Ok(RunOutcome::Completed);
                        }
                        // 兼容旧版：没有 settled 事件且不带 willRetry 标记时，agent_end 即结束。
                        "agent_end" if record.get("willRetry").is_none() => {
                            if cancelling { return Ok(RunOutcome::Canceled); }
                            if let Some(error) = last_error { bail!("{error}"); }
                            return Ok(RunOutcome::Completed);
                        }
                        "extension_ui_request" => {
                            match record["method"].as_str() {
                                Some("confirm" | "select" | "input" | "editor") => {
                                    let id = record["id"].as_str().ok_or_else(|| anyhow::anyhow!("Pi interaction missing id"))?.to_string();
                                    ensure!(pending.is_none(), "parallel Pi interactions are not supported");
                                    pending = Some(id.clone());
                                    events.send(AgentEvent::Interaction(Interaction { id, kind: "input".into(), method: "extension_ui_request".into(), params: record })).await?;
                                }
                                Some("notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text" | "setEditorText") => {}
                                _ => bail!("unsupported Pi extension UI method"),
                            }
                        }
                        "response" if record["success"] == false => bail!("Pi command failed: {}", record["error"]),
                        _ => {}
                    }
                }
            }
        }
    }
}
