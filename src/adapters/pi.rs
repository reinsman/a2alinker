//! pi 的 RPC 适配器：会话以 JSONL 文件保存，事件通过标准输入/输出传输。

use std::{collections::HashSet, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;

use super::{
    AdapterContext, AgentEvent, AgentSession, Interaction, InteractionReply, RunOutcome, ToolEvent,
    ToolPhase, process::JsonlProcess,
};

pub struct PiSession {
    process: JsonlProcess,
    native_id: String,
    cancel_seconds: u64,
    message_count: u64,
}

impl PiSession {
    /// 创建/恢复原生会话；新会话的路径由 Pi 决定，此处只握手，不请求模型。
    pub async fn start(context: &AdapterContext, native: Option<&str>) -> Result<Self> {
        let mut args: Vec<String> = vec!["--mode".into(), "rpc".into()];
        if let Some(path) = native {
            // 恢复数据库中的原路径，包括旧版本保存在 storage/sessions 中的文件。
            ensure!(
                std::path::Path::new(path).is_file(),
                "Pi session file is missing; refusing to silently reset context"
            );
            args.extend(["--session".into(), path.into()]);
        }
        if !context.options.agent.allow_extensions {
            args.push("--no-extensions".into());
        }
        for directory in &context.options.skills.directories {
            args.extend(["--skill".into(), directory.to_string_lossy().into()]);
        }
        if let Some(model) = &context.options.agent.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(provider) = &context.options.agent.provider {
            args.extend(["--provider".into(), provider.clone()]);
        }
        args.extend(context.options.agent.args.clone());
        let mut process = JsonlProcess::spawn(context, &args)?;
        let state = process.request("get_state", json!({}), true).await?;
        // 只记录选择的模型标识，不输出可能含端点、请求头等信息的完整 model 对象。
        if let Some(model) = state.get("model").filter(|value| value.is_object()) {
            tracing::info!(
                provider = model["provider"].as_str().unwrap_or("unknown"),
                model = model["id"].as_str().unwrap_or("unknown"),
                "pi model selected"
            );
        }
        let native_id = state
            .get("sessionFile")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .context("Pi did not return a persistent sessionFile")?
            .to_owned();
        Ok(Self {
            process,
            native_id,
            cancel_seconds: context.options.runtime.cancel_timeout_seconds,
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
        let mut text_started = false;
        let mut completed_reasoning = HashSet::<usize>::new();
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
                            text_started = false;
                            completed_reasoning.clear();
                        }
                        "message_update" if record["assistantMessageEvent"]["type"] == "text_delta" => {
                            if let Some(text) = record["assistantMessageEvent"]["delta"].as_str() {
                                text_started = true;
                                events.send(AgentEvent::OutputDelta { item_id: item_id.clone(), text: text.into() }).await?;
                            }
                        }
                        "message_update" if matches!(record["assistantMessageEvent"]["type"].as_str(), Some("thinking_delta" | "thinking_end")) => {
                            let update = &record["assistantMessageEvent"];
                            let index = update["contentIndex"].as_u64().unwrap_or(0) as usize;
                            let complete = update["type"] == "thinking_end";
                            // 只取公开文本；不转发 thinkingSignature、加密内容或完整原生报文。
                            let text = update[if complete { "content" } else { "delta" }].as_str();
                            if let Some(text) = text {
                                events.send(AgentEvent::Reasoning {
                                    item_id: format!("{item_id}-{index}"), text: text.into(),
                                    append: !complete, last_chunk: complete,
                                }).await?;
                                if complete { completed_reasoning.insert(index); }
                            }
                        }
                        "message_end" if record["message"]["role"] == "assistant" => {
                            let message = &record["message"];
                            let text = message["content"].as_array().map(|parts| parts.iter().filter(|p| p["type"] == "text")
                                .filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_default();
                            last_error = if message["stopReason"] == "error" { Some(message["errorMessage"].as_str().unwrap_or("Pi model error").to_string()) } else { None };
                            if let Some(parts) = message["content"].as_array() {
                                for (index, part) in parts.iter().enumerate() {
                                    // 兼容未发送 thinking_end 的版本，以消息中的完整文本补齐。
                                    if part["type"] == "thinking" && !completed_reasoning.contains(&index)
                                        && let Some(text) = part["thinking"].as_str()
                                    {
                                        events.send(AgentEvent::Reasoning {
                                            item_id: format!("{item_id}-{index}"), text: text.into(),
                                            append: false, last_chunk: true,
                                        }).await?;
                                    }
                                }
                            }
                            // 只有工具调用或思考的消息，不再生成空的回答 Artifact。
                            if text_started || !text.is_empty() {
                                events.send(AgentEvent::OutputComplete { item_id: item_id.clone(), text }).await?;
                            }
                        }
                        "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
                            let phase = match record["type"].as_str() {
                                Some("tool_execution_start") => ToolPhase::Start,
                                Some("tool_execution_update") => ToolPhase::Update,
                                _ => ToolPhase::End,
                            };
                            events.send(AgentEvent::Tool(ToolEvent {
                                call_id: record["toolCallId"].as_str().context("Pi tool event missing toolCallId")?.into(),
                                name: record["toolName"].as_str().unwrap_or("tool").into(),
                                phase,
                                arguments: record.get("args").cloned(),
                                result: record.get(if matches!(phase, ToolPhase::Update) { "partialResult" } else { "result" }).cloned(),
                                is_error: record["isError"].as_bool(),
                            })).await?;
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
