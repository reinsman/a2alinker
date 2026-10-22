//! 公开执行过程的公共数据结构，以及工具快照的合并。
//! 只输出上游提供的公开字段；模型的签名、加密推理等字段不属于本接口。

use ra2a::types::{Message, Part, PartContent, Task, TaskStatus, TaskStatusUpdateEvent};
use serde_json::{Value, json};

pub const EXECUTION_EXTENSION: &str = "urn:a2alinker:execution:v1";

#[derive(Debug, Clone, Copy)]
pub enum ToolPhase {
    Start,
    Update,
    End,
}

#[derive(Debug)]
pub struct ToolEvent {
    pub call_id: String,
    pub name: String,
    pub phase: ToolPhase,
    pub arguments: Option<Value>,
    /// update 的结果是截至此刻的完整快照，客户端应替换，而不是追加。
    pub result: Option<Value>,
    pub is_error: Option<bool>,
}

/// 从消息中提取工具记录；同时用于查询和更新任务历史中的同一次调用。
pub(crate) fn tool_data(message: &Message) -> Option<&Value> {
    if message.role != ra2a::types::Role::Agent
        || !message
            .extensions
            .iter()
            .any(|uri| uri == EXECUTION_EXTENSION)
    {
        return None;
    }
    message.parts.iter().find_map(|part| match &part.content {
        PartContent::Data(data) if data["a2alinker"]["kind"] == "tool" => Some(&data["a2alinker"]),
        _ => None,
    })
}

/// 每个 toolCallId 在 history 中只保存一份最新快照，避免累计输出被重复堆积。
/// 每次更新仍有独立 SSE 事件；断线后的 Task 快照可以恢复所有工具的最新状态。
pub(crate) fn tool_event(task: &mut Task, event: ToolEvent) -> TaskStatusUpdateEvent {
    let index = task.history.iter().position(|message| {
        tool_data(message).and_then(|data| data["toolCallId"].as_str())
            == Some(event.call_id.as_str())
    });
    let mut data = index
        .and_then(|i| tool_data(&task.history[i]).cloned())
        .unwrap_or_else(|| json!({"kind":"tool", "toolCallId":event.call_id}));
    let (event_type, state, description) = match event.phase {
        ToolPhase::Start => ("tool_call", "running", "Running tool"),
        ToolPhase::Update => ("tool_update", "running", "Tool output"),
        ToolPhase::End if event.is_error == Some(true) => ("tool_result", "failed", "Tool failed"),
        ToolPhase::End => ("tool_result", "completed", "Tool completed"),
    };
    data["type"] = json!(event_type);
    data["state"] = json!(state);
    data["name"] = json!(event.name);
    if let Some(arguments) = event.arguments {
        data["arguments"] = redact_fields(arguments);
    }
    if let Some(result) = event.result {
        data["result"] = redact_fields(result);
    }
    if let Some(is_error) = event.is_error {
        data["isError"] = json!(is_error);
    }
    let mut message = Message::agent(vec![
        Part::text(format!("{description}: {}", event.name)),
        Part::data(json!({"a2alinker":data})),
    ])
    .with_task_id(task.id.clone())
    .with_context_id(task.context_id.as_str());
    message.extensions.push(EXECUTION_EXTENSION.into());
    if let Some(i) = index {
        message.id = task.history[i].id.clone();
        task.history[i] = message.clone();
    } else {
        task.history.push(message.clone());
    }
    // 后台工具事件不能把等待用户输入的任务错误地恢复成 WORKING。
    let state = if task.status.state.is_interrupted() {
        task.status.state
    } else {
        ra2a::types::TaskState::Working
    };
    let status = TaskStatus::with_message(state, message);
    if !task.status.state.is_interrupted() {
        task.status = status.clone();
    }
    TaskStatusUpdateEvent::new(task.id.clone(), task.context_id.clone(), status)
}

/// 对结构化字段名做有限脱敏；自由文本（如 shell 命令、文件内容）无法可靠识别密钥。
/// 因此不能把这个函数当作完整的凭据过滤器，也不能把原始过程报文写入日志。
fn redact_fields(value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, value)| {
                    let normalized = key.to_ascii_lowercase().replace(['_', '-'], "");
                    let sensitive = matches!(
                        normalized.as_str(),
                        "authorization"
                            | "proxyauthorization"
                            | "apikey"
                            | "token"
                            | "accesstoken"
                            | "refreshtoken"
                            | "password"
                            | "secret"
                            | "clientsecret"
                            | "cookie"
                            | "setcookie"
                    );
                    let value = if sensitive {
                        json!("[REDACTED]")
                    } else {
                        redact_fields(value)
                    };
                    (key, value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(redact_fields).collect()),
        other => other,
    }
}
