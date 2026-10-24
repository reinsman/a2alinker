//! 原生进程启动：共享环境准备逻辑，参数通过独立参数传递，不拼进 shell 命令。

use std::process::Stdio;

use tokio::process::Command;

use crate::Options;
use anyhow::Result;

pub(super) fn command(options: &Options) -> Result<Command> {
    let mut command = if let Some(prepare) = &options.agent.prepare {
        tracing::info!(
            event = "agent.prepare",
            agent = options.agent.kind.as_str(),
            "原生进程启动前执行环境准备；不记录准备命令或环境变量值"
        );
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "--noprofile",
                "--norc",
                "-c",
                // 先保存 argv，避免 source 的脚本改变位置参数。准备命令在同一 shell 中执行，
                // export/PATH 的修改因此会传给 agent；准备阶段不能消费 stdin 或污染 stdout。
                r#"__a2alinker_argv=("${@:2}")
eval "$1" </dev/null >&2
__a2alinker_status=$?
if [ "$__a2alinker_status" -ne 0 ]; then
    printf 'a2alinker: 环境准备失败（退出码 %s）\n' "$__a2alinker_status" >&2
    exit "$__a2alinker_status"
fi
exec "${__a2alinker_argv[@]}""#,
                "a2alinker-prepare",
                prepare,
            ])
            .arg(options.executable());
        command
    } else {
        Command::new(options.executable())
    };
    command
        .current_dir(&options.agent.workspace)
        .envs(&options.agent.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(directory) = options.native_config_directory()? {
        let key = match options.agent.kind {
            crate::AgentKind::Pi => "PI_CODING_AGENT_DIR",
            crate::AgentKind::Codex => "CODEX_HOME",
        };
        command.env(key, &directory);
        tracing::info!(event = "config.native", agent = options.agent.kind.as_str(), directory = %directory.display(), "直接使用指定的原生配置目录，不复制文件");
    }
    #[cfg(unix)]
    // shell、agent 和工具共享一个独立进程组，取消和超时会一起清理。
    command.process_group(0);
    Ok(command)
}

pub(super) struct ProcessGroup(Option<u32>);

impl ProcessGroup {
    pub(super) fn new(id: Option<u32>) -> Self {
        Self(id)
    }

    pub(super) fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(id) = self.0.take()
            && let Ok(raw_id) = i32::try_from(id)
            && raw_id > 1
        {
            use nix::{
                sys::signal::{Signal, killpg},
                unistd::Pid,
            };
            let _ = killpg(Pid::from_raw(raw_id), Signal::SIGKILL);
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn preparation_preserves_environment_arguments_and_protocol_pipes() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("agent $(touch injected)");
        std::fs::write(
            &executable,
            "#!/bin/bash\nprintf '%s\\n' \"$HTTP_PROXY\" \"$PWD\" \"$@\"\ncat\nexit 23\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut options = Options::default();
        options.agent.workspace = temp.path().into();
        options.agent.executable = Some(executable);
        options.agent.prepare = Some(
            "printf 'setup noise'; export HTTP_PROXY=http://127.0.0.1:7890; read unused || true"
                .into(),
        );
        let args = [
            "with spaces",
            "$(touch injected-args)",
            "'quoted'",
            "",
            "; exit 9",
        ];
        let mut child = command(&options).unwrap().args(args).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol input\n")
            .await
            .unwrap();
        let output = child.wait_with_output().await.unwrap();
        assert_eq!(output.status.code(), Some(23));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "http://127.0.0.1:7890\n{}\n{}\nprotocol input\n",
                temp.path().canonicalize().unwrap().display(),
                args.join("\n")
            )
        );
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("setup noise")
        );
        assert!(!temp.path().join("injected").exists());
        assert!(!temp.path().join("injected-args").exists());
    }

    #[tokio::test]
    async fn failed_preparation_does_not_execute_the_agent() {
        let temp = tempfile::tempdir().unwrap();
        let mut options = Options::default();
        options.agent.workspace = temp.path().into();
        options.agent.executable = Some("/bin/touch".into());
        options.agent.prepare = Some("printf 'setup failed'; false".into());
        let output = command(&options)
            .unwrap()
            .arg("agent-started")
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("环境准备失败")
        );
        assert!(!temp.path().join("agent-started").exists());
    }
}
