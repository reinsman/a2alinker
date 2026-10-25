//! 任务调度中心：把 A2A 请求转成排队任务，把 agent 事件转成持久化状态和产物。
//! 每个实例只有一个 worker 执行任务，HTTP 查询和取消仍可并发处理。

use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use ra2a::types::{
    Artifact, Message, Part, PartContent, Role, StreamResponse, Task, TaskArtifactUpdateEvent,
    TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Mutex, broadcast, mpsc},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use crate::{
    adapters::{
        self, AdapterContext, AgentEvent, AgentSession, Interaction, InteractionReply, RunOutcome,
        execution::{EXECUTION_EXTENSION, tool_data, tool_event},
    },
    options::Options,
    storage::{Store, metadata},
};

pub const INTERACTION_EXTENSION: &str = "urn:a2alinker:interaction:v1";

#[derive(Debug)]
pub struct ApiError {
    pub code: i32,
    pub message: String,
}
impl ApiError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            code: -32004,
            message: message.into(),
        }
    }
    pub fn not_found() -> Self {
        Self {
            code: -32001,
            message: "Task not found".into(),
        }
    }
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for ApiError {}
impl From<anyhow::Error> for ApiError {
    fn from(_error: anyhow::Error) -> Self {
        // 服务端记录详细错误，客户端只得到通用信息，避免暴露内部路径或配置。
        #[cfg(feature = "logging")]
        tracing::error!(error = %_error, "gateway operation failed");
        Self {
            code: -32603,
            message: "Internal gateway error; see server logs".into(),
        }
    }
}
pub type ApiResult<T> = std::result::Result<T, ApiError>;

struct Work {
    task_id: String,
    prompt: String,
}
struct Control {
    cancel: CancellationToken,
    // HTTP 请求将审批/输入回复送入此通道。
    replies: mpsc::Sender<InteractionReply>,
}
struct QueuedControl {
    cancel: CancellationToken,
    // 接收端只有一份：从排队表移交给 worker，最终由本轮原生会话读取。
    replies: mpsc::Receiver<InteractionReply>,
}
struct SessionEntry {
    // dyn 表示运行时选择具体实现；Box 拥有该会话，释放时会连带清理子进程。
    session: Box<dyn AgentSession>,
    last_used_at: Instant,
}

