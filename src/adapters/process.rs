use std::{collections::VecDeque, process::Stdio};

use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::mpsc,
};
use tokio_util::codec::{FramedRead, LinesCodec};

use super::AdapterContext;

pub struct JsonlProcess {
    child: Child,
    group_id: Option<u32>,
    stdin: ChildStdin,
    incoming: mpsc::Receiver<Result<Value>>,
    deferred: VecDeque<Value>,
    next_id: u64,
    readers: Vec<tokio::task::JoinHandle<()>>,
}

impl JsonlProcess {
    pub fn spawn(context: &AdapterContext, args: &[String]) -> Result<Self> {
        let mut cmd = Command::new(context.config.executable());
        cmd.args(args)
            .current_dir(&context.config.agent.workspace)
            .envs(&context.config.agent.env)
            .env(
                match context.config.agent.kind {
                    crate::config::AgentKind::Pi => "PI_CODING_AGENT_DIR",
                    crate::config::AgentKind::Codex => "CODEX_HOME",
                },
                &context.home,
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().context("spawn agent process")?;
        let group_id = child.id();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, incoming) = mpsc::channel(128);
        let limit = context.config.runtime.max_protocol_record_bytes;
        let stdout_reader = tokio::spawn(async move {
            let mut lines = FramedRead::new(stdout, LinesCodec::new_with_max_length(limit));
            while let Some(line) = lines.next().await {
                let value = match line {
                    Ok(line) => serde_json::from_str(&line).context("agent emitted invalid JSON"),
                    Err(e) => Err(e).context("agent JSONL framing error"),
                };
                let failed = value.is_err();
                if tx.send(value).await.is_err() || failed {
                    return;
                }
            }
        });
        // Always drain stderr. Do not put arbitrary agent output (possibly credentials) in logs.
        let stderr_reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            loop {
                match reader.fill_buf().await {
                    Ok([]) | Err(_) => break,
                    Ok(buf) => {
                        let n = buf.len();
                        reader.consume(n);
                    }
                }
            }
        });
        Ok(Self {
            child,
            group_id,
            stdin,
            incoming,
            deferred: VecDeque::new(),
            next_id: 0,
            readers: vec![stdout_reader, stderr_reader],
        })
    }

    pub async fn send(&mut self, value: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .context("write agent protocol request")?;
        self.stdin.flush().await?;
        Ok(())
    }

    pub async fn request(&mut self, method: &str, params: Value, pi: bool) -> Result<Value> {
        self.next_id += 1;
        let id = format!("linker-{}", self.next_id);
        let value = if pi {
            let mut p = params.as_object().cloned().unwrap_or_default();
            p.insert("id".into(), json!(id));
            p.insert("type".into(), json!(method));
            Value::Object(p)
        } else {
            json!({"id": id, "method": method, "params": params})
        };
        self.send(value).await?;
        loop {
            let record = self.raw_next().await?;
            if record.get("id").and_then(Value::as_str) == Some(&id)
                && (!pi || record["type"] == "response")
            {
                if pi {
                    ensure!(
                        record["success"] == true,
                        "Pi rejected {method}: {}",
                        record.get("error").unwrap_or(&Value::Null)
                    );
                    return Ok(record.get("data").cloned().unwrap_or(Value::Null));
                }
                if record.get("error").is_some() {
                    bail!("Codex rejected {method}: {}", record["error"]);
                }
                return record
                    .get("result")
                    .cloned()
                    .context("agent response missing result");
            }
            ensure!(
                self.deferred.len() < 1024,
                "too many notifications before a command response"
            );
            self.deferred.push_back(record);
        }
    }

    async fn raw_next(&mut self) -> Result<Value> {
        match self.incoming.recv().await {
            Some(result) => result,
            None => bail!(
                "agent protocol stream closed (process status: {:?})",
                self.child.try_wait()?
            ),
        }
    }

    pub async fn next(&mut self) -> Result<Value> {
        match self.deferred.pop_front() {
            Some(v) => Ok(v),
            None => self.raw_next().await,
        }
    }

    pub fn is_alive(&mut self) -> bool {
        if matches!(self.child.try_wait(), Ok(None)) {
            true
        } else {
            self.terminate();
            false
        }
    }

    pub fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(id) = self.group_id.take() {
            // The child is the leader of a dedicated process group, including its tools.
            unsafe {
                libc::kill(-(id as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for JsonlProcess {
    fn drop(&mut self) {
        self.terminate();
        for task in &self.readers {
            task.abort();
        }
    }
}
