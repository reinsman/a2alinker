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

use crate::{
    config::AuthMode,
    runtime::{ApiError, ApiResult, Gateway, INTERACTION_EXTENSION, trim_history},
};

#[derive(Clone)]
struct App {
    gateway: Arc<Gateway>,
    token_hash: Option<[u8; 32]>,
}

pub fn router(gateway: Arc<Gateway>) -> Result<Router> {
    let token_hash = if gateway.config.auth.mode == AuthMode::Bearer {
        let token = std::env::var(&gateway.config.auth.token_env)
            .context("bearer token environment variable is not set")?;
        ensure!(
            token.len() >= 32,
            "bearer token must contain at least 32 bytes"
        );
        Some(Sha256::digest(token.as_bytes()).into())
    } else {
        None
    };
    let limit = gateway.config.server.max_request_bytes;
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
    let cfg = &gateway.config;
    let mut card = AgentCard::new(
        &cfg.identity.name,
        &cfg.identity.description,
        vec![AgentInterface::new(
            format!("{}/a2a", cfg.public_url()),
            "JSONRPC".into(),
        )],
    );
    card.version = env!("CARGO_PKG_VERSION").into();
    card.capabilities.streaming = Some(true);
    card.capabilities.push_notifications = Some(false);
    card.capabilities.extended_agent_card = Some(false);
    card.capabilities.extensions = vec![AgentExtension {
        uri: INTERACTION_EXTENSION.into(),
        description: Some("Structured input and approval replies; see docs/interaction.md".into()),
        required: false,
        params: None,
    }];
    card.default_input_modes = vec!["text/plain".into(), "application/json".into()];
    card.skills = cfg
        .capabilities
        .iter()
        .map(|c| {
            AgentSkill::new(&c.id, &c.name, &c.description, c.tags.clone())
                .with_examples(c.examples.clone())
        })
        .collect();
    if cfg.auth.mode == AuthMode::Bearer {
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
    let mut response = Json(agent_card(&app.gateway)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=60"),
    );
    response
}

async fn rpc(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&headers, app.token_hash.as_ref()) {
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
    // JSON requests cannot be sent as a browser's cross-origin simple form submission.
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if !["application/json", "application/a2a+json"].contains(&content_type) {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let request: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
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
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str);
    if request["jsonrpc"] != "2.0"
        || method.is_none()
        || request.get("id").is_none()
        || !(id.is_null() || id.is_string() || id.is_i64() || id.is_u64())
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
            id,
            ApiError {
                code: -32010,
                message: "Only A2A 1.0 is supported".into(),
            },
        );
    }
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    if params.get("tenant").is_some_and(|v| v.as_str() != Some("")) {
        return error_response(
            id,
            ApiError::unsupported("This instance does not support tenants"),
        );
    }
    let result = dispatch(&app.gateway, &headers, &id, method.unwrap(), params).await;
    match result {
        Ok(response) => response,
        Err(e) => error_response(id, e),
    }
}

fn authorized(headers: &HeaderMap, expected: Option<&[u8; 32]>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    let actual: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    actual.ct_eq(expected).into()
}

