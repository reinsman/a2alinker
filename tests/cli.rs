//! 启动真实二进制，使用协议替身验证 CLI 默认值、原生继承和端口递增。
#![cfg(all(unix, feature = "cli"))]

use std::{
    fs::{self, File},
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde_json::{Value, json};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent.py")
}

fn skill(root: &Path, name: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: CLI fixture skill\n---\nInspect the workspace.\n"),
    )
    .unwrap();
}

fn setup(root: &Path) {
    fs::create_dir_all(root.join("project")).unwrap();
    fs::create_dir_all(root.join("bin")).unwrap();
    for executable in ["pi", "codex", "agent with spaces"] {
        fs::copy(fixture(), root.join("bin").join(executable)).unwrap();
    }
    for native in [".pi/agent", ".codex"] {
        skill(
            &root.join("user").join(native).join("skills/native-review"),
            "native-review",
        );
    }
    fs::write(
        root.join("user/.pi/agent/settings.json"),
        r#"{"defaultModel":"native-model"}"#,
    )
    .unwrap();
    fs::write(
        root.join("user/.codex/config.toml"),
        "model = 'native-model'\n",
    )
    .unwrap();
}

struct Running {
    process: Child,
    log: PathBuf,
}

impl Running {
    fn start(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Self {
        Self::start_with_log(root, args, env, "server.log")
    }

    fn start_with_log(root: &Path, args: &[&str], env: &[(&str, &str)], log_name: &str) -> Self {
        let log = root.join(log_name);
        let mut command = Command::new(env!("CARGO_BIN_EXE_a2alinker"));
        // 不继承开发者当前的 CLI 覆盖项，也不读取真实 agent home。
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("A2ALINKER_") {
                command.env_remove(key);
            }
        }
        let mut paths = vec![root.join("bin")];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        command
            .arg("serve")
            .args(args)
            .current_dir(root.join("project"))
            .env("HOME", root.join("user"))
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("CODEX_HOME")
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("RUST_LOG", "a2alinker=info")
            .env("A2ALINKER_TEST_TRACE", root.join("trace.jsonl"))
            .envs(env.iter().copied())
            .stdout(Stdio::null())
            .stderr(File::create(&log).unwrap());
        Self {
            process: command.spawn().unwrap(),
            log,
        }
    }

