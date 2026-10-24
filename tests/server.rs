//! 从外部 daemon 的视角，仅使用公开 API 管理多个服务和每个实例的独立凭据。
#![cfg(unix)]

use std::{net::SocketAddr, path::Path, time::Duration};

use a2alinker::{AgentKind, AuthMode, Options, Server};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

const ALAN_TOKEN: &str = "alan-instance-token-at-least-32-bytes";
const BOB_TOKEN: &str = "bob-instance-token-at-least-32-bytes";

fn options(root: &Path, name: &str, kind: AgentKind, token: &str) -> Options {
    let mut options = Options::default();
    options.identity.name = name.into();
    options.identity.description = format!("{name} 的编程助手");
    options.agent.kind = kind;
    options.agent.workspace = root.join("workspace").join(name);
    std::fs::create_dir_all(&options.agent.workspace).unwrap();
    options.agent.executable =
        Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent.py"));
    for key in ["PI_CODING_AGENT_DIR", "CODEX_HOME"] {
        options.agent.env.insert(
            key.into(),
            root.join("native").join(name).display().to_string(),
        );
    }
    options.agent.env.insert(
        "A2ALINKER_TEST_TRACE".into(),
        root.join(format!("{name}.jsonl")).display().to_string(),
    );
    options.storage.directory = root.join("storage");
    options.server.listen = "127.0.0.1:0".parse().unwrap();
    options.runtime.cancel_timeout_seconds = 1;
    options.auth.mode = AuthMode::Bearer;
    options.auth.token = Some(token.into());
    options
}

async fn http(
    address: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (u16, Value) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut socket = TcpStream::connect(address).await.unwrap();
        let body = body.to_string();
        let authorization = token.map(|token| format!("Authorization: Bearer {token}\r\n")).unwrap_or_default();
        let request = format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nA2A-Version: 1.0\r\n{authorization}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        socket.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap())
    }).await.unwrap()
}

async fn rpc(address: SocketAddr, token: &str, method: &str, params: Value) -> Value {
    let (status, value) = http(
        address,
        "POST",
        "/a2a",
        Some(token),
        json!({"jsonrpc":"2.0","id":"daemon-request","method":method,"params":params}),
    )
    .await;
    assert_eq!(status, 200);
    value
}

fn message(id: &str, text: &str) -> Value {
    json!({"message":{"messageId":id,"role":"ROLE_USER","parts":[{"text":text}]}})
}

#[tokio::test]
async fn daemon_manages_isolated_instances_tokens_shutdown_and_restart() {
    let temp = tempfile::tempdir().unwrap();
    let alan_options = options(temp.path(), "Alan", AgentKind::Pi, ALAN_TOKEN);
    let mut alan = Server::start(alan_options.clone()).await.unwrap();
    let mut bob_options = options(temp.path(), "Bob", AgentKind::Codex, BOB_TOKEN);
    bob_options.server.listen = alan.local_addr();
    let bob = Server::start(bob_options).await.unwrap();
    assert_eq!(alan.name(), "Alan");
    assert!(!alan.is_finished());
    assert!(bob.local_addr().port() > alan.local_addr().port());
    // wait 可在 select/timeout 中取消，之后仍能使用同一实例和句柄。
    assert!(
        tokio::time::timeout(Duration::from_millis(20), alan.wait())
            .await
            .is_err()
    );
    for server in [&alan, &bob] {
        let (status, card) = http(
            server.local_addr(),
            "GET",
            "/.well-known/agent-card.json",
            None,
            Value::Null,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(card["name"], server.name());
        assert_eq!(
            http(
                server.local_addr(),
                "POST",
                "/a2a",
                None,
                message("m", "hello")
            )
            .await
            .0,
            401
        );
    }
    assert_eq!(
        http(
            alan.local_addr(),
            "POST",
            "/a2a",
            Some(BOB_TOKEN),
            message("m", "hello")
        )
        .await
        .0,
        401
    );
    assert_eq!(
        http(
            bob.local_addr(),
            "POST",
            "/a2a",
            Some(ALAN_TOKEN),
            message("m", "hello")
        )
        .await
        .0,
        401
    );
    let a = rpc(
        alan.local_addr(),
        ALAN_TOKEN,
        "SendMessage",
        message("same-id", "hello"),
    )
    .await;
    let b = rpc(
        bob.local_addr(),
        BOB_TOKEN,
        "SendMessage",
        message("same-id", "hello"),
    )
    .await;
    assert_eq!(
        a["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(
        b["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_ne!(a["result"]["task"]["id"], b["result"]["task"]["id"]);
    assert_eq!(
        rpc(
            bob.local_addr(),
            BOB_TOKEN,
            "GetTask",
            json!({"id":a["result"]["task"]["id"]})
        )
        .await["error"]["code"],
        -32001
    );
    assert!(
        Server::start(alan_options.clone())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("already used")
    );

    let mut slow = message("slow-id", "slow");
    slow["configuration"] = json!({"returnImmediately":true});
    let slow =
        rpc(alan.local_addr(), ALAN_TOKEN, "SendMessage", slow).await["result"]["task"]["id"]
            .clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let task = rpc(alan.local_addr(), ALAN_TOKEN, "GetTask", json!({"id":slow})).await;
            if task["result"]["status"]["state"] == "TASK_STATE_WORKING" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut queued = message("queued-id", "hello");
    queued["configuration"] = json!({"returnImmediately":true});
    let queued = rpc(alan.local_addr(), ALAN_TOKEN, "SendMessage", queued).await["result"]["task"]
        ["id"]
        .clone();
    tokio::time::timeout(Duration::from_secs(5), alan.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        http(bob.local_addr(), "GET", "/healthz", None, Value::Null)
            .await
            .1["status"],
        "ok"
    );

    // shutdown 返回后，端口、原生进程和实例锁应释放，历史和取消状态可在重启后读取。
    let mut restarted = Server::start(alan_options).await.unwrap();
    let mut followup = message("bob-followup", "hello");
    followup["message"]["contextId"] = b["result"]["task"]["contextId"].clone();
    assert_eq!(
        rpc(bob.local_addr(), BOB_TOKEN, "SendMessage", followup).await["result"]["task"]["artifacts"]
            [0]["parts"][0]["text"],
        "Hello 2"
    );
    for id in [slow, queued] {
        assert_eq!(
            rpc(
                restarted.local_addr(),
                ALAN_TOKEN,
                "GetTask",
                json!({"id":id})
            )
            .await["result"]["status"]["state"],
            "TASK_STATE_CANCELED"
        );
    }
    assert_eq!(
        rpc(
            restarted.local_addr(),
            ALAN_TOKEN,
            "GetTask",
            json!({"id":a["result"]["task"]["id"]})
        )
        .await["result"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    restarted.request_shutdown();
    restarted.wait().await.unwrap();
    assert!(restarted.is_finished());
    restarted.shutdown().await.unwrap();
    bob.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropped_handle_and_failed_start_release_the_instance() {
    let temp = tempfile::tempdir().unwrap();
    let options = options(temp.path(), "Alan", AgentKind::Pi, ALAN_TOKEN);
    let mut bad = options.clone();
    bad.agent.prepare = Some("false".into());
    assert!(Server::start(bad).await.is_err());
    let server = Server::start(options.clone()).await.unwrap();
    drop(server);
    let restarted = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match Server::start(options.clone()).await {
                Ok(server) => break server,
                Err(error) => {
                    assert!(error.to_string().contains("already used"), "{error:#}");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    })
    .await
    .unwrap();
    restarted.shutdown().await.unwrap();
}
