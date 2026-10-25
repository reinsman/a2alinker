//! HTTP 协议边界：Agent Card、认证、JSON-RPC 参数检查，以及 SSE 流式响应。
//! 具体执行交给 Gateway，网络连接的生命周期与 agent 任务的生命周期分开管理。

use std::{collections::HashMap, convert::Infallible, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use ra2a::types::{
    AgentCard, AgentExtension, AgentInterface, AgentSkill, CancelTaskRequest, GetTaskRequest,
    HttpAuthSecurityScheme, ListTasksRequest, ListTasksResponse, SecurityRequirement,
    SecurityScheme, SendMessageRequest, StreamResponse, SubscribeToTaskRequest,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
#[cfg(feature = "logging")]
use tracing::Instrument;

use crate::{
    adapters::execution::EXECUTION_EXTENSION,
    options::AuthMode,
    runtime::{ApiError, ApiResult, Gateway, INTERACTION_EXTENSION, trim_history},
};

#[derive(Clone)]
struct App {
    gateway: Arc<Gateway>,
    token_hash: Option<[u8; 32]>,
}

/// 构建共享同一网关的路由；App 的 Clone 只克隆 Arc 等轻量句柄。
pub fn router(gateway: Arc<Gateway>) -> Result<Router> {
    let token_hash = if gateway.options.auth.mode == AuthMode::Bearer {
        let token = gateway
            .options
            .auth
            .token
            .as_deref()
            .context("bearer token is not set in AuthOptions")?;
        ensure!(
            token.len() >= 32,
            "bearer token must contain at least 32 bytes"
        );
        let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        Some(token_hash)
    } else {
        None
    };
    let limit = gateway.options.server.max_request_bytes;
    Ok(Router::new()
        .route("/.well-known/agent-card.json", get(card))
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/a2a", post(rpc))
        .layer(DefaultBodyLimit::max(limit))
        .with_state(App {
            gateway,
            token_hash,
        }))
}

pub fn agent_card(gateway: &Gateway) -> AgentCard {
    let options = &gateway.options;
    let mut card = AgentCard::new(
        &options.identity.name,
        &options.identity.description,
        vec![AgentInterface::new(
            format!("{}/a2a", options.public_url()),
            "JSONRPC".into(),
        )],
    );
    card.version = env!("CARGO_PKG_VERSION").into();
    card.capabilities.streaming = Some(true);
    card.capabilities.push_notifications = Some(false);
    card.capabilities.extended_agent_card = Some(false);
    card.capabilities.extensions = vec![AgentExtension {
        uri: INTERACTION_EXTENSION.into(),
        description: Some("Structured input and approval replies".into()),
        required: false,
        params: None,
    }];
    if options.agent.kind == crate::options::AgentKind::Pi
        && (options.runtime.forward_reasoning || options.runtime.forward_tools)
    {
        card.capabilities.extensions.push(AgentExtension {
            uri: EXECUTION_EXTENSION.into(),
            description: Some("Public reasoning and structured tool execution".into()),
            required: false,
            params: Some(HashMap::from([
                ("reasoning".into(), json!(options.runtime.forward_reasoning)),
                ("tools".into(), json!(options.runtime.forward_tools)),
            ])),
        });
    }
    card.default_input_modes = vec!["text/plain".into(), "application/json".into()];
    // A2A 要求至少一项能力声明；占位项表示未声明具体能力，不代表没有本地技能。
    card.skills = vec![AgentSkill::new(
        "dummy",
        "未声明具体能力",
        "此 Agent 未提供具体能力清单，请通过消息询问。",
        vec!["unspecified".into()],
    )];
    if options.auth.mode == AuthMode::Bearer {
        card.security_schemes.insert(
            "bearer".into(),
            SecurityScheme::Http(HttpAuthSecurityScheme::bearer()),
        );
        card.security_requirements.push(SecurityRequirement {
            schemes: HashMap::from([("bearer".into(), vec![])]),
        });
    }
    card
}

async fn card(State(app): State<App>) -> Response {
    let card = agent_card(&app.gateway);
    #[cfg(feature = "logging")]
    tracing::info!(
        event = "a2a.discovery",
        protocol_version = "1.0",
        agent = app.gateway.options.agent.kind.as_str(),
        skill_count = card.skills.len(),
        "客户端读取 Agent Card：发现服务能力、协议和认证要求"
    );
    let mut response = Json(card).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60"),
    );
    response
}

// State 是 Axum 的提取器：框架从路由共享状态中取出 App，并解析请求头和请求体。
async fn rpc(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    #[cfg(feature = "logging")]
    let started = tokio::time::Instant::now();
    if !authorized(&headers, app.token_hash.as_ref()) {
        #[cfg(feature = "logging")]
        tracing::warn!(
            event = "a2a.auth_rejected",
            http_status = 401,
            "A2A 请求认证失败：需要有效的实例 bearer token"
        );
        let mut response = (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"Unauthorized"})),
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    }
    // 限定 JSON 内容类型，阻止浏览器以跨域简单表单请求直接提交 RPC。
    let content_type_header = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let content_type = content_type_header.split(';').next().unwrap_or("").trim();
    if !["application/json", "application/a2a+json"].contains(&content_type) {
        #[cfg(feature = "logging")]
        tracing::warn!(
            event = "a2a.content_type_rejected",
            http_status = 415,
            "A2A 请求被拒绝：Content-Type 必须是 JSON"
        );
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let request: Value = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => {
            return error_response(
                Value::Null,
                ApiError {
                    code: -32700,
                    message: "Invalid JSON".into(),
                },
            );
        }
    };
    let request_id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str);
    if request["jsonrpc"] != "2.0"
        || method.is_none()
        || request.get("id").is_none()
        || !(request_id.is_null()
            || request_id.is_string()
            || request_id.is_i64()
            || request_id.is_u64())
    {
        return error_response(
            Value::Null,
            ApiError {
                code: -32600,
                message: "Expected a JSON-RPC 2.0 request with an id".into(),
            },
        );
    }
    if let Some(version) = headers.get("a2a-version")
        && !matches!(version.to_str(), Ok("1.0" | "1.0.0"))
    {
        return error_response(
            request_id,
            ApiError {
                code: -32010,
                message: "Only A2A 1.0 is supported".into(),
            },
        );
    }
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    if params.get("tenant").is_some_and(|v| v.as_str() != Some("")) {
        return error_response(
            request_id,
            ApiError::unsupported("This instance does not support tenants"),
        );
    }
    let method = method.expect("JSON-RPC method must be validated before dispatch");
    // Instrument 让异步处理期间的日志自动带上请求关联信息，不在 .await 期间持有 enter guard。
    #[cfg(feature = "logging")]
    let span = tracing::info_span!("a2a_request", request_id = ?request_id, method);
    let request = async {
        #[cfg(feature = "logging")]
        tracing::info!(
            event = "a2a.request",
            request_bytes = body.len(),
            "收到 A2A JSON-RPC 请求，开始分派方法"
        );
        dispatch(&app.gateway, &headers, &request_id, method, params).await
    };
    #[cfg(feature = "logging")]
    let request = request.instrument(span);
    let result = request.await;
    match result {
        Ok(response) => {
            #[cfg(feature = "logging")]
            let streaming = response
                .headers()
                .get(header::CONTENT_TYPE)
                .is_some_and(|value| value == "text/event-stream");
            #[cfg(feature = "logging")]
            tracing::info!(
                event = "a2a.response", request_id = ?request_id, method,
                http_status = response.status().as_u16(), streaming,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "A2A HTTP 响应已返回；streaming=true 时任务结果继续通过 SSE 发送"
            );
            response
        }
        Err(error) => error_response(request_id, error),
    }
}

