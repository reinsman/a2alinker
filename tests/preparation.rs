//! 使用模拟 nvm 验证通用环境准备，不要求 CI 安装 nvm、Node.js 或真实 agent。
#![cfg(unix)]

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use a2alinker::{
    adapters,
    options::{AgentKind, Options},
    runtime::Gateway,
};
use ra2a::types::{Message, TaskState};

fn install_mock_nvm(root: &Path) -> (Options, PathBuf) {
    let nvm = root.join("nvm with spaces");
    let bin = nvm.join("versions/node/v24.20.0/bin");
    std::fs::create_dir_all(&bin).unwrap();
    // 故意向 stdout 输出初始化信息，测试脚本是否将其隔离到 stderr。
    std::fs::write(
        nvm.join("nvm.sh"),
        r#"printf 'mock nvm initialization\n'
nvm() {
    printf 'mock nvm use\n'
    [ "$1" = use ] && [ "$2" = --silent ] && [ "$3" = 24.20.0 ] || return 3
    export NVM_BIN="$NVM_DIR/versions/node/v$3/bin"
    export PATH="$NVM_BIN:$PATH"
}
"#,
    )
    .unwrap();
    executable(&bin.join("node"), "#!/bin/bash\nprintf 'v24.20.0\\n'\n");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent.py"),
        bin.join("pi"),
    )
    .unwrap();
    let mut cfg = Options::default();
    cfg.agent.kind = AgentKind::Pi;
    cfg.agent.prepare =
        Some(r#"source "$NVM_DIR/nvm.sh" --no-use && nvm use --silent 24.20.0"#.into());
    cfg.agent.workspace = root.into();
    cfg.agent.env.insert(
        "PI_CODING_AGENT_DIR".into(),
        root.join("user-pi").display().to_string(),
    );
    // 不修改测试进程的全局环境，避免与并发测试互相影响。
    cfg.agent.env.insert("PATH".into(), "/usr/bin:/bin".into());
    cfg.agent
        .env
        .insert("NVM_DIR".into(), nvm.display().to_string());
    cfg.storage.directory = root.join("data");
    cfg.runtime.startup_timeout_seconds = 5;
    cfg.runtime.task_timeout_seconds = 5;
    cfg.runtime.cancel_timeout_seconds = 1;
    (cfg, bin)
}

fn executable(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
async fn prepared_pi_supports_probe_handshake_tasks_and_cancellation() {
    let temp = tempfile::tempdir().unwrap();
    let (cfg, _) = install_mock_nvm(temp.path());
    assert_eq!(adapters::probe(&cfg).await.unwrap(), "fixture-agent 1.0");
    let gateway = Gateway::open(cfg).await.unwrap();
    let task = gateway.submit(Message::user_text("hello")).await.unwrap();
    let completed =
        tokio::time::timeout(Duration::from_secs(5), gateway.wait(task.id.as_str(), true))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(completed.status.state, TaskState::Completed);
    assert_eq!(completed.artifacts[0].parts[0].as_text(), Some("Hello 1"));

    let slow = gateway.submit(Message::user_text("slow")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while gateway.task(slow.id.as_str()).await.unwrap().status.state != TaskState::Working {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let canceled = gateway.cancel(slow.id.as_str()).await.unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);
    })
    .await
    .unwrap();
    gateway.shutdown().await;
}

#[tokio::test]
async fn generic_preparation_supports_nvm_proxy_and_each_agent_process() {
    for kind in [AgentKind::Pi, AgentKind::Codex] {
        let temp = tempfile::tempdir().unwrap();
        let (mut cfg, bin) = install_mock_nvm(temp.path());
        cfg.agent.kind = kind;
        cfg.agent.executable = None;
        cfg.agent.env.insert(
            "CODEX_HOME".into(),
            temp.path().join("user-codex").display().to_string(),
        );
        cfg.agent.env.insert(
            "PREP_TEST_AGENT".into(),
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/fake_agent.py")
                .display()
                .to_string(),
        );
        executable(
            &bin.join(kind.as_str()),
            r#"#!/bin/bash
[ "$(node --version)" = v24.20.0 ] && [ "$HTTPS_PROXY" = http://127.0.0.1:7890 ] || exit 9
exec "$PREP_TEST_AGENT" "$@"
"#,
        );
        cfg.agent.prepare = Some(r#"source "$NVM_DIR/nvm.sh" --no-use && nvm use --silent 24.20.0 && export HTTPS_PROXY=http://127.0.0.1:7890 && printf 'prepared\n' >> prepare-count"#.into());
        assert_eq!(adapters::probe(&cfg).await.unwrap(), "fixture-agent 1.0");
        let gateway = Gateway::open(cfg).await.unwrap();
        let task = gateway.submit(Message::user_text("hello")).await.unwrap();
        let task =
            tokio::time::timeout(Duration::from_secs(5), gateway.wait(task.id.as_str(), true))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(task.status.state, TaskState::Completed);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("prepare-count"))
                .unwrap()
                .lines()
                .count(),
            3,
            "version probe, preflight and task session must each prepare their environment"
        );
        gateway.shutdown().await;
    }
}

#[tokio::test]
async fn preparation_timeout_cleans_up_probe_and_session_processes() {
    use nix::{sys::signal::kill, unistd::Pid};
    let temp = tempfile::tempdir().unwrap();
    let (mut cfg, _) = install_mock_nvm(temp.path());
    cfg.agent.prepare = Some("printf '%s' $$ > prepare-pid; sleep 60".into());
    cfg.runtime.startup_timeout_seconds = 1;
    assert!(
        adapters::probe(&cfg)
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
    for session in [false, true] {
        if session {
            let error = Gateway::open(cfg.clone()).await.err().unwrap();
            assert!(error.to_string().contains("timed out"), "{error:#}");
        }
        let pid: i32 = std::fs::read_to_string(temp.path().join("prepare-pid"))
            .unwrap()
            .parse()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while kill(Pid::from_raw(pid), None).is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out preparation process must be killed and reaped");
    }
}
