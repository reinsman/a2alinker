//! 使用模拟 nvm 验证启动脚本，不要求 CI 安装 nvm、Node.js 或真实 pi。
#![cfg(unix)]

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use a2alinker::{
    adapters,
    config::{AgentKind, Config},
    runtime::Gateway,
};
use ra2a::types::{Message, TaskState};

fn install_mock_nvm(root: &Path) -> (Config, PathBuf) {
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
    let mut cfg = Config {
        schema_version: 1,
        ..Config::default()
    };
    cfg.agent.kind = AgentKind::Pi;
    cfg.agent.executable = Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/pi-nvm"));
    cfg.agent.workspace = root.into();
    cfg.agent.inherit_auth = false;
    cfg.agent.env.insert(
        "PI_CODING_AGENT_DIR".into(),
        root.join("user-pi").display().to_string(),
    );
    // 不修改测试进程的全局环境，避免与并发测试互相影响。
    cfg.agent.env.insert("PATH".into(), "/usr/bin:/bin".into());
    cfg.agent
        .env
        .insert("NVM_DIR".into(), nvm.display().to_string());
    cfg.agent
        .env
        .insert("A2ALINKER_NODE_VERSION".into(), "24.20.0".into());
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

fn launcher(cfg: &Config) -> Command {
    let mut command = Command::new(cfg.executable());
    command.envs(&cfg.agent.env);
    command
}

#[test]
fn nvm_launcher_preserves_arguments_exit_status_and_protocol_stdout() {
    let temp = tempfile::tempdir().unwrap();
    let (cfg, bin) = install_mock_nvm(temp.path());
    executable(
        &bin.join("pi"),
        "#!/bin/bash\n[ \"$(node --version)\" = v24.20.0 ] || exit 4\nprintf '%s\\n' \"$@\"\nexit 23\n",
    );
    let args = [
        "--mode",
        "rpc",
        "--session",
        "目录 with spaces/session.jsonl",
    ];
    let output = launcher(&cfg).args(args).output().unwrap();
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        args.join("\n") + "\n"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("mock nvm initialization"));
    assert!(stderr.contains("mock nvm use"));
}

#[test]
fn nvm_launcher_rejects_missing_nvm_version_and_pi_without_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let (cfg, bin) = install_mock_nvm(temp.path());
    let missing_nvm = launcher(&cfg)
        .env("NVM_DIR", temp.path().join("missing"))
        .output()
        .unwrap();
    assert_eq!(missing_nvm.status.code(), Some(127));
    assert!(missing_nvm.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing_nvm.stderr).contains("NVM_DIR"));

    let missing_version = launcher(&cfg)
        .env("A2ALINKER_NODE_VERSION", "99.0.0")
        .output()
        .unwrap();
    assert_eq!(missing_version.status.code(), Some(127));
    assert!(missing_version.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing_version.stderr).contains("nvm install 99.0.0"));

    std::fs::remove_file(bin.join("pi")).unwrap();
    // PATH 中即使还有另一个 pi，也不能绕过选定 Node.js 版本下的安装检查。
    let other = temp.path().join("other-bin");
    std::fs::create_dir(&other).unwrap();
    executable(&other.join("pi"), "#!/bin/bash\nprintf 'wrong pi\\n'\n");
    let missing_pi = launcher(&cfg)
        .env("PATH", format!("{}:/usr/bin:/bin", other.display()))
        .output()
        .unwrap();
    assert_eq!(missing_pi.status.code(), Some(127));
    assert!(missing_pi.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing_pi.stderr).contains("npm install -g"));
}

#[tokio::test]
async fn nvm_launcher_supports_probe_handshake_tasks_and_cancellation() {
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