async fn dispatch(
    gateway: &Arc<Gateway>,
    headers: &HeaderMap,
    id: &Value,
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
            let config = request.configuration.unwrap_or_default();
            history_length(config.history_length)?;
            if config.task_push_notification_config.is_some() {
                return Err(ApiError {
                    code: -32003,
                    message: "Push notifications are not supported".into(),
                });
            }
            if !config.accepted_output_modes.is_empty()
                && !config
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
                    gateway.clone(),
                    id.clone(),
                    task.id.to_string(),
                    None,
                    config.history_length,
                )
                .await;
            }
            let mut task = if config.return_immediately {
                task
            } else {
                gateway.wait(task.id.as_str(), true).await?
            };
            trim_history(&mut task, config.history_length);
            Ok(success(id, json!({"task":task})))
        }
        "GetTask" => {
            let request: GetTaskRequest = parse(params)?;
            history_length(request.history_length)?;
            let mut task = gateway.task(request.id.as_str()).await?;
            trim_history(&mut task, request.history_length);
            Ok(success(
                id,
                serde_json::to_value(task).map_err(anyhow::Error::from)?,
            ))
        }
        "CancelTask" => {
            let request: CancelTaskRequest = parse(params)?;
            Ok(success(
                id,
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
            let cursor = headers
                .get("last-event-id")
                .map(|v| v.to_str().map(str::to_owned))
                .transpose()
                .map_err(|_| ApiError::invalid("Invalid Last-Event-ID"))?;
            stream(
                gateway.clone(),
                id.clone(),
                request.id.to_string(),
                cursor,
                None,
            )
            .await
        }
        "ListTasks" => {
            let request: ListTasksRequest = parse(params)?;
            history_length(request.history_length)?;
            let size = request.page_size.unwrap_or(50);
            if !(1..=100).contains(&size) {
                return Err(ApiError::invalid("pageSize must be 1..=100"));
            }
            let offset = match request.page_token.as_deref().filter(|s| !s.is_empty()) {
                Some(token) => token
                    .strip_prefix("offset:")
                    .and_then(|s| s.parse::<usize>().ok())
                    .ok_or_else(|| ApiError::invalid("Invalid pageToken"))?,
                None => 0,
            };
            let after = request
                .status_timestamp_after
                .as_deref()
                .map(chrono::DateTime::parse_from_rfc3339)
                .transpose()
                .map_err(|_| ApiError::invalid("Invalid statusTimestampAfter"))?;
            let tasks = gateway.store.tasks().await?;
            let filtered: Vec<_> = tasks
                .into_iter()
                .filter(|task| {
                    request
                        .context_id
                        .as_ref()
                        .is_none_or(|id| id == task.context_id.as_str())
                        && request.status.is_none_or(|s| s == task.status.state)
                        && after.as_ref().is_none_or(|after| {
                            task.status
                                .timestamp
                                .as_deref()
                                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                                .is_some_and(|t| t >= *after)
                        })
                })
                .collect();
            if offset > filtered.len() {
                return Err(ApiError::invalid("pageToken is outside the result set"));
            }
            let total = filtered.len();
            let tasks = filtered
                .into_iter()
                .skip(offset)
                .take(size as usize)
                .map(|mut t| {
                    trim_history(&mut t, request.history_length);
                    if request.include_artifacts != Some(true) {
                        t.artifacts.clear();
                    }
                    t
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
                page_size: size,
                total_size: total as i32,
            };
            Ok(success(
                id,
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

async fn stream(
    gateway: Arc<Gateway>,
    id: Value,
    task_id: String,
    cursor: Option<String>,
    history: Option<i32>,
) -> ApiResult<Response> {
    let mut notifications = gateway.subscribe();
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
    // A2A subscriptions always start with the current Task. Reconnection replaces the
    // client's previous snapshot rather than replaying deltas already present in it.
    let (mut task, mut after) = gateway
        .store
        .snapshot(&task_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    trim_history(&mut task, history);
    let initial_terminal = task.status.state.is_terminal();
    let output = async_stream::stream! {
        yield Ok::<Event, Infallible>(sse_event(&id, &task_id, after, StreamResponse::Task(task)));
        if initial_terminal { return; }
        loop {
            match gateway.store.events(&task_id, after).await {
                Ok(events) if !events.is_empty() => {
                    for (seq, mut event) in events {
                        after = seq;
                        if let StreamResponse::Task(task) = &mut event { trim_history(task, history); }
                        let terminal = event.is_terminal();
                        yield Ok(sse_event(&id, &task_id, seq, event));
                        if terminal { return; }
                    }
                }
                Ok(_) => {
                    tokio::select! { _ = notifications.recv() => {}, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
                }
                Err(error) => {
                    tracing::error!(error = %error, "SSE event read failed");
                    yield Ok(Event::default().data(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"Event stream storage error"}}).to_string()));
                    return;
                }
            }
        }
    };
    Ok(Sse::new(output)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

fn sse_event(id: &Value, task_id: &str, seq: i64, event: StreamResponse) -> Event {
    Event::default()
        .id(format!("{task_id}:{seq}"))
        .data(json!({"jsonrpc":"2.0","id":id,"result":event}).to_string())
}

fn has_interaction_extension(headers: &HeaderMap) -> bool {
    headers
        .get("a2a-extensions")
        .and_then(|s| s.to_str().ok())
        .is_some_and(|s| s.split(',').any(|s| s.trim() == INTERACTION_EXTENSION))
}
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
fn success(id: &Value, value: Value) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"result":value})).into_response()
}
fn error_response(id: Value, error: ApiError) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":error.code,"message":error.message}}))
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