    fn ready(&mut self) -> SocketAddr {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let log = fs::read_to_string(&self.log).unwrap();
            assert!(
                self.process.try_wait().unwrap().is_none(),
                "service exited: {log}"
            );
            if log.contains("A2A server ready")
                && let Some((_, tail)) = log.split_once("http://127.0.0.1:")
            {
                let port: String = tail.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(port) = port.parse::<u16>() {
                    return SocketAddr::from(([127, 0, 0, 1], port));
                }
            }
            assert!(
                Instant::now() < deadline,
                "service did not become ready: {log}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn stop(&mut self) {
        kill(
            Pid::from_raw(i32::try_from(self.process.id()).unwrap()),
            Signal::SIGTERM,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "service did not shut down");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn failure(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                assert!(!status.success(), "service unexpectedly succeeded");
                return fs::read_to_string(&self.log).unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "service did not reject invalid startup"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.process.try_wait().ok().flatten().is_none() {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }
}

fn http(address: SocketAddr, method: &str, path: &str, body: &str) -> Value {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    serde_json::from_str(body).unwrap()
}

#[test]
fn missing_name_or_new_description_is_rejected_without_starting_an_agent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let mut missing_name = Running::start(root, &["--desc", "开发助手"], &[]);
    assert!(missing_name.failure().contains("--name"));
    let mut missing_description = Running::start(root, &["--name", "Alan"], &[]);
    let log = missing_description.failure();
    assert!(log.contains("instance.new"));
    assert!(log.contains("必须提供 --desc"));
    assert!(!root.join("user/.rein/storage/Alan").exists());
    for description in ["", " \t"] {
        let mut blank = Running::start(root, &["--name", "Alan", "--desc", description], &[]);
        assert!(blank.failure().contains("描述不能为空"));
    }
    // 已存在但尚未登记的空目录，也必须填写描述才能完成初始化。
    fs::create_dir_all(root.join("user/.rein/storage/Empty")).unwrap();
    let mut empty = Running::start(root, &["--name", "Empty"], &[]);
    let log = empty.failure();
    assert!(log.contains("instance.existing"));
    assert!(log.contains("必须提供 --desc"));
    assert!(!root.join("trace.jsonl").exists());
}

#[test]
fn instance_identity_survives_restart_and_rejects_another_agent_before_launch() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let mut initial = Running::start(
        root,
        &["--listen", "0"],
        &[
            ("A2ALINKER_NAME", "Alan"),
            ("A2ALINKER_DESC", "原始项目助手"),
        ],
    );
    let address = initial.ready();
    assert_eq!(
        http(address, "GET", "/.well-known/agent-card.json", "")["description"],
        "原始项目助手"
    );
    assert!(
        fs::read_to_string(&initial.log)
            .unwrap()
            .contains("instance.new")
    );
    initial.stop();
    let trace = fs::read_to_string(root.join("trace.jsonl")).unwrap();
    let mut wrong = Running::start(
        root,
        &[
            "codex",
            "--name",
            "Alan",
            "--desc",
            "不能覆盖原描述",
            "--listen",
            "0",
        ],
        &[],
    );
    let log = wrong.failure();
    assert!(log.contains("instance.existing"));
    assert!(log.contains("agent 为 pi"));
    assert!(log.contains("codex 不一致"));
    assert_eq!(fs::read_to_string(root.join("trace.jsonl")).unwrap(), trace);
    for (description, expected) in [
        (None, "原始项目助手"),
        (Some("更新后的项目助手"), "更新后的项目助手"),
        (None, "更新后的项目助手"),
    ] {
        let mut args = vec!["--name", "Alan", "--listen", "0"];
        if let Some(description) = description {
            args.extend(["--desc", description]);
        }
        let mut resumed = Running::start(root, &args, &[]);
        let address = resumed.ready();
        let card = http(address, "GET", "/.well-known/agent-card.json", "");
        assert_eq!(card["description"], expected);
        assert!(
            fs::read_to_string(&resumed.log)
                .unwrap()
                .contains("instance.existing")
        );
        resumed.stop();
    }
}

#[test]
fn serve_uses_pi_current_workspace_native_skills_and_user_storage() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let mut server = Running::start(
        root,
        &[
            "--name",
            "Alan",
            "--desc",
            "本地 pi 编程助手",
            "--listen",
            "0",
            "--executable",
            "   ",
        ],
        &[],
    );
    let address = server.ready();
    let card = http(address, "GET", "/.well-known/agent-card.json", "");
    assert_eq!(card["name"], "Alan");
    assert_eq!(card["description"], "本地 pi 编程助手");
    assert_eq!(card["skills"][0]["id"], "dummy");
    assert_eq!(
        card["supportedInterfaces"][0]["url"],
        format!("http://{address}/a2a")
    );
    let request = json!({"jsonrpc":"2.0","id":"default","method":"SendMessage","params":{"message":{
        "messageId":"default-message","role":"ROLE_USER","parts":[{"text":"hello"}]
    }}});
    let response = http(address, "POST", "/a2a", &request.to_string());
    assert_eq!(
        response["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    assert_eq!(
        response["result"]["task"]["metadata"]["a2alinker"]["agent"],
        "pi"
    );
    let storage = root.join("user/.rein/storage/Alan");
    assert!(storage.join("a2alinker.db").is_file());
    assert!(!storage.join("home").exists());
    assert!(!storage.join("sessions").exists());
    assert!(!storage.join("skills").exists());
    assert!(root.join("user/.pi/agent/sessions").is_dir());
    let log = fs::read_to_string(root.join("server.log")).unwrap();
    assert!(
        log.contains(
            root.join("project")
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
        )
    );
    server.stop();
    let trace = fs::read_to_string(root.join("trace.jsonl")).unwrap();
    assert!(!trace.contains("--no-skills"));
    assert!(!trace.contains("--no-extensions"));
    assert!(
        !trace.contains("--model"),
        "native model selection must be preserved"
    );
}

#[test]
fn bearer_credentials_are_read_by_the_binary() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let args = [
        "--name",
        "Alan",
        "--desc",
        "开发助手",
        "--auth",
        "bearer",
        "--listen",
        "0",
    ];
    let mut missing = Running::start(root, &args, &[]);
    assert!(missing.failure().contains("A2ALINKER_TOKEN"));
    assert!(!root.join("trace.jsonl").exists());
    let mut server = Running::start(
        root,
        &args,
        &[(
            "A2ALINKER_TOKEN",
            "binary-token-containing-at-least-32-bytes",
        )],
    );
    let address = server.ready();
    let card = http(address, "GET", "/.well-known/agent-card.json", "");
    assert!(card["securitySchemes"]["bearer"].is_object());
    server.stop();
}

