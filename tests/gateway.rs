//! 使用独立子进程测试完整任务生命周期，不依赖真实模型，也不消耗推理额度。
//! 重点覆盖去重、会话恢复、交互回复、SSE 和异常情况下的资源清理。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use a2alinker::{
    a2a,
    config::{AgentKind, Config},
    runtime::{Gateway, INTERACTION_EXTENSION},
    storage::Store,
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use ra2a::types::{Message, Part, StreamResponse, Task, TaskState};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent.py")
}

fn config(temp: &TempDir, kind: AgentKind) -> Config {
    let mut cfg = Config {
        schema_version: 1,
        ..Config::default()
    };
    cfg.agent.kind = kind;
    cfg.agent.executable = Some(fixture());
    cfg.agent.workspace = temp.path().into();
    cfg.agent.inherit_auth = false;
    cfg.agent.env.insert(
        "PI_CODING_AGENT_DIR".into(),
        temp.path().join("user-pi").display().to_string(),
    );
    cfg.storage.directory = temp.path().join("data");
    cfg.runtime.startup_timeout_seconds = 5;
    cfg.runtime.cancel_timeout_seconds = 1;
    cfg.runtime.task_timeout_seconds = 5;
    cfg
}

async fn gateway(temp: &TempDir, kind: AgentKind) -> Arc<Gateway> {
    Gateway::open(config(temp, kind)).await.unwrap()
}

async fn rpc(router: &Router, method: &str, params: Value) -> Value {
    let request = Request::post("/a2a")
        .header("Content-Type", "application/json")
        .header("A2A-Version", "1.0")
        .header("A2A-Extensions", INTERACTION_EXTENSION)
        .body(Body::from(
            json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string(),
        ))
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(10), router.clone().oneshot(request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

fn prompt(text: &str) -> Value {
    json!({"message":{"messageId":uuid::Uuid::new_v4().to_string(),"role":"ROLE_USER","parts":[{"text":text}]}})
}

async fn complete(g: &Gateway, message: Message) -> Task {
    let task = g.submit(message).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), g.wait(task.id.as_str(), true))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn both_adapters_complete_without_duplicating_streamed_text() {
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let temp = tempfile::tempdir().unwrap();
        let g = gateway(&temp, kind).await;
        let task = complete(&g, Message::user_text("hello")).await;
        assert_eq!(task.status.state, TaskState::Completed, "{:?}", task.status);
        assert_eq!(task.artifacts.len(), 1);
        assert_eq!(task.artifacts[0].parts[0].as_text(), Some("Hello 1"));
        let context = task.context_id.clone();
        let next = complete(
            &g,
            Message::user_text("follow up").with_context_id(context.as_str()),
        )
        .await;
        assert_eq!(next.artifacts[0].parts[0].as_text(), Some("Hello 2"));
        g.shutdown().await;
    }
}

#[tokio::test]
async fn persistence_and_native_session_resume_survive_restart() {
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let temp = tempfile::tempdir().unwrap();
        let cfg = config(&temp, kind);
        let g = Gateway::open(cfg.clone()).await.unwrap();
        let task = complete(&g, Message::user_text("hello")).await;
        g.shutdown().await;
        drop(g);
        let g = Gateway::open(cfg).await.unwrap();
        assert_eq!(
            g.task(task.id.as_str()).await.unwrap().status.state,
            TaskState::Completed
        );
        let next = complete(
            &g,
            Message::user_text("again").with_context_id(task.context_id.as_str()),
        )
        .await;
        assert_eq!(next.status.state, TaskState::Completed);
        assert_eq!(next.artifacts[0].parts[0].as_text(), Some("Hello 2"));
        g.shutdown().await;
    }
}

#[tokio::test]
async fn duplicate_messages_are_atomic_and_conflicts_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let g = gateway(&temp, AgentKind::Pi).await;
    let message = Message::user_text("hello");
    let (a, b) = tokio::join!(g.submit(message.clone()), g.submit(message.clone()));
    assert_eq!(a.unwrap().id, b.unwrap().id);
    let mut changed = message;
    changed.parts = vec![Part::text("different")];
    assert_eq!(g.submit(changed).await.unwrap_err().code, -32602);
    assert_eq!(g.store.tasks().await.unwrap().len(), 1);
    g.shutdown().await;
}

