//! 验证教学日志能串起真实协议流程，且不会记录消息、输出正文或环境凭据。

use std::{
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use a2alinker::{
    a2a,
    options::{AgentKind, Options},
    runtime::Gateway,
};
use axum::{Router, body::Body, http::Request, response::Response};
use http_body_util::BodyExt;
use ra2a::types::{Message, TaskState};
use serde_json::{Value, json};
use tower::ServiceExt;

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn request(router: &Router, method: &str, params: Value) -> Response {
    router
        .clone()
        .oneshot(
            Request::post("/a2a")
                .header("Content-Type", "application/json")
                .header("A2A-Extensions", "urn:a2alinker:interaction:v1")
                .body(Body::from(
                    json!({"jsonrpc":"2.0","id":"learning-request","method":method,"params":params})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body(response: Response) -> String {
    let bytes = tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn learning_logs_explain_lifecycle_dedup_interaction_and_disconnect_without_payloads() {
    // 该集成测试二进制只有此用例，全局订阅器也能捕获 Tokio 后台 worker 的日志。
    let writer = LogWriter(Arc::new(Mutex::new(Vec::new())));
    let captured = writer.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_env_filter("a2alinker=debug")
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    )
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = Options::default();
    cfg.agent.kind = AgentKind::Pi;
    #[cfg(unix)]
    {
        cfg.agent.prepare = Some(
            "printf 'PREPARE_MUST_STAY_PRIVATE'; export HTTP_PROXY=http://127.0.0.1:7890".into(),
        );
    }
    cfg.agent.executable =
        Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent.py"));
    cfg.agent.workspace = temp.path().into();
    cfg.agent.env.insert(
        "PI_CODING_AGENT_DIR".into(),
        temp.path().join("user-pi").display().to_string(),
    );
    cfg.agent
        .env
        .insert("FAKE_API_KEY".into(), "CREDENTIAL_MUST_STAY_PRIVATE".into());
    cfg.runtime.cancel_timeout_seconds = 1;
    cfg.storage.directory = temp.path().join("storage");
    let gateway = Gateway::open(cfg).await.unwrap();
    let router = a2a::router(gateway.clone()).unwrap();
    router
        .clone()
        .oneshot(
            Request::get("/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let params = json!({"message":{
        "messageId":"learning-message-1","role":"ROLE_USER",
        "parts":[{"text":"PROMPT_MUST_STAY_PRIVATE"}],
        "metadata":{"private":"METADATA_MUST_STAY_PRIVATE"}
    }});
    let stream = body(request(&router, "SendStreamingMessage", params.clone()).await).await;
    let snapshot: Value = serde_json::from_str(
        stream
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap();
    let task_id = snapshot["result"]["task"]["id"].as_str().unwrap();
    let context_id = snapshot["result"]["task"]["contextId"].as_str().unwrap();
    assert!(stream.contains("TASK_STATE_COMPLETED"));
    let duplicate = body(request(&router, "SendStreamingMessage", params).await).await;
    assert!(duplicate.contains(task_id));

    let next: Value = serde_json::from_str(
        &body(
            request(
                &router,
                "SendMessage",
                json!({"message":{
                    "messageId":"learning-message-2","role":"ROLE_USER","contextId":context_id,
                    "parts":[{"text":"NEXT_PROMPT_MUST_STAY_PRIVATE"}]
                }}),
            )
            .await,
        )
        .await,
    )
    .unwrap();
    assert_eq!(
        next["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );

    let execution = gateway
        .submit(Message::user_text("execution"))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(5),
            gateway.wait(execution.id.as_str(), true)
        )
        .await
        .unwrap()
        .unwrap()
        .status
        .state,
        TaskState::Completed
    );

    let approval = gateway
        .submit(Message::user_text("approval").with_context_id(context_id))
        .await
        .unwrap();
    let pending = tokio::time::timeout(
        Duration::from_secs(5),
        gateway.wait(approval.id.as_str(), true),
    )
    .await
    .unwrap()
    .unwrap();
    let interaction = &pending.metadata.as_ref().unwrap()["pendingInteraction"]["id"];
    let reply: Value = serde_json::from_str(
        &body(
            request(
                &router,
                "SendMessage",
                json!({"message":{
                    "messageId":"learning-reply","role":"ROLE_USER","taskId":approval.id,
                    "parts":[{"data":{"interactionId":interaction,"response":{"confirmed":true}}}]
                }}),
            )
            .await,
        )
        .await,
    )
    .unwrap();
    assert_eq!(
        reply["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );

    let failed = gateway.submit(Message::user_text("fail")).await.unwrap();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(5),
            gateway.wait(failed.id.as_str(), true)
        )
        .await
        .unwrap()
        .unwrap()
        .status
        .state,
        TaskState::Failed
    );

    let disconnected = request(
        &router,
        "SendStreamingMessage",
        json!({"message":{
            "messageId":"learning-disconnect","role":"ROLE_USER","parts":[{"text":"slow"}]
        }}),
    )
    .await;
    // 不轮询响应体就丢弃连接，也必须释放日志守卫，同时保留后台任务。
    drop(disconnected);
    let slow = gateway
        .store
        .tasks()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.history[0].id.as_str() == "learning-disconnect")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while gateway.task(slow.id.as_str()).await.unwrap().status.state != TaskState::Working {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            gateway.cancel(slow.id.as_str()).await.unwrap().status.state,
            TaskState::Canceled
        );
    })
    .await
    .unwrap();
    body(request(&router, "GetTask", json!({"id":task_id})).await).await;
    body(request(&router, "ListTasks", json!({"pageSize":10})).await).await;
    let missing: Value = serde_json::from_str(
        &body(request(&router, "GetTask", json!({"id":"missing"})).await).await,
    )
    .unwrap();
    assert_eq!(missing["error"]["code"], -32001);
    gateway.shutdown().await;

    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    for event in [
        "a2a.discovery",
        "a2a.request",
        "a2a.response",
        "task.queued",
        "task.started",
        "message.duplicate",
        "session.ready",
        "session.reused",
        "artifact.started",
        "artifact.complete",
        "artifact.delta",
        "tool.execution",
        "sse.opened",
        "sse.event",
        "sse.closed",
        "interaction.pending",
        "interaction.reply",
        "task.cancel_requested",
        "task.finished",
        "task.query",
        "task.list",
        "a2a.error",
        "gateway.shutdown",
    ] {
        assert!(logs.contains(event), "missing log event: {event}");
    }
    for identifier in [
        "learning-request",
        "learning-message-1",
        task_id,
        context_id,
        "client_disconnected",
        "task_terminal",
    ] {
        assert!(
            logs.contains(identifier),
            "missing log correlation: {identifier}"
        );
    }
    for private in [
        "PROMPT_MUST_STAY_PRIVATE",
        "NEXT_PROMPT_MUST_STAY_PRIVATE",
        "METADATA_MUST_STAY_PRIVATE",
        "CREDENTIAL_MUST_STAY_PRIVATE",
        "Hello 1",
        "检查仓库结构",
        "ls -la",
        "文件A",
        "SIGNATURE_MUST_NOT_BE_FORWARDED",
        "TOOL_SECRET_MUST_BE_REDACTED",
        "RESULT_SECRET_MUST_BE_REDACTED",
        "PREPARE_MUST_STAY_PRIVATE",
    ] {
        assert!(!logs.contains(private), "private content leaked into logs");
    }
}