#[test]
fn occupied_port_is_incremented_and_card_describes_the_actual_codex_service() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port().to_string();
    let executable = root.join("bin/agent with spaces");
    let mut server = Running::start(
        root,
        &[
            "codex",
            "--listen",
            &port,
            "--name",
            "Bob",
            "--desc",
            "项目开发助手",
            "--storage-dir",
            "state",
            "--executable",
            executable.to_str().unwrap(),
        ],
        &[("A2ALINKER_AGENT", "pi")],
    );
    let address = server.ready();
    assert!(address.port() > occupied.local_addr().unwrap().port());
    let card = http(address, "GET", "/.well-known/agent-card.json", "");
    assert_eq!(card["name"], "Bob");
    assert_eq!(card["description"], "项目开发助手");
    assert_eq!(
        card["supportedInterfaces"][0]["url"],
        format!("http://{address}/a2a")
    );
    assert!(root.join("project/state/Bob/a2alinker.db").is_file());
    server.stop();
    let trace: Vec<Value> = fs::read_to_string(root.join("trace.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let thread = trace
        .iter()
        .find(|line| line["request"]["method"] == "thread/start")
        .unwrap();
    assert!(thread["request"]["params"].get("sandbox").is_none());
    assert!(thread["request"]["params"].get("approvalPolicy").is_none());
    assert!(thread["request"]["params"].get("model").is_none());
}

#[test]
fn flags_override_environment_and_explicit_skills_are_loaded() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    skill(
        &root.join("project/custom-skills/custom-review"),
        "custom-review",
    );
    let mut server = Running::start(
        root,
        &[
            "--agent",
            "pi",
            "--name",
            "Alan",
            "--desc",
            "负责当前项目的代码审查",
            "--listen",
            "0",
            "--skill-dir",
            "custom-skills",
            "--model",
            "chosen-model",
            "--provider",
            "chosen-provider",
            "--prepare",
            "printf 'prepare noise'; export HTTPS_PROXY=http://127.0.0.1:7890",
        ],
        &[("A2ALINKER_AGENT", "codex"), ("A2ALINKER_NAME", "Bob")],
    );
    let address = server.ready();
    let card = http(address, "GET", "/.well-known/agent-card.json", "");
    assert_eq!(card["name"], "Alan");
    assert_eq!(card["description"], "负责当前项目的代码审查");
    assert_eq!(card["skills"].as_array().unwrap().len(), 1);
    assert_eq!(card["skills"][0]["id"], "dummy");
    server.stop();
    let trace = fs::read_to_string(root.join("trace.jsonl")).unwrap();
    assert!(!trace.contains("--no-skills"));
    assert!(trace.contains("--skill"));
    assert!(trace.contains(root.join("project/custom-skills").to_str().unwrap()));
    assert!(trace.contains("chosen-model"));
    assert!(trace.contains("chosen-provider"));
}