#[tokio::test]
async fn queued_and_running_tasks_cancel_and_contexts_remain_exclusive() {
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let temp = tempfile::tempdir().unwrap();
        let g = gateway(&temp, kind).await;
        let task = g.submit(Message::user_text("slow")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            g.submit(Message::user_text("overlap").with_context_id(task.context_id.as_str()))
                .await
                .unwrap_err()
                .code,
            -32004
        );
        let queued = g.submit(Message::user_text("hello")).await.unwrap();
        let canceled =
            tokio::time::timeout(Duration::from_millis(300), g.cancel(queued.id.as_str()))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);
        let canceled = tokio::time::timeout(Duration::from_secs(3), g.cancel(task.id.as_str()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);
        assert_eq!(
            g.task(queued.id.as_str()).await.unwrap().status.state,
            TaskState::Canceled
        );
        g.shutdown().await;
    }
}

#[tokio::test]
async fn approval_round_trip_validates_ids_and_survives_duplicate_replies() {
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let temp = tempfile::tempdir().unwrap();
        let g = gateway(&temp, kind).await;
        let router = a2a::router(g.clone()).unwrap();
        let response = rpc(&router, "SendMessage", prompt("approval")).await;
        assert!(response.get("error").is_none(), "{response}");
        let task: Task = serde_json::from_value(response["result"]["task"].clone()).unwrap();
        assert_eq!(task.status.state, TaskState::InputRequired);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            g.task(task.id.as_str()).await.unwrap().status.state,
            TaskState::InputRequired
        );
        let interaction = &task.metadata.as_ref().unwrap()["pendingInteraction"];
        let reply = if kind == AgentKind::Pi {
            json!({"confirmed":true})
        } else {
            json!({"decision":"accept"})
        };
        let mut message = Message::user(vec![Part::data(
            json!({"interactionId":"wrong","response":reply}),
        )])
        .with_task_id(task.id.clone());
        assert_eq!(g.submit(message.clone()).await.unwrap_err().code, -32602);
        message.parts = vec![Part::data(
            json!({"interactionId":interaction["id"],"response":reply}),
        )];
        let response = rpc(&router, "SendMessage", json!({"message":message})).await;
        assert_eq!(
            response["result"]["task"]["status"]["state"], "TASK_STATE_COMPLETED",
            "{response}"
        );
        let duplicate = g.submit(message).await.unwrap();
        assert_eq!(duplicate.id, task.id);
        g.shutdown().await;
    }
}

#[tokio::test]
async fn pi_waits_through_retry_and_preserves_unicode_line_separators() {
    let temp = tempfile::tempdir().unwrap();
    let g = gateway(&temp, AgentKind::Pi).await;
    let task = complete(&g, Message::user_text("retry")).await;
    assert_eq!(task.status.state, TaskState::Completed);
    assert_eq!(
        task.artifacts.last().unwrap().parts[0].as_text(),
        Some("Hello 1")
    );
    let task = complete(&g, Message::user_text("unicode")).await;
    assert_eq!(task.status.state, TaskState::Completed);
    assert_eq!(
        task.artifacts[0].parts[0].as_text(),
        Some("hello\u{2028}world")
    );
    g.shutdown().await;
}

#[tokio::test]
async fn task_failures_crashes_and_bad_frames_do_not_poison_next_tasks() {
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let temp = tempfile::tempdir().unwrap();
        let g = gateway(&temp, kind).await;
        for prompt in ["fail", "crash", "malformed"] {
            let failed = complete(&g, Message::user_text(prompt)).await;
            assert_eq!(failed.status.state, TaskState::Failed, "{prompt}");
            let next = complete(
                &g,
                Message::user_text("hello").with_context_id(failed.context_id.as_str()),
            )
            .await;
            assert_eq!(
                next.status.state,
                TaskState::Completed,
                "{prompt}: {:?}",
                next.status
            );
        }
        g.shutdown().await;
    }
}

#[tokio::test]
async fn timeout_also_interrupts_an_unacknowledged_command() {
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = config(&temp, AgentKind::Codex);
    cfg.runtime.task_timeout_seconds = 1;
    let g = Gateway::open(cfg).await.unwrap();
    let task = complete(&g, Message::user_text("hang-ack")).await;
    assert_eq!(task.status.state, TaskState::Failed);
    assert!(
        task.status
            .message
            .unwrap()
            .text_content()
            .unwrap()
            .contains("timed out")
    );
    g.shutdown().await;
}

#[tokio::test]
async fn output_limit_fails_the_task_and_releases_the_session() {
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = config(&temp, AgentKind::Pi);
    cfg.runtime.max_output_bytes = 1024;
    let g = Gateway::open(cfg).await.unwrap();
    let failed = complete(&g, Message::user_text("large")).await;
    assert_eq!(failed.status.state, TaskState::Failed);
    let next = complete(
        &g,
        Message::user_text("hello").with_context_id(failed.context_id.as_str()),
    )
    .await;
    assert_eq!(next.status.state, TaskState::Completed);
    g.shutdown().await;
}