// 当前是实例共享 bearer token 认证，没有用户身份、角色或任务所有权层面的 RBAC。
// 比较固定长度摘要，使用常量时间比较接口，避免普通字符串比较的提前退出行为。
fn authorized(headers: &HeaderMap, expected: Option<&[u8; 32]>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    let Some(authorization) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(authorization) = authorization.to_str() else {
        return false;
    };
    let Some(token) = authorization.strip_prefix("Bearer ") else {
        return false;
    };
    let actual: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    // 先做常量时间比较，再将比较结果转成 bool；不直接用普通字节数组相等判断。
    bool::from(actual.ct_eq(expected))
}

async fn dispatch(
    gateway: &Arc<Gateway>,
    headers: &HeaderMap,
    request_id: &Value,
    method: &str,
    params: Value,
) -> ApiResult<Response> {
    match method {
        "SendMessage" | "SendStreamingMessage" => {
            if let Some(parts) = params["message"]["parts"].as_array() {
                for part in parts {
                    if ["text", "raw", "url", "data"]
                        .iter()
                        .filter(|name| part.get(**name).is_some())
                        .count()
                        != 1
                    {
                        return Err(ApiError::invalid(
                            "Each part must contain exactly one content field",
                        ));
                    }
                }
            }
            let request: SendMessageRequest = parse(params)?;
            let options = request.configuration.unwrap_or_default();
            history_length(options.history_length)?;
            if options.task_push_notification_config.is_some() {
                return Err(ApiError {
                    code: -32003,
                    message: "Push notifications are not supported".into(),
                });
            }
            if !options.accepted_output_modes.is_empty()
                && !options
                    .accepted_output_modes
                    .iter()
                    .any(|m| m == "text/plain")
            {
                return Err(ApiError {
                    code: -32005,
                    message: "Output mode must include text/plain".into(),
                });
            }
            if request.message.parts.iter().any(|p| p.as_data().is_some())
                && !has_interaction_extension(headers)
            {
                return Err(ApiError::invalid(format!(
                    "Structured replies require A2A-Extensions: {INTERACTION_EXTENSION}"
                )));
            }
            let task = gateway.submit(request.message).await?;
            if method == "SendStreamingMessage" {
                return stream(
                    Arc::clone(gateway),
                    request_id.clone(),
                    task.id.to_string(),
                    None,
                    options.history_length,
                )
                .await;
            }
            let mut task = if options.return_immediately {
                task
            } else {
                gateway.wait(task.id.as_str(), true).await?
            };
            trim_history(&mut task, options.history_length);
            Ok(success(request_id, json!({"task":task})))
        }
        "GetTask" => {
            let request: GetTaskRequest = parse(params)?;
            history_length(request.history_length)?;
            let mut task = gateway.task(request.id.as_str()).await?;
            #[cfg(feature = "logging")]
            tracing::info!(event = "task.query", task_id = task.id.as_str(), context_id = task.context_id.as_str(), state = %task.status.state, "GetTask：读取已保存的任务状态和产物，不发起新的模型调用");
            trim_history(&mut task, request.history_length);
            Ok(success(
                request_id,
                serde_json::to_value(task).map_err(anyhow::Error::from)?,
            ))
        }
        "CancelTask" => {
            let request: CancelTaskRequest = parse(params)?;
            Ok(success(
                request_id,
                serde_json::to_value(gateway.cancel(request.id.as_str()).await?)
                    .map_err(anyhow::Error::from)?,
            ))
        }
        "SubscribeToTask" => {
            let request: SubscribeToTaskRequest = parse(params)?;
            let task = gateway.task(request.id.as_str()).await?;
            if task.status.state.is_terminal() {
                return Err(ApiError::unsupported("Use GetTask for a terminal task"));
            }
            let cursor = match headers.get("last-event-id") {
                Some(value) => {
                    let cursor = value
                        .to_str()
                        .map_err(|_| ApiError::invalid("Invalid Last-Event-ID"))?;
                    Some(String::from(cursor))
                }
                None => None,
            };
            stream(
                Arc::clone(gateway),
                request_id.clone(),
                request.id.to_string(),
                cursor,
                None,
            )
            .await
        }
        "ListTasks" => {
            let request: ListTasksRequest = parse(params)?;
            history_length(request.history_length)?;
            let page_size = request.page_size.unwrap_or(50);
            if !(1..=100).contains(&page_size) {
                return Err(ApiError::invalid("pageSize must be 1..=100"));
            }
            let offset = match request.page_token.as_deref().filter(|s| !s.is_empty()) {
                Some(token) => token
                    .strip_prefix("offset:")
                    .and_then(|s| s.parse::<usize>().ok())
                    .ok_or_else(|| ApiError::invalid("Invalid pageToken"))?,
                None => 0,
            };
            let updated_after = match request.status_timestamp_after.as_deref() {
                Some(value) => {
                    let timestamp = chrono::DateTime::parse_from_rfc3339(value)
                        .map_err(|_| ApiError::invalid("Invalid statusTimestampAfter"))?;
                    Some(timestamp)
                }
                None => None,
            };
            let tasks = gateway.store.tasks().await?;
            let filtered: Vec<_> = tasks
                .into_iter()
                .filter(|task| {
                    if let Some(context_id) = &request.context_id
                        && context_id != task.context_id.as_str()
                    {
                        return false;
                    }
                    if let Some(state) = request.status
                        && state != task.status.state
                    {
                        return false;
                    }
                    let Some(updated_after) = &updated_after else {
                        return true;
                    };
                    let timestamp = task
                        .status
                        .timestamp
                        .as_deref()
                        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
                    match timestamp {
                        Some(timestamp) => timestamp >= *updated_after,
                        None => false,
                    }
                })
                .collect();
            if offset > filtered.len() {
                return Err(ApiError::invalid("pageToken is outside the result set"));
            }
            let total = filtered.len();
            let tasks = filtered
                .into_iter()
                .skip(offset)
                .take(page_size as usize)
                .map(|mut task| {
                    trim_history(&mut task, request.history_length);
                    if request.include_artifacts != Some(true) {
                        task.artifacts.clear();
                    }
                    task
                })
                .collect::<Vec<_>>();
            let next = offset + tasks.len();
            let response = ListTasksResponse {
                tasks,
                next_page_token: if next < total {
                    format!("offset:{next}")
                } else {
                    String::new()
                },
                page_size,
                total_size: total as i32,
            };
            #[cfg(feature = "logging")]
            tracing::info!(
                event = "task.list",
                returned = response.tasks.len(),
                total = response.total_size,
                "ListTasks：返回符合筛选条件的任务列表"
            );
            Ok(success(
                request_id,
                serde_json::to_value(response).map_err(anyhow::Error::from)?,
            ))
        }
        "CreateTaskPushNotificationConfig"
        | "GetTaskPushNotificationConfig"
        | "ListTaskPushNotificationConfigs"
        | "DeleteTaskPushNotificationConfig" => Err(ApiError {
            code: -32003,
            message: "Push notifications are not supported".into(),
        }),
        "GetExtendedAgentCard" => Err(ApiError::unsupported(
            "Extended agent cards are not supported",
        )),
        _ => Err(ApiError {
            code: -32601,
            message: "Method not found".into(),
        }),
    }
}

