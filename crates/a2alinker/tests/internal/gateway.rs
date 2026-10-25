//! 使用独立子进程测试完整任务生命周期，不依赖真实模型，也不消耗推理额度。
//! 重点覆盖去重、会话恢复、交互回复、SSE 和异常情况下的资源清理。

use crate as a2alinker;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use a2alinker::{
    a2a,
    options::{AgentKind, Options},
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

fn options(temp: &TempDir, kind: AgentKind) -> Options {
    let mut cfg = Options::default();
    cfg.agent.kind = kind;
    cfg.agent.executable = Some(fixture());
    cfg.agent.workspace = temp.path().into();
    cfg.agent.env.insert(
        "PI_CODING_AGENT_DIR".into(),
        temp.path().join("user-pi").display().to_string(),
    );
    cfg.agent.env.insert(
        "CODEX_HOME".into(),
        temp.path().join("user-codex").display().to_string(),
    );
    cfg.storage.directory = temp.path().join("data");
    cfg.runtime.startup_timeout_seconds = 5;
    cfg.runtime.cancel_timeout_seconds = 1;
    cfg.runtime.task_timeout_seconds = 5;
    cfg
}

async fn gateway(temp: &TempDir, kind: AgentKind) -> Arc<Gateway> {
    Gateway::open(options(temp, kind)).await.unwrap()
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
        // 两种 agent 都能在未声明具体能力、未选择本地技能包时正常执行任务。
        let card = serde_json::to_value(a2a::agent_card(&g)).unwrap();
        assert_eq!(card["skills"].as_array().unwrap().len(), 1);
        assert_eq!(card["skills"][0]["id"], "dummy");
        assert!(
            !card["skills"][0]["description"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        assert!(!card["skills"][0]["tags"].as_array().unwrap().is_empty());
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
async fn pi_execution_stream_preserves_reasoning_tools_arguments_results_and_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = options(&temp, AgentKind::Pi);
    let g = Gateway::open(cfg.clone()).await.unwrap();
    let router = a2a::router(g.clone()).unwrap();
    let card = serde_json::to_value(a2a::agent_card(&g)).unwrap();
    let extension = card["capabilities"]["extensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|extension| extension["uri"] == "urn:a2alinker:execution:v1")
        .unwrap();
    assert_eq!(extension["params"], json!({"reasoning":true,"tools":true}));
    let request = Request::post("/a2a").header("Content-Type", "application/json")
        .body(Body::from(json!({"jsonrpc":"2.0","id":"execution-stream","method":"SendStreamingMessage","params":prompt("execution")}).to_string())).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.headers()["Content-Type"], "text/event-stream");
    let body = tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    let body = String::from_utf8(body.to_vec()).unwrap();
    for private in [
        "SIGNATURE_MUST_NOT_BE_FORWARDED",
        "TOOL_SECRET_MUST_BE_REDACTED",
        "RESULT_SECRET_MUST_BE_REDACTED",
    ] {
        assert!(!body.contains(private));
    }
    let frames: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str::<Value>(data).unwrap()["result"].clone())
        .collect();
    for frame in &frames {
        let _: StreamResponse = serde_json::from_value(frame.clone()).unwrap();
    }
    let id = frames[0]["task"]["id"].as_str().unwrap();
    let task = g.task(id).await.unwrap();
    assert_eq!(task.status.state, TaskState::Completed);
    assert_eq!(
        task.artifacts.len(),
        3,
        "no empty answer artifact for tool-only message"
    );
    assert_eq!(task.artifacts[0].parts[0].as_text(), Some("检查仓库结构。"));
    assert_eq!(task.artifacts[1].parts[0].as_text(), Some("再查看文件。"));
    assert_eq!(task.artifacts[2].parts[0].as_text(), Some("Hello 1"));
    assert_eq!(
        task.history.last().unwrap().text_content().unwrap(),
        "Hello 1"
    );
    assert_eq!(
        task.history.len(),
        4,
        "one user, two tool snapshots and one answer"
    );

    // 按 A2A append/lastChunk 语义组装客户端输出，验证最终块不会造成重复文本。
    let mut artifacts = std::collections::HashMap::<String, String>::new();
    for frame in &frames {
        if let Some(snapshot) = frame["task"]["artifacts"].as_array() {
            for artifact in snapshot {
                artifacts.insert(
                    artifact["artifactId"].as_str().unwrap().into(),
                    artifact["parts"][0]["text"].as_str().unwrap().into(),
                );
            }
        }
        if let Some(update) = frame.get("artifactUpdate") {
            let id = update["artifact"]["artifactId"]
                .as_str()
                .unwrap()
                .to_string();
            let text = update["artifact"]["parts"][0]["text"].as_str().unwrap();
            if update["append"] == true {
                artifacts.entry(id).or_default().push_str(text);
            } else {
                artifacts.insert(id, text.into());
            }
            assert!(matches!(
                update["artifact"]["metadata"]["a2alinker"]["kind"].as_str(),
                Some("reasoning" | "answer")
            ));
        }
    }
    for artifact in &task.artifacts {
        assert_eq!(
            artifacts[artifact.artifact_id.as_str()],
            artifact.parts[0].as_text().unwrap()
        );
    }

    // 从数据库验证完整生命周期；首次 SSE 快照之前的事件也不会影响断言。
    let events = g.store.events(id, 0).await.unwrap();
    let records: Vec<Value> = events
        .iter()
        .filter_map(|(_, event)| {
            let value = serde_json::to_value(event).unwrap();
            value["statusUpdate"]["status"]["message"]["parts"]
                .as_array()?
                .iter()
                .find_map(|part| {
                    (part["data"]["a2alinker"]["kind"] == "tool")
                        .then(|| part["data"]["a2alinker"].clone())
                })
        })
        .collect();
    assert_eq!(records.len(), 6);
    assert_eq!(records[0]["type"], "tool_call");
    assert_eq!(records[0]["arguments"]["command"], "ls -la");
    assert_eq!(
        records[0]["arguments"]["headers"]["Authorization"],
        "[REDACTED]"
    );
    assert_eq!(records[2]["result"]["content"][0]["text"], "文件A\n");
    assert_eq!(records[3]["result"]["content"][0]["text"], "文件A\n文件B\n");
    assert_eq!(records[4]["toolCallId"], "call-read");
    assert_eq!(records[4]["state"], "failed");
    assert_eq!(records[4]["isError"], true);
    assert_eq!(records[5]["toolCallId"], "call-bash");
    assert_eq!(
        records[5]["arguments"]["command"], "ls -la",
        "end event preserves start arguments"
    );
    assert_eq!(records[5]["result"]["details"]["exitCode"], 0);
    assert_eq!(records[5]["result"]["details"]["api_key"], "[REDACTED]");
    let queried = rpc(&router, "GetTask", json!({"id":id})).await;
    assert_eq!(queried["result"], serde_json::to_value(&task).unwrap());
    g.shutdown().await;
    drop(router);
    drop(g);
    let restarted = Gateway::open(cfg).await.unwrap();
    assert_eq!(restarted.task(id).await.unwrap(), task);
    restarted.shutdown().await;
}

#[tokio::test]
async fn pi_execution_forwarding_switches_filter_stream_and_storage_independently() {
    for (reasoning, tools) in [(false, false), (false, true), (true, false)] {
        let temp = tempfile::tempdir().unwrap();
        let mut cfg = options(&temp, AgentKind::Pi);
        cfg.runtime.forward_reasoning = reasoning;
        cfg.runtime.forward_tools = tools;
        let g = Gateway::open(cfg).await.unwrap();
        let task = complete(&g, Message::user_text("execution")).await;
        assert_eq!(task.status.state, TaskState::Completed);
        assert_eq!(task.artifacts.len(), if reasoning { 3 } else { 1 });
        assert_eq!(task.history.len(), if tools { 4 } else { 2 });
        let events =
            serde_json::to_string(&g.store.events(task.id.as_str(), 0).await.unwrap()).unwrap();
        assert_eq!(events.contains("检查仓库结构"), reasoning);
        assert_eq!(events.contains("toolCallId"), tools);
        assert!(!events.contains("SIGNATURE_MUST_NOT_BE_FORWARDED"));
        g.shutdown().await;
    }
}

#[tokio::test]
async fn pi_running_tool_snapshot_survives_sse_reconnect_and_disconnect() {
    let temp = tempfile::tempdir().unwrap();
    let g = gateway(&temp, AgentKind::Pi).await;
    let task = g
        .submit(Message::user_text("execution-pause"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = serde_json::to_value(g.task(task.id.as_str()).await.unwrap()).unwrap();
            if snapshot["history"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["parts"].as_array().unwrap().iter().any(|part| {
                        part["data"]["a2alinker"]["result"]["content"][0]["text"]
                            == "文件A\n文件B\n"
                    })
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let router = a2a::router(g.clone()).unwrap();
    let response = router.oneshot(Request::post("/a2a").header("Content-Type", "application/json")
        .body(Body::from(json!({"jsonrpc":"2.0","id":"reconnect","method":"SubscribeToTask","params":{"id":task.id}}).to_string())).unwrap()).await.unwrap();
    let mut body = response.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = std::str::from_utf8(frame.data_ref().unwrap()).unwrap();
    let snapshot: Value = serde_json::from_str(
        text.lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        snapshot["result"]["task"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    assert_eq!(
        snapshot["result"]["task"]["artifacts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let history = snapshot["result"]["task"]["history"].as_array().unwrap();
    let records: Vec<&Value> = history
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .filter_map(|part| part.get("data"))
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["a2alinker"]["toolCallId"], "call-bash");
    assert_eq!(records[0]["a2alinker"]["arguments"]["command"], "ls -la");
    assert_eq!(
        records[0]["a2alinker"]["result"]["content"][0]["text"],
        "文件A\n文件B\n"
    );
    drop(body);
    assert_eq!(
        g.task(task.id.as_str()).await.unwrap().status.state,
        TaskState::Working
    );
    assert_eq!(
        g.cancel(task.id.as_str()).await.unwrap().status.state,
        TaskState::Canceled
    );
    g.shutdown().await;
}

#[tokio::test]
async fn pi_execution_content_counts_toward_output_limit_and_releases_session() {
    for prompt in ["reasoning-large", "arguments-large", "tool-large"] {
        let temp = tempfile::tempdir().unwrap();
        let mut cfg = options(&temp, AgentKind::Pi);
        cfg.runtime.max_output_bytes = 1024;
        let g = Gateway::open(cfg).await.unwrap();
        let failed = complete(&g, Message::user_text(prompt)).await;
        assert_eq!(failed.status.state, TaskState::Failed, "{prompt}");
        assert!(
            failed
                .status
                .message
                .as_ref()
                .unwrap()
                .text_content()
                .unwrap()
                .contains("max_output_bytes")
        );
        let next = complete(
            &g,
            Message::user_text("hello").with_context_id(failed.context_id.as_str()),
        )
        .await;
        assert_eq!(next.status.state, TaskState::Completed);
        let persisted =
            serde_json::to_string(&g.store.events(failed.id.as_str(), 0).await.unwrap()).unwrap();
        assert!(
            !persisted.contains(&"x".repeat(2048)),
            "oversized event must not be persisted"
        );
        g.shutdown().await;
    }
}

#[tokio::test]
async fn persistence_and_native_session_resume_survive_restart() {
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let temp = tempfile::tempdir().unwrap();
        let cfg = options(&temp, kind);
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
    let mut cfg = options(&temp, AgentKind::Codex);
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
    let mut cfg = options(&temp, AgentKind::Pi);
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
async fn list_tasks_combines_filters_and_keeps_pagination_and_validation() {
    let temp = tempfile::tempdir().unwrap();
    let gateway = gateway(&temp, AgentKind::Pi).await;
    let first = complete(
        &gateway,
        Message::user_text("hello").with_context_id("conversation-a"),
    )
    .await;
    let second = complete(
        &gateway,
        Message::user_text("hello").with_context_id("conversation-b"),
    )
    .await;
    let router = a2a::router(Arc::clone(&gateway)).unwrap();

    let first_page = rpc(&router, "ListTasks", json!({"pageSize":1})).await;
    assert_eq!(first_page["result"]["totalSize"], 2);
    let next_page = rpc(
        &router,
        "ListTasks",
        json!({"pageSize":1,"pageToken":first_page["result"]["nextPageToken"]}),
    )
    .await;
    let listed_ids = [
        first_page["result"]["tasks"][0]["id"].as_str().unwrap(),
        next_page["result"]["tasks"][0]["id"].as_str().unwrap(),
    ];
    assert!(listed_ids.contains(&first.id.as_str()));
    assert!(listed_ids.contains(&second.id.as_str()));

    // 时间条件包含等于边界；筛选后再计算分页数量，history/artifacts 选项仍然生效。
    let filtered = rpc(
        &router,
        "ListTasks",
        json!({
            "contextId":"conversation-a",
            "status":"TASK_STATE_COMPLETED",
            "statusTimestampAfter":first.status.timestamp,
            "historyLength":0,
            "includeArtifacts":true
        }),
    )
    .await;
    assert_eq!(filtered["result"]["totalSize"], 1);
    let task = &filtered["result"]["tasks"][0];
    assert_eq!(task["id"], first.id.as_str());
    assert!(task.get("history").is_none());
    assert!(!task["artifacts"].as_array().unwrap().is_empty());

    for params in [
        json!({"contextId":"missing"}),
        json!({"status":"TASK_STATE_FAILED"}),
        json!({"statusTimestampAfter":"2999-01-01T00:00:00Z"}),
    ] {
        let response = rpc(&router, "ListTasks", params).await;
        assert_eq!(response["result"]["totalSize"], 0);
    }
    for params in [
        json!({"pageToken":"offset:invalid"}),
        json!({"pageToken":"offset:3"}),
        json!({"statusTimestampAfter":"invalid"}),
    ] {
        let response = rpc(&router, "ListTasks", params).await;
        assert_eq!(response["error"]["code"], -32602);
    }
    gateway.shutdown().await;
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
    let cfg = options(&temp, AgentKind::Codex);
    let store = Store::open(&cfg.instance_directory().unwrap())
        .await
        .unwrap();
    let mut task = Task::new("unfinished", "context");
    a2alinker::storage::metadata(&mut task).insert("a2alinker".into(), json!({"agent":"codex"}));
    task.status.state = TaskState::Working;
    store
        .insert_task(&task, "original-message", "digest")
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
        let mut cfg = options(&temp, kind);
        cfg.skills.directories =
            vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills")];
        let g = Gateway::open(cfg).await.unwrap();
        // Card 的占位声明不受已加载 SKILL.md 影响，也不会阻止技能加载与任务执行。
        let card = serde_json::to_value(a2a::agent_card(&g)).unwrap();
        assert_eq!(card["skills"].as_array().unwrap().len(), 1);
        assert_eq!(card["skills"][0]["id"], "dummy");
        let task = complete(&g, Message::user_text("hello")).await;
        assert_eq!(task.status.state, TaskState::Completed);
        g.shutdown().await;
    }
}

#[tokio::test]
async fn source_skills_and_native_configuration_are_not_copied_or_rewritten() {
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let temp = tempfile::tempdir().unwrap();
        let mut cfg = options(&temp, kind);
        let source = temp.path().join("specified-native");
        std::fs::create_dir_all(source.join("skills/native-off")).unwrap();
        let filename = if kind == AgentKind::Pi {
            "settings.json"
        } else {
            "config.toml"
        };
        let config = source.join(filename);
        std::fs::write(&config, "original native configuration").unwrap();
        std::fs::write(
            source.join("skills/native-off/SKILL.md"),
            "---\nname: native-off\ndescription: Native skill\nenabled: false\n---\nNative skill\n",
        )
        .unwrap();
        cfg.agent.native_config = Some(config.clone());
        cfg.skills.directories =
            vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills")];
        let trace = temp.path().join("trace.jsonl");
        cfg.agent
            .env
            .insert("A2ALINKER_TEST_TRACE".into(), trace.display().to_string());
        let g = Gateway::open(cfg.clone()).await.unwrap();
        assert_eq!(
            complete(&g, Message::user_text("hello")).await.status.state,
            TaskState::Completed
        );
        g.shutdown().await;
        let records: Vec<Value> = std::fs::read_to_string(trace)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for startup in records.iter().filter_map(|record| record.get("startup")) {
            assert_eq!(Path::new(startup["configHome"].as_str().unwrap()), source);
            assert!(
                !startup["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|arg| arg == "--no-skills")
            );
        }
        assert!(
            !records
                .iter()
                .any(|record| record["request"]["method"] == "skills/config/write")
        );
        if kind == AgentKind::Codex {
            let roots = records
                .iter()
                .find(|record| record["request"]["method"] == "skills/extraRoots/set")
                .unwrap();
            assert_eq!(
                roots["request"]["params"]["extraRoots"][0],
                cfg.skills.directories[0].to_str().unwrap()
            );
        }
        assert_eq!(
            std::fs::read_to_string(config).unwrap(),
            "original native configuration"
        );
        for directory in ["home", "skills", "sessions"] {
            assert!(!cfg.instance_directory().unwrap().join(directory).exists());
        }
    }
}

#[tokio::test]
async fn evicted_sessions_resume_across_model_and_skill_changes() {
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let temp = tempfile::tempdir().unwrap();
        let mut cfg = options(&temp, kind);
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
        cfg.skills.directories =
            vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills")];
        let g = Gateway::open(cfg).await.unwrap();
        let resumed = complete(
            &g,
            Message::user_text("hello").with_context_id(first.context_id.as_str()),
        )
        .await;
        assert_eq!(resumed.context_id, first.context_id);
        assert_eq!(resumed.status.state, TaskState::Completed);
        assert_eq!(resumed.artifacts[0].parts[0].as_text(), Some("Hello 3"));
        g.shutdown().await;
    }
}

#[tokio::test]
async fn bounded_queue_rejects_overflow_without_persisting_a_phantom_task() {
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = options(&temp, AgentKind::Codex);
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

#[tokio::test]
async fn legacy_pi_session_paths_and_fingerprints_do_not_prevent_continuation() {
    use sqlx::Connection;
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = options(&temp, AgentKind::Pi);
    let g = Gateway::open(cfg.clone()).await.unwrap();
    let first = complete(&g, Message::user_text("hello")).await;
    let original = g
        .store
        .context(first.context_id.as_str())
        .await
        .unwrap()
        .unwrap()
        .native_id
        .unwrap();
    g.shutdown().await;
    drop(g);

    // 模拟旧版本的实例私有会话路径和非空 fingerprint，原生历史仍须原地恢复。
    let storage = cfg.instance_directory().unwrap();
    let legacy = storage.join("sessions/old-session.jsonl");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::rename(original, &legacy).unwrap();
    let store = Store::open(&storage).await.unwrap();
    store
        .set_native_id(first.context_id.as_str(), legacy.to_str().unwrap())
        .await
        .unwrap();
    let mut connection = sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new().filename(storage.join("a2alinker.db")),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE contexts SET fingerprint='old-configuration' WHERE id=?")
        .bind(first.context_id.as_str())
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    store.close().await;
    drop(store);

    cfg.agent.model = Some("new-model".into());
    let g = Gateway::open(cfg.clone()).await.unwrap();
    let resumed = complete(
        &g,
        Message::user_text("continue").with_context_id(first.context_id.as_str()),
    )
    .await;
    assert_eq!(resumed.status.state, TaskState::Completed);
    assert_eq!(resumed.artifacts[0].parts[0].as_text(), Some("Hello 2"));
    assert_eq!(
        g.store
            .context(first.context_id.as_str())
            .await
            .unwrap()
            .unwrap()
            .native_id
            .as_deref(),
        legacy.to_str()
    );
    assert!(!storage.join("home").exists());
    g.shutdown().await;
    drop(g);

    // 原生文件确实丢失时应报实际错误，而不能悄悄新建空会话继续回答。
    std::fs::remove_file(&legacy).unwrap();
    let g = Gateway::open(cfg).await.unwrap();
    let failed = complete(
        &g,
        Message::user_text("continue").with_context_id(first.context_id.as_str()),
    )
    .await;
    assert_eq!(failed.status.state, TaskState::Failed);
    assert!(
        failed
            .status
            .message
            .unwrap()
            .text_content()
            .unwrap()
            .contains("Pi session file is missing")
    );
    assert!(!legacy.exists());
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
    let mut cfg = options(&temp, AgentKind::Pi);
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
