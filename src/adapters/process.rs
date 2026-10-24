//! 两种 agent 共用的 JSONL 子进程通信层。
//!
//! 子进程的 stdin 用于发送命令，stdout 用于接收协议，stderr 必须单独持续读取。
//! 本模块还拥有子进程的生命周期：对象被释放时，会清理进程组和后台读取任务。

use std::collections::VecDeque;

use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin},
    sync::mpsc,
};
use tokio_util::codec::{FramedRead, LinesCodec};

use super::{
    AdapterContext,
    launch::{self, ProcessGroup},
};

/// 拥有一个 agent 进程及其通信管道；由单个会话使用，不在多个任务间同时读写。
pub struct JsonlProcess {
    child: Child,
    // 单独保存进程组 ID：主进程退出后，其启动的工具进程仍可能存活。
    group: ProcessGroup,
    stdin: ChildStdin,
    // 后台读取任务通过有容量上限的通道送入报文，防止内存无限增长。
    incoming: mpsc::Receiver<Result<Value>>,
    // 等待命令响应时可能先收到通知；暂存这些通知，随后按原顺序处理。
    deferred: VecDeque<Value>,
    next_id: u64,
    readers: Vec<tokio::task::JoinHandle<()>>,
}

impl JsonlProcess {
    /// 启动进程并创建管道读取任务。返回 Result，启动失败时由 ? 向上层返回错误。
    pub fn spawn(context: &AdapterContext, args: &[String]) -> Result<Self> {
        let mut cmd = launch::command(&context.options)?;
        cmd.args(args);
        let mut child = cmd.spawn().with_context(|| {
            format!(
                "cannot spawn agent executable '{}'; check --prepare, --executable, PATH and the script interpreter (see README.md)",
                context.options.executable().display()
            )
        })?;
        let group = ProcessGroup::new(child.id());
        // 前面已将三个管道设置为 piped，启动成功后这些 Option 必然为 Some。
        // take() 取出管道所有权并把原字段置为 None，之后由本对象或读取任务持有。
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, incoming) = mpsc::channel(128);
        let limit = context.options.runtime.max_protocol_record_bytes;
        // async move 将管道和发送端的所有权移入后台任务；这里只按换行切分 JSONL。
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
        // 持续消费 stderr，防止管道写满后卡住 agent；不转发原文，避免泄露凭据。
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
            group,
            stdin,
            incoming,
            deferred: VecDeque::new(),
            next_id: 0,
            readers: vec![stdout_reader, stderr_reader],
        })
    }

    /// 将一条 JSON 命令写入 stdin，并以换行结束，形成完整的 JSONL 报文。
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

    /// 发送带唯一 ID 的命令并等待对应响应；中途收到的其他报文会放入 deferred。
    pub async fn request(&mut self, method: &str, params: Value, pi: bool) -> Result<Value> {
        self.next_id += 1;
        let id = format!("linker-{}", self.next_id);
        tracing::debug!(
            event = "agent.command",
            native_request_id = id,
            method,
            protocol = if pi { "pi-rpc" } else { "codex-app-server" },
            "向原生 agent 发送命令；不记录 params 或提示词"
        );
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
                tracing::debug!(
                    event = "agent.response",
                    native_request_id = id,
                    method,
                    "收到匹配的原生协议响应；命令响应不代表整个 A2A Task 已完成"
                );
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

    /// 从读取任务的通道中取一条报文；通道关闭说明协议流已结束。
    async fn raw_next(&mut self) -> Result<Value> {
        match self.incoming.recv().await {
            Some(result) => result,
            None => bail!(
                "agent protocol stream closed (process status: {:?}); check --prepare and --executable if startup failed",
                self.child.try_wait()?
            ),
        }
    }

    /// 优先处理等待响应期间暂存的报文，保证业务事件不会被跳过或乱序。
    pub async fn next(&mut self) -> Result<Value> {
        match self.deferred.pop_front() {
            Some(v) => Ok(v),
            None => self.raw_next().await,
        }
    }

    /// 非阻塞检查进程状态；发现主进程退出后，也清理可能残留的工具进程。
    pub fn is_alive(&mut self) -> bool {
        if matches!(self.child.try_wait(), Ok(None)) {
            true
        } else {
            self.terminate();
            false
        }
    }

    /// 尽力终止整个进程组。可重复调用，清理失败时继续尝试终止直接子进程。
    pub fn terminate(&mut self) {
        self.group.terminate();
        let _ = self.child.start_kill();
    }
}

impl Drop for JsonlProcess {
    fn drop(&mut self) {
        // Drop 是 Rust 的自动清理机制：正常离开作用域、通过 ? 提前返回时都会执行这里。
        // Drop 不能异步等待，因此这里只发出终止信号，并中止后台读取任务。
        self.terminate();
        for task in &self.readers {
            task.abort();
        }
    }
}