// 独立拥有此守卫，连接被丢弃时也能记录结束原因；它只观察，不取消任务。
#[cfg(feature = "logging")]
struct StreamLog {
    request_id: Value,
    task_id: String,
    started: tokio::time::Instant,
    events: u64,
    reason: &'static str,
}

#[cfg(feature = "logging")]
impl StreamLog {
    fn sent(&mut self) {
        self.events += 1;
    }

    fn end(&mut self, reason: &'static str) {
        self.reason = reason;
    }
}

#[cfg(feature = "logging")]
impl Drop for StreamLog {
    fn drop(&mut self) {
        tracing::info!(
            event = "sse.closed", request_id = ?self.request_id,
            task_id = self.task_id.as_str(), reason = self.reason, events = self.events,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            "SSE 订阅已结束；连接断开本身不会取消任务"
        );
    }
}

// 响应流被丢弃只会结束订阅；真正的任务由 Gateway 和 worker 持有，继续执行。
async fn stream(
    gateway: Arc<Gateway>,
    request_id: Value,
    task_id: String,
    cursor: Option<String>,
    history: Option<i32>,
) -> ApiResult<Response> {
    let mut notifications = gateway.subscribe();
    #[cfg(feature = "logging")]
    let reconnect = cursor.is_some();
    if let Some(cursor) = cursor {
        let prefix = format!("{task_id}:");
        let seq = cursor
            .strip_prefix(&prefix)
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or_else(|| ApiError::invalid("Invalid Last-Event-ID"))?;
        gateway
            .store
            .validate_cursor(&task_id, seq)
            .await
            .map_err(|_| ApiError::invalid("Last-Event-ID does not belong to this task"))?;
    }
    // 订阅首先发送当前 Task 快照。重连时客户端用它替换旧快照，
    // 然后只接收后续事件，避免重放快照中已包含的文本增量。
    let (mut task, mut after) = gateway
        .store
        .snapshot(&task_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    trim_history(&mut task, history);
    let initial_terminal = task.status.state.is_terminal();
    #[cfg(feature = "logging")]
    tracing::info!(
        event = "sse.opened", request_id = ?request_id, task_id, context_id = task.context_id.as_str(),
        state = %task.status.state, cursor = after, reconnect,
        "建立 SSE 订阅：先发送当前 Task 完整快照，再发送后续事件"
    );
    #[cfg(feature = "logging")]
    let mut lifecycle = StreamLog {
        request_id: request_id.clone(),
        task_id: task_id.clone(),
        started: tokio::time::Instant::now(),
        events: 0,
        reason: if initial_terminal {
            "task_terminal"
        } else {
            "client_disconnected"
        },
    };
    // yield 每次产生一条 SSE；数据库是事件来源，广播通知用于减少轮询延迟。
    let output = async_stream::stream! {
        #[cfg(feature = "logging")]
        lifecycle.sent();
        yield Ok::<Event, Infallible>(sse_event(&request_id, &task_id, after, StreamResponse::Task(task)));
        if initial_terminal {
            #[cfg(feature = "logging")]
            lifecycle.end("task_terminal");
            return;
        }
        loop {
            match gateway.store.events(&task_id, after).await {
                Ok(events) if !events.is_empty() => {
                    for (seq, mut event) in events {
                        after = seq;
                        if let StreamResponse::Task(task) = &mut event {
                            trim_history(task, history);
                        }
                        let terminal = event.is_terminal();
                        // 客户端可能收到终态后立即断开，必须在 yield 前标记正常结束。
                        #[cfg(feature = "logging")]
                        if terminal {
                            lifecycle.end("task_terminal");
                        }
                        #[cfg(feature = "logging")]
                        lifecycle.sent();
                        yield Ok(sse_event(&request_id, &task_id, seq, event));
                        if terminal {
                            return;
                        }
                    }
                }
                Ok(_) => {
                    tokio::select! {
                        _ = notifications.recv() => {},
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                    }
                }
                Err(_error) => {
                    #[cfg(feature = "logging")]
                    lifecycle.end("storage_error");
                    #[cfg(feature = "logging")]
                    tracing::error!(error = %_error, "SSE event read failed");
                    #[cfg(feature = "logging")]
                    lifecycle.sent();
                    yield Ok(Event::default().data(json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32603,"message":"Event stream storage error"}}).to_string()));
                    return;
                }
            }
        }
    };
    Ok(Sse::new(output)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

fn sse_event(request_id: &Value, task_id: &str, seq: i64, event: StreamResponse) -> Event {
    #[cfg(feature = "logging")]
    let kind = match &event {
        StreamResponse::Task(_) => "task",
        StreamResponse::Message(_) => "message",
        StreamResponse::StatusUpdate(_) => "statusUpdate",
        StreamResponse::ArtifactUpdate(_) => "artifactUpdate",
    };
    #[cfg(feature = "logging")]
    tracing::debug!(event = "sse.event", request_id = ?request_id, task_id, cursor = seq, kind, "发送一条 SSE 事件；不记录事件正文");
    Event::default()
        .id(format!("{task_id}:{seq}"))
        .data(json!({"jsonrpc":"2.0","id":request_id,"result":event}).to_string())
}

fn has_interaction_extension(headers: &HeaderMap) -> bool {
    headers
        .get("a2a-extensions")
        .and_then(|s| s.to_str().ok())
        .is_some_and(|s| s.split(',').any(|s| s.trim() == INTERACTION_EXTENSION))
}
// 泛型 T 表示目标请求类型；DeserializeOwned 要求反序列化结果自行拥有数据。
fn parse<T: DeserializeOwned>(params: Value) -> ApiResult<T> {
    serde_json::from_value(params).map_err(|e| ApiError::invalid(e.to_string()))
}
fn history_length(n: Option<i32>) -> ApiResult<()> {
    if n.is_some_and(|n| n < 0) {
        Err(ApiError::invalid("historyLength must be nonnegative"))
    } else {
        Ok(())
    }
}
fn success(request_id: &Value, value: Value) -> Response {
    Json(json!({"jsonrpc":"2.0","id":request_id,"result":value})).into_response()
}
fn error_response(request_id: Value, error: ApiError) -> Response {
    // 参数错误有时包含客户端原始值，因此日志只记录错误码；详情仍按协议返回调用方。
    #[cfg(feature = "logging")]
    tracing::warn!(event = "a2a.error", request_id = ?request_id, rpc_error_code = error.code, "A2A 请求失败：错误详情见 JSON-RPC 响应中的 error");
    Json(json!({"jsonrpc":"2.0","id":request_id,"error":{"code":error.code,"message":error.message}}))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bearer_authentication() {
        let expected: [u8; 32] = Sha256::digest(b"a-strong-test-token").into();
        let mut headers = HeaderMap::new();
        assert!(!authorized(&headers, Some(&expected)));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        assert!(!authorized(&headers, Some(&expected)));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer a-strong-test-token"),
        );
        assert!(authorized(&headers, Some(&expected)));
    }
}