/// Arc 允许路由、请求和后台任务共同持有网关，不会复制整个网关及数据库。
pub struct Gateway {
    pub options: Arc<Options>,
    pub store: Arc<Store>,
    queue: mpsc::Sender<Work>,
    controls: Mutex<HashMap<String, Control>>,
    queued: Mutex<HashMap<String, QueuedControl>>,
    // () 不存放业务数据；拿到锁即进入任务修改临界区，避免读改写相互覆盖。
    mutation: Mutex<()>,
    // 广播只负责唤醒等待者；完整事件在数据库中，通知丢失不等于事件丢失。
    changes: broadcast::Sender<()>,
    shutdown: CancellationToken,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Gateway {
    /// 初始化存储、技能与原生协议，恢复历史状态，然后启动唯一的执行 worker。
    pub async fn open(mut options: Options) -> Result<Arc<Self>> {
        options.resolve()?;
        options.agent.workspace = options
            .agent
            .workspace
            .canonicalize()
            .context("canonicalize workspace")?;
        options.validate()?;
        let storage = options.instance_directory()?;
        // 缺少新实例描述时提前退出，不创建一个空目录影响下次的新旧判断。
        if !storage.try_exists()? && options.identity.description.is_empty() {
            #[cfg(feature = "logging")]
            tracing::info!(event = "instance.new", name = options.identity.name, storage = %storage.display(), "这是新实例，需要提供 --desc");
            anyhow::bail!("新实例 {} 必须提供 --desc", options.identity.name);
        }
        let store = Arc::new(Store::open(&storage).await?);
        store
            .prepare_instance(options.agent.kind, &mut options.identity)
            .await?;
        #[cfg(feature = "logging")]
        tracing::info!(event = "agent.config", agent = options.agent.kind.as_str(),
            workspace = %options.agent.workspace.display(), storage_root = %options.storage.directory.display(), storage = %storage.display(),
            skill_directory_count = options.skills.directories.len(),
            "直接使用原生配置、认证、技能及会话存储；单个 worker 串行执行任务");
        let options = Arc::new(options);
        let adapter = AdapterContext {
            options: Arc::clone(&options),
        };
        // 正式对外报告就绪前，验证原生协议和选中技能；此时不请求模型推理。
        let preflight_result = tokio::time::timeout(
            Duration::from_secs(options.runtime.startup_timeout_seconds),
            adapters::create_session(&adapter, None),
        )
        .await
        .context("agent preflight timed out")?;
        // 外层检查握手是否超时，内层检查原生会话是否创建成功。
        let preflight_session = preflight_result?;
        drop(preflight_session);
        #[cfg(feature = "logging")]
        tracing::info!(
            event = "gateway.preflight",
            "原生协议握手与技能预检通过；预检不调用模型，模型账号权限尚未验证"
        );
        let (queue, receiver) = mpsc::channel(options.runtime.queue_capacity);
        let (changes, _) = broadcast::channel(256);
        let gateway = Arc::new(Self {
            options,
            store,
            queue,
            controls: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
            changes,
            shutdown: CancellationToken::new(),
            worker: Mutex::new(None),
        });
        // 重启前的任务可能已修改文件；标记失败，不自动重跑有副作用的工作。
        for task in gateway.store.tasks().await? {
            if !task.status.state.is_terminal() {
                gateway.finish(task.id.as_str(), TaskState::Failed, Some("Gateway restarted; previous execution was interrupted and was not replayed".into())).await?;
            }
        }
        // worker 使用 Weak 弱引用，不让空闲后台任务一直维持网关的强引用计数。
        let worker_task = tokio::spawn(worker(
            Arc::downgrade(&gateway),
            receiver,
            adapter,
            gateway.shutdown.clone(),
        ));
        *gateway.worker.lock().await = Some(worker_task);
        Ok(gateway)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changes.subscribe()
    }

    pub async fn task(&self, id: &str) -> ApiResult<Task> {
        self.store.task(id).await?.ok_or_else(ApiError::not_found)
    }

    /// 校验和去重后创建任务；带 taskId 的消息会转交给交互回复处理。
    pub async fn submit(&self, message: Message) -> ApiResult<Task> {
        if self.shutdown.is_cancelled() {
            return Err(ApiError::unsupported("Gateway is shutting down"));
        }
        if message.role != Role::User
            || message.id.as_str().is_empty()
            || message.id.as_str().len() > 256
            || message.parts.is_empty()
        {
            return Err(ApiError::invalid(
                "messageId, ROLE_USER and nonempty parts are required",
            ));
        }
        if message
            .context_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 256)
        {
            return Err(ApiError::invalid("contextId must contain 1..=256 bytes"));
        }
        // 摘要包含整条 A2A Message，用于判断相同 messageId 是否是原请求的重试。
        let message_bytes = serde_json::to_vec(&message).map_err(anyhow::Error::from)?;
        let digest = format!("{:x}", Sha256::digest(message_bytes));
        // guard 离开作用域时自动释放锁；去重和创建任务必须处于同一临界区。
        let _mutation_guard = self.mutation.lock().await;
        if let Some((stored_digest, task_id)) = self.store.duplicate(message.id.as_str()).await? {
            if stored_digest != digest {
                #[cfg(feature = "logging")]
                tracing::warn!(
                    event = "message.conflict",
                    message_id = message.id.as_str(),
                    task_id,
                    "拒绝消息：相同 messageId 对应不同内容；新消息需要新的 ID"
                );
                return Err(ApiError::invalid(
                    "messageId was already used with different content",
                ));
            }
            let task = self.task(&task_id).await?;
            #[cfg(feature = "logging")]
            tracing::info!(
                event = "message.duplicate", message_id = message.id.as_str(), task_id,
                context_id = task.context_id.as_str(), state = %task.status.state,
                "命中消息去重：返回原任务，不会重新调用 agent；希望重新执行时请使用新的 messageId"
            );
            return Ok(task);
        }
        if let Some(task_id) = &message.task_id {
            return self.reply(task_id.as_str(), &message, &digest).await;
        }
        if message.parts.iter().any(|p| {
            !matches!(p.content, PartContent::Text(_))
                || p.media_type.as_deref().is_some_and(|m| m != "text/plain")
        }) {
            return Err(ApiError {
                code: -32005,
                message: "New tasks accept text/plain parts only".into(),
            });
        }
        let prompt = message
            .text_content()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ApiError::invalid("prompt must not be empty"))?;
        let context_id = match &message.context_id {
            Some(context_id) => context_id.clone(),
            None => uuid::Uuid::new_v4().to_string(),
        };
        if self.store.active_context(&context_id).await? {
            #[cfg(feature = "logging")]
            tracing::warn!(
                event = "context.busy",
                context_id,
                "同一 context 已有未结束任务，当前消息未入队"
            );
            return Err(ApiError::unsupported("Context already has an active task"));
        }
        // 先预留队列容量再落库，避免创建了一个没有位置入队的任务。
        let queue_sender = self.queue.clone();
        let permit = match queue_sender.try_reserve_owned() {
            Ok(permit) => permit,
            Err(_) => {
                #[cfg(feature = "logging")]
                tracing::warn!(
                    event = "task.queue_full",
                    capacity = self.options.runtime.queue_capacity,
                    "任务队列已满，当前消息未创建任务，请稍后重试"
                );
                return Err(ApiError::unsupported("Task queue is full; retry later"));
            }
        };
        let mut task = Task::new(uuid::Uuid::new_v4().to_string(), context_id);
        task.history.push(
            message
                .clone()
                .with_context_id(task.context_id.as_str())
                .with_task_id(task.id.clone()),
        );
        metadata(&mut task).insert(
            "a2alinker".into(),
            json!({"agent":self.options.agent.kind.as_str(),"runId":uuid::Uuid::new_v4().to_string()}),
        );
        self.store
            .insert_task(&task, message.id.as_str(), &digest)
            .await?;
        // Sender 留给 HTTP 回复入口，Receiver 的所有权交给 worker，再交给 agent。
        let (reply_sender, reply_receiver) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        self.controls.lock().await.insert(
            task.id.to_string(),
            Control {
                cancel: cancel.clone(),
                replies: reply_sender,
            },
        );
        self.queued.lock().await.insert(
            task.id.to_string(),
            QueuedControl {
                cancel,
                replies: reply_receiver,
            },
        );
        #[cfg(feature = "logging")]
        tracing::info!(
            event = "task.queued", message_id = message.id.as_str(), task_id = task.id.as_str(),
            context_id = task.context_id.as_str(), state = %task.status.state, prompt_bytes = prompt.len(),
            "Task 已持久化并准备入队：taskId 标识本次执行，contextId 标识可复用的会话"
        );
        permit.send(Work {
            task_id: task.id.to_string(),
            prompt,
        });
        let _ = self.changes.send(());
        Ok(task)
    }

    /// 只接受与当前 interactionId 匹配的结构化回复，普通聊天文本不会触发授权。
    async fn reply(&self, id: &str, message: &Message, digest: &str) -> ApiResult<Task> {
        let mut task = self.task(id).await?;
        if message
            .context_id
            .as_ref()
            .is_some_and(|ctx| ctx != task.context_id.as_str())
        {
            return Err(ApiError::invalid("taskId and contextId do not match"));
        }
        if !task.status.state.is_interrupted() {
            return Err(ApiError::invalid(
                "Existing task only accepts replies while awaiting input; use contextId for a new task",
            ));
        }
        let pending = task
            .metadata
            .as_ref()
            .and_then(|m| m.get("pendingInteraction"))
            .cloned()
            .ok_or_else(|| ApiError::unsupported("Task has no pending interaction"))?;
        let interaction: Interaction =
            serde_json::from_value(pending).map_err(anyhow::Error::from)?;
        if message.parts.len() != 1 {
            return Err(ApiError::invalid(
                "Interaction reply requires exactly one data part",
            ));
        }
        let PartContent::Data(data) = &message.parts[0].content else {
            return Err(ApiError::invalid("Interaction reply requires a data part"));
        };
        if data["interactionId"].as_str() != Some(&interaction.id) {
            return Err(ApiError::invalid("Interaction id mismatch"));
        }
        let response = data
            .get("response")
            .ok_or_else(|| ApiError::invalid("Interaction reply missing response"))?;
        adapters::validate_reply(&interaction, response)
            .map_err(|e| ApiError::invalid(e.to_string()))?;
        // submit() 已持有 mutation 锁；此处延续同一个临界区，并在发送前先保存回复。
        let controls = self.controls.lock().await;
        let control = controls
            .get(id)
            .ok_or_else(|| ApiError::unsupported("Task execution is no longer active"))?;
        if control.cancel.is_cancelled() {
            return Err(ApiError::unsupported("Task is being canceled"));
        }
        let permit = control
            .replies
            .try_reserve()
            .map_err(|_| ApiError::unsupported("A reply is already pending"))?;
        task.history
            .push(message.clone().with_context_id(task.context_id.as_str()));
        metadata(&mut task).remove("pendingInteraction");
        task.status = TaskStatus::working();
        let event = status_event(&task);
        self.store
            .record_reply(&task, message, digest, &event)
            .await?;
        #[cfg(feature = "logging")]
        tracing::info!(
            event = "interaction.reply", task_id = id, context_id = task.context_id.as_str(),
            message_id = message.id.as_str(), interaction_id = interaction.id, method = interaction.method,
            state = %task.status.state, "交互回复已校验并持久化，转交原生 agent 继续执行；不记录回复内容"
        );
        permit.send(InteractionReply {
            id: interaction.id,
            response: response.clone(),
        });
        let _ = self.changes.send(());
        Ok(task)
    }

    pub async fn wait(&self, id: &str, interrupted: bool) -> ApiResult<Task> {
        // 先订阅再查询，防止状态恰好在查询与订阅之间改变，导致漏掉唤醒。
        let mut changes = self.subscribe();
        loop {
            let task = self.task(id).await?;
            if task.status.state.is_terminal()
                || (interrupted && task.status.state.is_interrupted())
            {
                return Ok(task);
            }
            let _ = changes.recv().await;
        }
    }

    pub async fn cancel(&self, id: &str) -> ApiResult<Task> {
        {
            // 只在这个代码块内修改状态；退出代码块后先解锁，再等待任务结束。
            let _mutation_guard = self.mutation.lock().await;
            let task = self.task(id).await?;
            if task.status.state == TaskState::Canceled {
                return Ok(task);
            }
            if task.status.state.is_terminal() {
                #[cfg(feature = "logging")]
                tracing::warn!(event = "task.cancel_rejected", task_id = id, state = %task.status.state, "CancelTask 被拒绝：任务已进入终态");
                return Err(ApiError {
                    code: -32002,
                    message: "Task is already terminal".into(),
                });
            }
            if let Some(control) = self.controls.lock().await.get(id) {
                #[cfg(feature = "logging")]
                tracing::info!(event = "task.cancel_requested", task_id = id, context_id = task.context_id.as_str(), state = %task.status.state, "收到 CancelTask：通知执行器中断任务");
                control.cancel.cancel();
            } else {
                return Err(ApiError::unsupported("Task has no live execution"));
            }
            // 排队任务可直接落为取消，无须等待当前正在执行的其他任务结束。
            if task.status.state == TaskState::Submitted {
                let mut task = task;
                task.status = TaskStatus::new(TaskState::Canceled);
                self.store.save_event(&task, &status_event(&task)).await?;
                #[cfg(feature = "logging")]
                tracing::info!(event = "task.finished", task_id = id, context_id = task.context_id.as_str(), state = %task.status.state, "排队任务已取消并持久化，不会发给 agent 执行");
                let _ = self.changes.send(());
                return Ok(task);
            }
        }
        // 此处已离开持锁作用域，worker 可以获得同一把锁来写入取消结果。
        let task = self.wait(id, false).await?;
        if task.status.state != TaskState::Canceled {
            return Err(ApiError {
                code: -32002,
                message: "Task finished before cancellation could take effect".into(),
            });
        }
        Ok(task)
    }

    /// 将统一的 agent 事件应用到 Task，再原子保存快照和对应的 A2A 事件。
    async fn event(&self, id: &str, event: AgentEvent) -> Result<()> {
        // 关闭的过程内容既不发送，也不进入任务快照和事件数据库。
        if matches!(&event, AgentEvent::Reasoning { .. }) && !self.options.runtime.forward_reasoning
            || matches!(&event, AgentEvent::Tool(_)) && !self.options.runtime.forward_tools
        {
            return Ok(());
        }
        let _mutation_guard = self.mutation.lock().await;
        let mut task = self.store.task(id).await?.context("task disappeared")?;
        if task.status.state.is_terminal() {
            return Ok(());
        }
        #[cfg(feature = "logging")]
        let previous_state = task.status.state;
        let protocol_event = match event {
            AgentEvent::OutputDelta { item_id, text } => {
                output_event(&mut task, &item_id, &text, true, false)
            }
            AgentEvent::OutputComplete { item_id, text } => {
                output_event(&mut task, &item_id, &text, false, true)
            }
            AgentEvent::Reasoning {
                item_id,
                text,
                append,
                last_chunk,
            } => {
                if text.is_empty()
                    && !task
                        .artifacts
                        .iter()
                        .any(|a| a.artifact_id.as_str() == format!("reasoning-{item_id}"))
                {
                    return Ok(());
                }
                text_event(&mut task, &item_id, &text, append, last_chunk, "reasoning")
            }
            AgentEvent::Tool(event) => {
                if !matches!(event.phase, adapters::ToolPhase::Update) {
                    #[cfg(feature = "logging")]
                    tracing::info!(event = "tool.execution", task_id = id, tool_call_id = event.call_id.as_str(), tool = event.name.as_str(), phase = ?event.phase, "工具执行状态更新；参数和结果通过 A2A 返回，不写入日志");
                }
                StreamResponse::StatusUpdate(tool_event(&mut task, event))
            }
            AgentEvent::Progress(text) => {
                if !task.status.state.is_interrupted() {
                    task.status = TaskStatus::with_message(
                        TaskState::Working,
                        Message::agent_text(text)
                            .with_task_id(task.id.clone())
                            .with_context_id(task.context_id.as_str()),
                    );
                }
                status_event(&task)
            }
            AgentEvent::Interaction(interaction) => {
                ensure!(
                    task.metadata
                        .as_ref()
                        .and_then(|m| m.get("pendingInteraction"))
                        .is_none(),
                    "parallel interactions are unsupported"
                );
                metadata(&mut task).insert(
                    "pendingInteraction".into(),
                    serde_json::to_value(&interaction)?,
                );
                let msg = Message::agent(vec![Part::data(json!({"interaction":interaction}))])
                    .with_task_id(task.id.clone())
                    .with_context_id(task.context_id.as_str());
                task.status = TaskStatus::with_message(TaskState::InputRequired, msg);
                status_event(&task)
            }
        };
        let mut output_bytes: usize = task
            .artifacts
            .iter()
            .flat_map(|a| &a.parts)
            .filter_map(Part::as_text)
            .map(str::len)
            .sum();
        // 工具参数和结果也纳入输出限额；累计输出快照替换原记录，不重复计数。
        for message in task
            .history
            .iter()
            .filter(|message| tool_data(message).is_some())
        {
            output_bytes += serde_json::to_vec(&message.parts)?.len();
        }
        ensure!(
            output_bytes <= self.options.runtime.max_output_bytes,
            "Task output exceeded max_output_bytes"
        );
        self.store.save_event(&task, &protocol_event).await?;
        #[cfg(feature = "logging")]
        match &protocol_event {
            StreamResponse::ArtifactUpdate(update) if update.last_chunk => {
                tracing::info!(
                    event = "artifact.complete",
                    task_id = id,
                    context_id = task.context_id.as_str(),
                    artifact_id = update.artifact.artifact_id.as_str(),
                    output_bytes,
                    append = update.append,
                    last_chunk = update.last_chunk,
                    "收到完整输出：用最终文本更新 Artifact；产物结束不等于整个 Task 已结束"
                );
            }
            StreamResponse::ArtifactUpdate(update) if !update.append => {
                tracing::info!(
                    event = "artifact.started",
                    task_id = id,
                    context_id = task.context_id.as_str(),
                    artifact_id = update.artifact.artifact_id.as_str(),
                    "开始生成 Artifact：后续文本增量将更新同一产物"
                );
            }
            StreamResponse::ArtifactUpdate(update) => {
                tracing::debug!(
                    event = "artifact.delta",
                    task_id = id,
                    artifact_id = update.artifact.artifact_id.as_str(),
                    output_bytes,
                    append = update.append,
                    "文本增量已保存；只记录累计字节数，不记录正文"
                );
            }
            StreamResponse::StatusUpdate(_) if previous_state != task.status.state => {
                tracing::info!(event = "task.state", task_id = id, context_id = task.context_id.as_str(), from = %previous_state, to = %task.status.state, "Task 状态已更新并持久化，订阅者可读取 statusUpdate");
                if let Some(pending) = task
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("pendingInteraction"))
                {
                    tracing::info!(
                        event = "interaction.pending",
                        task_id = id,
                        interaction_id = pending["id"].as_str(),
                        method = pending["method"].as_str(),
                        kind = pending["kind"].as_str(),
                        "agent 等待输入或审批：客户端需发送匹配 interactionId 的结构化回复"
                    );
                }
            }
            _ => {
                tracing::debug!(event = "agent.progress", task_id = id, state = %task.status.state, "agent 进度事件已保存；不记录工具参数或命令正文")
            }
        }
        let _ = self.changes.send(());
        Ok(())
    }

    /// 终态只写入一次；清除待处理交互并释放该任务的控制通道。
    async fn finish(&self, id: &str, state: TaskState, detail: Option<String>) -> Result<()> {
        let _mutation_guard = self.mutation.lock().await;
        let mut task = self.store.task(id).await?.context("task disappeared")?;
        if !task.status.state.is_terminal() {
            task.status = match detail {
                Some(text) => TaskStatus::with_message(
                    state,
                    Message::agent_text(text)
                        .with_task_id(task.id.clone())
                        .with_context_id(task.context_id.as_str()),
                ),
                None => TaskStatus::new(state),
            };
            metadata(&mut task).remove("pendingInteraction");
            if state == TaskState::Completed {
                let text = task
                    .artifacts
                    .iter()
                    .filter(|artifact| {
                        artifact
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.get("a2alinker"))
                            .is_none_or(|metadata| metadata["kind"] != "reasoning")
                    })
                    .flat_map(|a| &a.parts)
                    .filter_map(Part::as_text)
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if !text.is_empty() {
                    task.history.push(
                        Message::agent_text(text)
                            .with_task_id(task.id.clone())
                            .with_context_id(task.context_id.as_str()),
                    );
                }
            }
            self.store.save_event(&task, &status_event(&task)).await?;
            #[cfg(feature = "logging")]
            if state == TaskState::Failed {
                tracing::warn!(event = "task.finished", task_id = id, context_id = task.context_id.as_str(), state = %state, "Task 已失败并持久化；通过 GetTask 查看 status.message 中的具体错误");
            } else {
                tracing::info!(event = "task.finished", task_id = id, context_id = task.context_id.as_str(), state = %state, artifact_count = task.artifacts.len(), "Task 已进入终态并持久化；可以查询结果，同一 context 的后续消息使用新的 messageId");
            }
            let _ = self.changes.send(());
        }
        self.controls.lock().await.remove(id);
        self.queued.lock().await.remove(id);
        Ok(())
    }

    pub async fn shutdown(&self) {
        if !self.shutdown.is_cancelled() {
            #[cfg(feature = "logging")]
            tracing::info!(
                event = "gateway.shutdown",
                "开始关闭网关：取消执行和排队任务，持久化终态并清理 agent 进程"
            );
        }
        self.shutdown.cancel();
        if let Some(worker) = self.worker.lock().await.take() {
            // take() 取走 JoinHandle 并留下 None；只有取得句柄的调用者负责等待清理。
            let _ = worker.await;
        }
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn status_event(task: &Task) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent::new(
        task.id.clone(),
        task.context_id.clone(),
        task.status.clone(),
    ))
}

