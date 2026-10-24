//! Codex app-server 适配器：一个 thread 对应原生会话，一个 turn 对应一次任务。

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
    /// 初始化原生协议、传入附加技能目录，再创建或恢复 thread；不改写技能启用设置。
    pub async fn start(context: &AdapterContext, native: Option<&str>) -> Result<Self> {
        let mut args = vec!["app-server".into(), "--listen".into(), "stdio://".into()];
        args.extend(context.options.agent.args.clone());
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
        if !context.options.skills.directories.is_empty() {
            process
                .request(
                    "skills/extraRoots/set",
                    json!({"extraRoots":context.options.skills.directories}),
                    false,
                )
                .await?;
        }
        let mut params = json!({"cwd":canonical(&context.options.agent.workspace)?});
        if let Some(sandbox) = &context.options.agent.sandbox {
            params["sandbox"] = json!(sandbox);
        }
        if let Some(policy) = &context.options.agent.approval_policy {
            params["approvalPolicy"] = json!(policy);
        }
        if let Some(model) = &context.options.agent.model {
            params["model"] = json!(model);
        }
        if let Some(provider) = &context.options.agent.provider {
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
            cancel_seconds: context.options.runtime.cancel_timeout_seconds,
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
        // 客户端使用新的 interactionId；此表保留它与原生 JSON-RPC 请求 ID 的对应关系。
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
                            // 不支持的原生请求必须明确返回错误，避免 agent 一直等待响应。
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