#[test]
fn named_instances_share_a_storage_root_and_resume_their_own_tasks() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    setup(root);
    let storage = root.join("shared-storage");
    let storage_arg = storage.to_str().unwrap();
    let mut alan = Running::start_with_log(
        root,
        &[
            "pi",
            "--name",
            "Alan",
            "--desc",
            "Pi 项目开发助手",
            "--storage-dir",
            storage_arg,
            "--listen",
            "0",
        ],
        &[],
        "alan.log",
    );
    let alan_addr = alan.ready();
    let alan_port = alan_addr.port().to_string();
    let mut bob = Running::start_with_log(
        root,
        &[
            "codex",
            "--name",
            "Bob",
            "--desc",
            "Codex 项目开发助手",
            "--storage-dir",
            storage_arg,
            "--listen",
            &alan_port,
        ],
        &[],
        "bob.log",
    );
    let bob_addr = bob.ready();
    assert_ne!(alan_addr, bob_addr);
    assert_eq!(
        http(alan_addr, "GET", "/.well-known/agent-card.json", "")["name"],
        "Alan"
    );
    assert_eq!(
        http(bob_addr, "GET", "/.well-known/agent-card.json", "")["name"],
        "Bob"
    );
    let message = json!({"jsonrpc":"2.0", "id":"shared-root", "method":"SendMessage", "params":{"message":{
        "messageId":"same-id-in-different-instances", "role":"ROLE_USER", "parts":[{"text":"hello"}]
    }}}).to_string();
    let alan_task = http(alan_addr, "POST", "/a2a", &message)["result"]["task"].clone();
    let bob_task = http(bob_addr, "POST", "/a2a", &message)["result"]["task"].clone();
    assert_eq!(alan_task["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(bob_task["status"]["state"], "TASK_STATE_COMPLETED");
    assert_ne!(alan_task["id"], bob_task["id"]);
    for name in ["Alan", "Bob"] {
        assert!(storage.join(name).join("a2alinker.db").is_file());
        assert!(!storage.join(name).join("home").exists());
    }
    assert!(!storage.join("a2alinker.db").exists());
    assert!(!storage.join("Alan/sessions").exists());

    // 同名实例仍竞争同一个目录锁，不能绕过锁创建另一个数据库。
    let mut duplicate = Running::start_with_log(
        root,
        &[
            "pi",
            "--name",
            "Alan",
            "--storage-dir",
            storage_arg,
            "--listen",
            "0",
        ],
        &[],
        "duplicate.log",
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = duplicate.process.try_wait().unwrap() {
            assert!(!status.success());
            assert!(
                fs::read_to_string(&duplicate.log)
                    .unwrap()
                    .contains("already used")
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "duplicate instance did not reject its locked directory"
        );
        thread::sleep(Duration::from_millis(10));
    }
    alan.stop();
    assert_eq!(http(bob_addr, "GET", "/healthz", "")["status"], "ok");
    let mut resumed = Running::start_with_log(
        root,
        &[
            "pi",
            "--name",
            "Alan",
            "--storage-dir",
            storage_arg,
            "--listen",
            "0",
        ],
        &[],
        "resumed.log",
    );
    let resumed_addr = resumed.ready();
    let query = json!({"jsonrpc":"2.0", "id":"resume", "method":"GetTask", "params":{"id":alan_task["id"]}}).to_string();
    assert_eq!(
        http(resumed_addr, "POST", "/a2a", &query)["result"]["id"],
        alan_task["id"]
    );
    assert!(!storage.join("Alan/Alan").exists());
    resumed.stop();
    bob.stop();
}