// 文本增量追加到产物；最终完整文本覆盖同一产物，避免重复累加已收到的内容。
fn output_event(
    task: &mut Task,
    item_id: &str,
    text: &str,
    append: bool,
    final_chunk: bool,
) -> StreamResponse {
    text_event(task, item_id, text, append, final_chunk, "answer")
}

/// 回答和公开思考使用不同 ID 与 metadata，最终聊天历史仅收集回答。
fn text_event(
    task: &mut Task,
    item_id: &str,
    text: &str,
    append: bool,
    final_chunk: bool,
    kind: &str,
) -> StreamResponse {
    let prefix = if kind == "reasoning" {
        "reasoning"
    } else {
        "output"
    };
    let id = format!("{prefix}-{item_id}");
    let artifact = |text: &str| {
        let mut artifact = Artifact::new(id.clone(), vec![Part::text(text)]);
        artifact.metadata = Some(HashMap::from([("a2alinker".into(), json!({"kind":kind}))]));
        if kind == "reasoning" {
            artifact.extensions.push(EXECUTION_EXTENSION.into());
        }
        artifact
    };
    let index = task
        .artifacts
        .iter()
        .position(|a| a.artifact_id.as_str() == id);
    let actual_append = append && index.is_some();
    if let Some(i) = index {
        let previous = task.artifacts[i]
            .parts
            .iter()
            .filter_map(Part::as_text)
            .collect::<String>();
        task.artifacts[i].parts = vec![Part::text(if append {
            previous + text
        } else {
            text.into()
        })];
    } else {
        task.artifacts.push(artifact(text));
    }
    let mut event =
        TaskArtifactUpdateEvent::new(task.id.clone(), task.context_id.clone(), artifact(text));
    event.append = actual_append;
    event.last_chunk = final_chunk;
    StreamResponse::ArtifactUpdate(event)
}