#[tokio::test]
async fn a2a_wire_protocol_streams_and_supports_task_queries() {
    let temp = tempfile::tempdir().unwrap();
    let g = gateway(&temp, AgentKind::Codex).await;
    let router = a2a::router(g.clone()).unwrap();
    let card = router
        .clone()
        .oneshot(
            Request::get("/.well-known/agent-card.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let card: ra2a::types::AgentCard =
        serde_json::from_slice(&card.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(card.supported_interfaces[0].protocol_version, "1.0");
    let response = router.clone().oneshot(Request::post("/a2a").header("Content-Type", "application/json")
        .body(Body::from(json!({"jsonrpc":"2.0","id":"stream","method":"SendStreamingMessage","params":prompt("hello")}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.headers()["Content-Type"], "text/event-stream");
    let body = tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).unwrap();
    let frames: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    let task_id = frames[0]["result"]["task"]["id"].as_str().unwrap();
    for frame in &frames {
        let _: StreamResponse = serde_json::from_value(frame["result"].clone()).unwrap();
    }
    assert_eq!(
        frames.last().unwrap()["result"]["statusUpdate"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    let got = rpc(&router, "GetTask", json!({"id":task_id,"historyLength":0})).await;
    assert!(got["result"].get("history").is_none());
    assert_eq!(got["result"]["artifacts"][0]["parts"][0]["text"], "Hello 1");
    let list = rpc(&router, "ListTasks", json!({"pageSize":1})).await;
    assert_eq!(list["result"]["totalSize"], 1);
    assert!(list["result"]["tasks"][0].get("artifacts").is_none());
    assert_eq!(
        rpc(&router, "CancelTask", json!({"id":task_id})).await["error"]["code"],
        -32002
    );
    assert_eq!(
        rpc(&router, "GetTask", json!({"id":"missing"})).await["error"]["code"],
        -32001
    );
    assert_eq!(
        rpc(&router, "message/send", prompt("hello")).await["error"]["code"],
        -32601
    );
    g.shutdown().await;
}

#[tokio::test]
async fn subscriptions_start_with_an_atomic_snapshot_even_with_a_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let g = gateway(&temp, AgentKind::Pi).await;
    let task = g.submit(Message::user_text("slow")).await.unwrap();
    let cursor = g.store.latest_event(task.id.as_str()).await.unwrap();
    let router = a2a::router(g.clone()).unwrap();
    let response = router.clone().oneshot(Request::post("/a2a").header("Content-Type", "application/json")
        .header("Last-Event-ID", format!("{}:{cursor}", task.id))
        .body(Body::from(json!({"jsonrpc":"2.0","id":1,"method":"SubscribeToTask","params":{"id":task.id}}).to_string())).unwrap()).await.unwrap();
    let mut body = response.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = std::str::from_utf8(frame.data_ref().unwrap()).unwrap();
    assert!(text.contains("\"task\":"), "{text}");
    drop(body);
    assert_eq!(
        g.cancel(task.id.as_str()).await.unwrap().status.state,
        TaskState::Canceled
    );
    g.shutdown().await;
}

#[tokio::test]
async fn restart_marks_unfinished_tasks_failed_without_replaying_them() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = config(&temp, AgentKind::Codex);
    let store = Store::open(&cfg.storage.directory).await.unwrap();
    let mut task = Task::new("unfinished", "context");
    task.status.state = TaskState::Working;
    store
        .insert_task(&task, "original-message", "digest", "config")
        .await
        .unwrap();
    store.close().await;
    drop(store);
    let g = Gateway::open(cfg).await.unwrap();
    let recovered = g.task("unfinished").await.unwrap();
    assert_eq!(recovered.status.state, TaskState::Failed);
    assert!(
        recovered
            .status
            .message
            .unwrap()
            .text_content()
            .unwrap()
            .contains("not replayed")
    );
    g.shutdown().await;
}

#[tokio::test]
async fn a_storage_directory_cannot_be_shared_by_two_instances() {
    let temp = tempfile::tempdir().unwrap();
    let first = Store::open(temp.path()).await.unwrap();
    assert!(Store::open(temp.path()).await.is_err());
    first.close().await;
}

#[tokio::test]
async fn malformed_and_unsupported_requests_have_explicit_errors() {
    let temp = tempfile::tempdir().unwrap();
    let g = gateway(&temp, AgentKind::Codex).await;
    let router = a2a::router(g.clone()).unwrap();
    assert_eq!(rpc(&router, "SendMessage", json!({"message":{"messageId":"x","role":"ROLE_USER","parts":[{"url":"http://example.com/image"}]}})).await["error"]["code"], -32005);
    assert_eq!(rpc(&router, "SendMessage", json!({"message":{"messageId":"x","role":"ROLE_USER","parts":[{"text":"hello"}]},"configuration":{"historyLength":-1}})).await["error"]["code"], -32602);
    assert_eq!(
        rpc(&router, "ListTasks", json!({"pageSize":101})).await["error"]["code"],
        -32602
    );
    assert_eq!(
        rpc(&router, "GetExtendedAgentCard", json!({})).await["error"]["code"],
        -32004
    );
    assert_eq!(
        rpc(&router, "CreateTaskPushNotificationConfig", json!({})).await["error"]["code"],
        -32003
    );
    let response = router
        .oneshot(
            Request::post("/a2a")
                .header("Content-Type", "text/plain")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    g.shutdown().await;
}

#[tokio::test]
async fn selected_skills_are_loaded_by_each_adapter_before_accepting_tasks() {
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let temp = tempfile::tempdir().unwrap();
        let mut cfg = config(&temp, kind);
        cfg.skills.directories =
            vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/skills")];
        cfg.skills.enabled = vec!["code-review".into()];
        let g = Gateway::open(cfg).await.unwrap();
        assert_eq!(g.skills.skills.len(), 1);
        let task = complete(&g, Message::user_text("hello")).await;
        assert_eq!(task.status.state, TaskState::Completed);
        g.shutdown().await;
    }
}

#[tokio::test]
async fn evicted_sessions_resume_and_configuration_changes_require_a_new_context() {
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let temp = tempfile::tempdir().unwrap();
        let mut cfg = config(&temp, kind);
        cfg.runtime.max_sessions = 1;
        let g = Gateway::open(cfg.clone()).await.unwrap();
        let first = complete(&g, Message::user_text("hello")).await;
        complete(&g, Message::user_text("hello")).await;
        let resumed = complete(
            &g,
            Message::user_text("hello").with_context_id(first.context_id.as_str()),
        )
        .await;
        assert_eq!(resumed.artifacts[0].parts[0].as_text(), Some("Hello 2"));
        g.shutdown().await;
        drop(g);
        cfg.agent.model = Some("different-model".into());
        let g = Gateway::open(cfg).await.unwrap();
        assert_eq!(
            g.submit(Message::user_text("hello").with_context_id(first.context_id.as_str()))
                .await
                .unwrap_err()
                .code,
            -32602
        );
        assert_eq!(
            complete(&g, Message::user_text("hello")).await.status.state,
            TaskState::Completed
        );
        g.shutdown().await;
    }
}

#[tokio::test]
async fn bounded_queue_rejects_overflow_without_persisting_a_phantom_task() {
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = config(&temp, AgentKind::Codex);
    cfg.runtime.queue_capacity = 1;
    let g = Gateway::open(cfg).await.unwrap();
    let running = g.submit(Message::user_text("slow")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let queued = g.submit(Message::user_text("hello")).await.unwrap();
    assert_eq!(
        g.submit(Message::user_text("overflow"))
            .await
            .unwrap_err()
            .code,
        -32004
    );
    assert_eq!(g.store.tasks().await.unwrap().len(), 2);
    g.cancel(queued.id.as_str()).await.unwrap();
    g.cancel(running.id.as_str()).await.unwrap();
    g.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn process_group_cleanup_kills_tools_even_after_agent_process_exits() {
    use nix::{
        errno::Errno,
        sys::signal::{Signal, kill},
        unistd::Pid,
    };

    let temp = tempfile::tempdir().unwrap();
    let mut cfg = config(&temp, AgentKind::Pi);
    let pid_file = temp.path().join("child.pid");
    cfg.agent.env.insert(
        "A2ALINKER_TEST_CHILD_PID".into(),
        pid_file.to_str().unwrap().into(),
    );
    let g = Gateway::open(cfg).await.unwrap();
    let task = complete(&g, Message::user_text("crash-with-child")).await;
    assert_eq!(task.status.state, TaskState::Failed);
    let pid: i32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
    assert!(pid > 1, "fixture must provide an ordinary child PID");
    let child_pid = Pid::from_raw(pid);
    let mut stopped = false;
    for _ in 0..50 {
        // None 只检查进程是否存在，不实际发送信号；ESRCH 表示进程已不存在。
        // 其他错误（例如无权访问）不能被误判为清理成功。
        if kill(child_pid, None) == Err(Errno::ESRCH) {
            stopped = true;
            break;
        }
        let status = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        // 僵尸进程已经停止运行，可能还在等待操作系统回收记录。
        if String::from_utf8_lossy(&status.stdout)
            .trim_start()
            .starts_with('Z')
        {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if !stopped {
        // 断言失败前也尽量清理测试进程，避免给后续测试留下后台进程。
        let _ = kill(child_pid, Signal::SIGKILL);
    }
    assert!(stopped, "agent tool process survived process group cleanup");
    g.shutdown().await;
}
