use std::{collections::HashMap, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;

use super::{
    AdapterContext, AgentEvent, AgentSession, Interaction, InteractionReply, RunOutcome, canonical,
    process::JsonlProcess,
};

pub struct CodexSession {
    process: JsonlProcess,
    native_id: String,
    cancel_seconds: u64,
}

impl CodexSession {
    pub async fn start(context: &AdapterContext, native: Option<&str>) -> Result<Self> {
        let mut args = vec!["app-server".into(), "--listen".into(), "stdio://".into()];
        args.extend(context.config.agent.args.clone());
        let mut process = JsonlProcess::spawn(context, &args)?;
        process
            .request(
                "initialize",
                json!({"clientInfo":{"name":"a2alinker","version":env!("CARGO_PKG_VERSION")}}),
                false,
            )
            .await?;
        process
            .send(json!({"method":"initialized","params":{}}))
            .await?;
        if let Some(root) = &context.skills.root {
            process
                .request(
                    "skills/extraRoots/set",
                    json!({"extraRoots":[canonical(root)?]}),
                    false,
                )
                .await?;
        }
        let list = process
            .request(
                "skills/list",
                json!({"cwds":[context.config.agent.workspace],"forceReload":true}),
                false,
            )
            .await?;
        let expected: Vec<_> = context
            .skills
            .skills
            .iter()
            .map(|s| canonical(&s.path))
            .collect::<Result<_>>()?;
        let mut loaded = vec![];
        for entry in list["data"]
            .as_array()
            .context("Codex skills/list missing data")?
        {
            for skill in entry["skills"]
                .as_array()
                .context("Codex skill list missing skills")?
            {
                let path = skill["path"].as_str().context("Codex skill missing path")?;
                let selected = std::path::Path::new(path)
                    .canonicalize()
                    .ok()
                    .is_some_and(|p| expected.contains(&p));
                if selected {
                    loaded.push(path.to_string());
                }
                let enabled = selected || context.config.skills.inherit_user;
                if skill["enabled"].as_bool() != Some(enabled) {
                    process
                        .request(
                            "skills/config/write",
                            json!({"path":path,"enabled":enabled}),
                            false,
                        )
                        .await?;
                }
            }
        }
        ensure!(
            loaded.len() == expected.len(),
            "Codex did not discover every selected skill"
        );
        if !expected.is_empty() || !context.config.skills.inherit_user {
            let refreshed = process
                .request(
                    "skills/list",
                    json!({"cwds":[context.config.agent.workspace],"forceReload":true}),
                    false,
                )
                .await?;
            let mut active = vec![];
            for entry in refreshed["data"]
                .as_array()
                .context("Codex skills/list missing data")?
            {
                for skill in entry["skills"]
                    .as_array()
                    .context("Codex skill list missing skills")?
                {
                    if skill["enabled"] == true {
                        let path = skill["path"].as_str().context("Codex skill missing path")?;
                        active.push(canonical(std::path::Path::new(path))?);
                    }
                }
            }
            ensure!(
                expected.iter().all(|p| active.contains(p)),
                "Codex did not enable every selected skill"
            );
            ensure!(
                context.config.skills.inherit_user || active.iter().all(|p| expected.contains(p)),
                "Codex did not disable inherited skills"
            );
        }
        let mut params = json!({"cwd":canonical(&context.config.agent.workspace)?,
            "sandbox":context.config.agent.sandbox,"approvalPolicy":context.config.agent.approval_policy});
        if let Some(model) = &context.config.agent.model {
            params["model"] = json!(model);
        }
        if let Some(provider) = &context.config.agent.provider {
            params["modelProvider"] = json!(provider);
        }
        let method = if let Some(id) = native {
            params["threadId"] = json!(id);
            "thread/resume"
        } else {
            "thread/start"
        };
        let response = process.request(method, params, false).await?;
        let native_id = response["thread"]["id"]
            .as_str()
            .context("Codex did not return a thread id")?
            .to_string();
        Ok(Self {
            process,
            native_id,
            cancel_seconds: context.config.runtime.cancel_timeout_seconds,
        })
    }
}

#[async_trait]
impl AgentSession for CodexSession {
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
        let started = self
            .process
            .request(
                "turn/start",
                json!({"threadId":self.native_id,
            "input":[{"type":"text","text":prompt}]}),
                false,
            )
            .await?;
        let turn_id = started["turn"]["id"]
            .as_str()
            .context("Codex turn/start missing turn id")?
            .to_string();
        let mut cancelling = false;
        let mut deadline = Instant::now() + Duration::from_secs(86400 * 365);
        let mut pending = HashMap::<String, Value>::new();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled(), if !cancelling => {
                    cancelling = true;
                    deadline = Instant::now() + Duration::from_secs(self.cancel_seconds);
                    self.process.send(json!({"id":"linker-cancel","method":"turn/interrupt",
                        "params":{"threadId":self.native_id,"turnId":turn_id}})).await?;
                }
                _ = tokio::time::sleep_until(deadline), if cancelling => {
                    self.process.terminate(); return Ok(RunOutcome::Canceled);
                }
                Some(reply) = replies.recv(), if !pending.is_empty() && !cancelling => {
                    let native_id = pending.remove(&reply.id).context("unknown interaction reply id")?;
                    self.process.send(json!({"id":native_id,"result":reply.response})).await?;
                }
                record = self.process.next() => {
                    let record = record?;
                    if record.get("method").is_none() { continue; }
                    let method = record["method"].as_str().unwrap_or("");
                    let params = &record["params"];
                    if let Some(thread) = params["threadId"].as_str() && thread != self.native_id { continue; }
                    if let Some(turn) = params["turnId"].as_str() && turn != turn_id { continue; }
                    if let Some(id) = record.get("id") {
                        match method {
                            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" | "item/tool/requestUserInput" => {
                                ensure!(pending.is_empty(), "parallel Codex interactions are not supported");
                                let interaction_id = uuid::Uuid::new_v4().to_string();
                                pending.insert(interaction_id.clone(), id.clone());
                                events.send(AgentEvent::Interaction(Interaction { id:interaction_id,
                                    kind:if method.ends_with("requestApproval") { "approval" } else { "input" }.into(),
                                    method:method.into(), params:params.clone() })).await?;
                            }
                            // Unsupported server requests must get an explicit error instead of hanging forever.
                            _ => {
                                self.process.send(json!({"id":id,"error":{"code":-32601,"message":"Server request is not supported by a2alinker"}})).await?;
                                bail!("unsupported Codex server request: {method}");
                            }
                        }
                        continue;
                    }
                    match method {
                        "item/agentMessage/delta" => {
                            let item_id = params["itemId"].as_str().context("Codex delta missing item id")?;
                            let text = params["delta"].as_str().context("Codex delta missing text")?;
                            events.send(AgentEvent::OutputDelta { item_id:item_id.into(), text:text.into() }).await?;
                        }
                        "item/completed" if params["item"]["type"] == "agentMessage" => {
                            let item = &params["item"];
                            events.send(AgentEvent::OutputComplete { item_id:item["id"].as_str().context("Codex message missing id")?.into(),
                                text:item["text"].as_str().unwrap_or("").into() }).await?;
                        }
                        "item/started" if params["item"]["type"] == "commandExecution" => {
                            events.send(AgentEvent::Progress("Running command".into())).await?;
                        }
                        "turn/completed" if params["turn"]["id"] == turn_id => {
                            return match params["turn"]["status"].as_str() {
                                Some("completed") if !cancelling => Ok(RunOutcome::Completed),
                                Some("interrupted") => Ok(RunOutcome::Canceled),
                                _ if cancelling => Ok(RunOutcome::Canceled),
                                _ => Err(anyhow::anyhow!("Codex turn failed: {}", params["turn"]["error"]["message"].as_str().unwrap_or("unknown error"))),
                            };
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