// 会话表只由此 worker 持有和修改，因此不需要额外为每个会话加锁。
async fn worker(
    gateway_weak: Weak<Gateway>,
    mut work_receiver: mpsc::Receiver<Work>,
    adapter: AdapterContext,
    shutdown: CancellationToken,
) {
    let mut sessions = HashMap::<String, SessionEntry>::new();
    let mut cleanup = tokio::time::interval(Duration::from_secs(5));
    loop {
        // select! 同时等待多个异步操作；biased 让退出信号优先得到处理。
        let work = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = cleanup.tick() => {
                sessions.retain(|_context_id, entry| {
                    let within_idle_timeout = entry.last_used_at.elapsed().as_secs()
                        < adapter.options.runtime.session_idle_timeout_seconds;
                    let keep = within_idle_timeout && entry.session.is_alive();
                    #[cfg(feature = "logging")]
                    if !keep {
                        tracing::info!(event = "session.released", context_id = _context_id, native_session_id = entry.session.native_id(), "释放空闲或已退出的 agent 会话进程；保留会话文件以便后续恢复");
                    }
                    keep
                });
                continue;
            }
            work = work_receiver.recv() => match work {
                Some(work) => work,
                None => break,
            },
        };
        // Weak 本身不维持实例存活；只在执行当前任务期间取得一份强引用。
        let Some(gateway) = gateway_weak.upgrade() else {
            break;
        };
        let task_id = work.task_id.clone();
        let result = execute(&gateway, &adapter, &mut sessions, work).await;
        if let Err(error) = result {
            #[cfg(feature = "logging")]
            tracing::error!(task_id = %task_id, error = %error, "task execution failed");
            if let Err(_error) = gateway
                .finish(&task_id, TaskState::Failed, Some(error.to_string()))
                .await
            {
                #[cfg(feature = "logging")]
                tracing::error!(error = %_error, "cannot persist failed task");
            }
        }
    }
    work_receiver.close();
    #[cfg(feature = "logging")]
    tracing::info!(
        event = "worker.stopped",
        session_count = sessions.len(),
        "任务执行器停止，清理会话进程和剩余任务"
    );
    sessions.clear();
    if let Some(gateway) = gateway_weak.upgrade() {
        // 收集 ID 后即释放 controls 锁，避免持有它调用同样需要此锁的 finish()。
        let task_ids: Vec<String> = gateway.controls.lock().await.keys().cloned().collect();
        for task_id in task_ids {
            let _ = gateway
                .finish(
                    &task_id,
                    TaskState::Canceled,
                    Some("Gateway shut down".into()),
                )
                .await;
        }
    }
}

// 执行一个任务：取得/恢复会话，消费事件，处理取消与超时，最后持久化终态。
async fn execute(
    gateway: &Gateway,
    adapter: &AdapterContext,
    sessions: &mut HashMap<String, SessionEntry>,
    work: Work,
) -> Result<()> {
    #[cfg(feature = "logging")]
    let started = Instant::now();
    // 1. 取出排队任务及其控制通道，再在 mutation 锁内将状态切换为 WORKING。
    let task = gateway
        .store
        .task(&work.task_id)
        .await?
        .context("queued task disappeared")?;
    if task.status.state.is_terminal() {
        gateway.controls.lock().await.remove(&work.task_id);
        gateway.queued.lock().await.remove(&work.task_id);
        return Ok(());
    }
    let control = gateway
        .queued
        .lock()
        .await
        .remove(&work.task_id)
        .context("queued control disappeared")?;
    let context_id = task.context_id.to_string();
    if control.cancel.is_cancelled() {
        return gateway
            .finish(&work.task_id, TaskState::Canceled, None)
            .await;
    }
    {
        let _mutation_guard = gateway.mutation.lock().await;
        let mut task = gateway
            .store
            .task(&work.task_id)
            .await?
            .context("queued task disappeared")?;
        if task.status.state.is_terminal() {
            gateway.controls.lock().await.remove(&work.task_id);
            return Ok(());
        }
        task.status = TaskStatus::working();
        gateway
            .store
            .save_event(&task, &status_event(&task))
            .await?;
        #[cfg(feature = "logging")]
        tracing::info!(event = "task.started", task_id = work.task_id, context_id, from = "TASK_STATE_SUBMITTED", to = %task.status.state, "worker 取出 Task 开始执行，保存 WORKING 状态");
        let _ = gateway.changes.send(());
    }
    // 2. 根据 contextId 复用会话；进程已退出或缓存超限时释放旧进程。
    if sessions
        .get_mut(&context_id)
        .is_some_and(|entry| !entry.session.is_alive())
    {
        #[cfg(feature = "logging")]
        tracing::warn!(
            event = "session.unavailable",
            task_id = work.task_id,
            context_id,
            "缓存中的 agent 进程已退出，将尝试恢复原生会话"
        );
        sessions.remove(&context_id);
    }
    if !sessions.contains_key(&context_id) {
        if sessions.len() >= adapter.options.runtime.max_sessions
            && let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, entry)| entry.last_used_at)
                .map(|(context_id, _)| context_id.clone())
        {
            #[cfg(feature = "logging")]
            tracing::info!(
                event = "session.evicted",
                context_id = oldest,
                "会话缓存达到上限，释放最久未使用的进程；会话历史仍然保留"
            );
            sessions.remove(&oldest);
        }
        let stored = gateway
            .store
            .context(&context_id)
            .await?
            .context("context disappeared")?;
        #[cfg(feature = "logging")]
        tracing::info!(
            event = "session.opening",
            task_id = work.task_id,
            context_id,
            action = if stored.native_id.is_some() {
                "resume"
            } else {
                "create"
            },
            agent = adapter.options.agent.kind.as_str(),
            "为 context 创建或恢复原生会话；A2A contextId 与原生会话标识通过数据库映射"
        );
        let startup_future = tokio::time::timeout(
            Duration::from_secs(adapter.options.runtime.startup_timeout_seconds),
            adapters::create_session(adapter, stored.native_id.as_deref()),
        );
        // 启动握手也可被取消；即使 agent 没有回应，外层仍有超时保证。
        let session = tokio::select! {
            _ = control.cancel.cancelled() => return gateway.finish(&work.task_id, TaskState::Canceled, None).await,
            _ = gateway.shutdown.cancelled() => return gateway.finish(&work.task_id, TaskState::Canceled, None).await,
            startup_result = startup_future => {
                let session_result = startup_result.context("agent startup timed out")?;
                session_result?
            },
        };
        gateway
            .store
            .set_native_id(&context_id, session.native_id())
            .await?;
        #[cfg(feature = "logging")]
        tracing::info!(
            event = "session.ready",
            task_id = work.task_id,
            context_id,
            native_session_id = session.native_id(),
            "原生会话就绪，映射已保存；pi 的新 JSONL 文件通常在首次 assistant 消息后生成"
        );
        sessions.insert(
            context_id.clone(),
            SessionEntry {
                session,
                last_used_at: Instant::now(),
            },
        );
    } else {
        #[cfg(feature = "logging")]
        tracing::info!(
            event = "session.reused",
            task_id = work.task_id,
            context_id,
            native_session_id = sessions[&context_id].session.native_id(),
            "复用同一 context 的存活会话，保留对话上下文"
        );
    }
    // 3. 同时消费原生事件、执行结果、取消信号和超时信号。
    let result = {
        #[cfg(feature = "logging")]
        tracing::info!(
            event = "agent.run",
            task_id = work.task_id,
            context_id,
            prompt_bytes = work.prompt.len(),
            "向 agent 提交本轮消息，开始接收输出、进度或交互事件；不记录提示词正文"
        );
        let session_entry = sessions
            .get_mut(&context_id)
            .expect("session must be created or reused before execution");
        let session = &mut session_entry.session;
        let (event_sender, mut event_receiver) = mpsc::channel(128);
        let run_future = session.run(
            &work.prompt,
            event_sender,
            control.replies,
            control.cancel.clone(),
        );
        // Future 固定位置后，可在循环中反复等待同一次 run，而不会重复启动推理。
        tokio::pin!(run_future);
        let mut deadline =
            Instant::now() + Duration::from_secs(adapter.options.runtime.task_timeout_seconds);
        let mut interrupted = false;
        let mut timed_out = false;
        loop {
            tokio::select! {
                event = event_receiver.recv(), if !event_receiver.is_closed() || !event_receiver.is_empty() => {
                    if let Some(event) = event
                        && let Err(error) = gateway.event(&work.task_id, event).await
                    {
                        break Err(error);
                    }
                }
                _ = control.cancel.cancelled(), if !interrupted => {
                    #[cfg(feature = "logging")]
                    tracing::info!(event = "agent.interrupt", task_id = work.task_id, context_id, grace_seconds = adapter.options.runtime.cancel_timeout_seconds, "通知原生 agent 中断；超过宽限时间后会释放进程");
                    interrupted = true;
                    deadline = Instant::now() + Duration::from_secs(adapter.options.runtime.cancel_timeout_seconds);
                }
                _ = gateway.shutdown.cancelled(), if !interrupted => {
                    #[cfg(feature = "logging")]
                    tracing::info!(event = "agent.interrupt", task_id = work.task_id, context_id, "网关关闭，通知原生 agent 中断当前任务");
                    control.cancel.cancel();
                    interrupted = true;
                    deadline = Instant::now() + Duration::from_secs(adapter.options.runtime.cancel_timeout_seconds);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    if interrupted {
                        #[cfg(feature = "logging")]
                        tracing::warn!(event = "agent.interrupt_timeout", task_id = work.task_id, context_id, "原生中断超过宽限时间，将释放会话并清理进程组");
                        break if timed_out { Err(anyhow!("Task timed out")) } else { Ok(RunOutcome::Canceled) };
                    }
                    #[cfg(feature = "logging")]
                    tracing::warn!(event = "task.timeout", task_id = work.task_id, context_id, timeout_seconds = adapter.options.runtime.task_timeout_seconds, "任务达到时间上限，先尝试原生中断，再清理进程");
                    timed_out = true;
                    interrupted = true;
                    control.cancel.cancel();
                    deadline = Instant::now() + Duration::from_secs(adapter.options.runtime.cancel_timeout_seconds);
                }
                result = &mut run_future => {
                    let mut result = result;
                    // run 完成时通道中可能还有最后几条事件，先处理完，再写终态。
                    while let Ok(event) = event_receiver.try_recv() {
                        if let Err(error) = gateway.event(&work.task_id, event).await {
                            result = Err(error);
                            break;
                        }
                    }
                    break if timed_out { Err(anyhow!("Task timed out")) } else { result };
                }
            }
        }
    };
    let state = match &result {
        Ok(RunOutcome::Completed) => TaskState::Completed,
        Ok(RunOutcome::Canceled) => TaskState::Canceled,
        Err(_) => TaskState::Failed,
    };
    // 4. 失败/取消时释放会话及进程；成功的会话留下以供同一 context 后续任务复用。
    if state != TaskState::Completed {
        #[cfg(feature = "logging")]
        tracing::info!(
            event = "session.released",
            task_id = work.task_id,
            context_id,
            "任务失败或取消，释放原生会话进程；已生成的会话文件不会删除"
        );
        sessions.remove(&context_id);
    } else if let Some(session) = sessions.get_mut(&context_id) {
        session.last_used_at = Instant::now();
    }
    let error_detail = match result {
        Ok(_) => None,
        Err(error) => Some(error.to_string()),
    };
    gateway.finish(&work.task_id, state, error_detail).await?;
    #[cfg(feature = "logging")]
    tracing::info!(event = "task.execution_end", task_id = work.task_id, context_id, state = %state, elapsed_ms = started.elapsed().as_millis() as u64, "本轮执行已结束；耗时包含会话准备、模型处理和等待交互，不含排队时间");
    Ok(())
}

pub fn trim_history(task: &mut Task, length: Option<i32>) {
    if let Some(n) = length {
        let remove = task.history.len().saturating_sub(n as usize);
        task.history.drain(..remove);
    }
}
