# a2alinker

使用 Rust 把本地 pi、Codex 提供为 A2A 1.0 服务。安装并配置好原生 agent 后，在项目目录运行即可。

```bash
cargo build --release --locked

./target/release/a2alinker serve       # 默认 pi
./target/release/a2alinker serve pi
./target/release/a2alinker serve codex
./target/release/a2alinker serve --help
```

程序只有 `serve` 子命令和帮助入口，不读取 a2alinker 配置文件。

## 启动参数

```bash
a2alinker serve codex \
  --name Bob \
  --desc "负责当前项目的开发与代码检查" \
  --workspace /work/project \
  --storage-dir ~/.rein/storage \
  --listen 10100
```

| 参数 | 默认值与行为 |
| --- | --- |
| `AGENT` / `--agent` | `pi`，支持 `pi`、`codex`；位置参数优先 |
| `--name` | `Alan`，Agent Card 中的简短名字 |
| `--desc` | `本地 pi 编程助手` 或 `本地 codex 编程助手` |
| `--workspace` | 当前进程的工作目录 |
| `--storage-dir` | 共享根目录 `~/.rein/storage`，每个实例保存到其中的 `{name}/` |
| `--listen` | `10100`，默认监听 `127.0.0.1`；支持 `IP:端口` |
| `--executable` | 省略或空白时从 PATH 查找 `pi` / `codex`；填写时使用指定文件 |
| `--skill-dir` | 对应 agent 的原生技能目录与发现规则；显式指定时只加载指定目录中的技能包，可重复 |
| `--agent-config` | agent 原生配置文件或目录；省略时自动使用原生配置 |
| `--model` / `--provider` | 沿用原生配置，填写时覆盖 |
| `--public-url` | 实际监听地址；反向代理部署时可以指定对外 origin |
| `--auth` | `none`；非回环监听必须设置 `bearer` |

端口占用时依次尝试 10101、10102 等，日志和 Agent Card 会显示实际地址。只有端口占用会触发递增，权限或地址错误会直接报错；`--listen 0` 由系统分配端口。

`--workspace` 决定 agent 在哪里读写文件和执行命令，`--storage-dir` 指定共享存储根目录。实例实际使用 `--storage-dir/{name}/`：

```text
~/.rein/storage/
├── Alan/
│   ├── a2alinker.db
│   ├── instance.lock
│   ├── sessions/
│   └── home/
└── Bob/
    ├── a2alinker.db
    ├── instance.lock
    └── home/
```

不同名字可以共享根目录并同时启动，端口占用时自动递增。同一根目录内的同名实例使用相同目录锁，不能同时运行。重启时保留相同根目录和名字，即可读取该实例原有的任务与会话。名字必须是单个目录名，例如 Alan、Bob，不能包含路径分隔符或 `..` 路径。

每个参数还支持对应的环境变量，命令行优先；完整列表见 `serve --help`。例如：

```bash
A2ALINKER_NAME=Alan A2ALINKER_WORKSPACE=/work/project a2alinker serve pi
```

## 沿用本地 agent

默认读取 pi 的 `PI_CODING_AGENT_DIR`（未设置时为 `~/.pi/agent`）或 Codex 的 `CODEX_HOME`（未设置时为 `~/.codex`）。默认技能目录分别为该目录下的 `skills/`；项目技能继续遵守 agent 原生的发现与信任规则。

原生模型配置、认证、技能和 pi 扩展等资源会用于服务。配置和认证文件使用私有运行副本，原生资源通过目录链接访问。Codex 的沙箱与审批规则默认沿用其原生配置，审批请求通过 A2A 转发给客户端。库调用方可以显式覆盖执行策略。

显式 `--skill-dir` 会校验并复制完整技能包，再按 pi / Codex 的原生接口加载；不会把这些文件直接拼接进提示词。技能加载并不是文件权限隔离。

如果 pi 依赖 nvm，可以先在当前终端执行 `nvm use 24.20.0`，或指定仓库内的启动脚本：

```bash
A2ALINKER_NODE_VERSION=24.20.0 a2alinker serve pi --executable ./scripts/pi-nvm
```

## A2A 调用

以启动日志显示的端口为准，默认是 10100：

```bash
curl http://127.0.0.1:10100/.well-known/agent-card.json

curl -N http://127.0.0.1:10100/a2a \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  --data-binary @- <<JSON
{
  "jsonrpc": "2.0",
  "id": "$(uuidgen)",
  "method": "SendStreamingMessage",
  "params": {
    "message": {
      "messageId": "$(uuidgen)",
      "role": "ROLE_USER",
      "parts": [{"text": "请介绍当前项目的结构"}]
    }
  }
}
JSON
```

支持 `SendMessage`、`SendStreamingMessage`、`GetTask`、`ListTasks`、`CancelTask`、`SubscribeToTask`。新任务接受文本，首个流式事件是当前 Task 快照，后续返回状态和产物事件。

Agent Card 的 `skills` 固定包含一项 `dummy` 占位，表示未声明具体能力清单；它不表示没有加载本地技能。Agent 的整体用途由 `--desc` 描述，Pi / Codex 的本地技能仍按原生规则或 `--skill-dir` 加载。

后续对话使用新的 `messageId` 并携带原 `contextId`。同一 `messageId` 与内容重试会返回原任务，不会重复执行。`taskId` 用于查询、取消和回复交互。每个实例串行执行任务，同一个 context 同时只能有一个活动任务。

SSE 断开不会取消任务。重连使用 `SubscribeToTask`，先恢复当前快照，再处理后续事件；已完成任务使用 `GetTask`。重启时未结束的任务会明确标记失败，不自动重跑可能修改文件的操作。

### 思考和工具过程

pi 的公开思考文本使用独立 Artifact，`metadata.a2alinker.kind = "reasoning"`；回答标记为 `answer`。客户端按 `artifactId` 更新文本，`append: true` 时追加，否则替换；最终完整文本不能重复追加。

工具过程位于 `statusUpdate.status.message.parts` 的 data Part：

```json
{
  "data": {
    "a2alinker": {
      "kind": "tool",
      "type": "tool_call",
      "toolCallId": "call-123",
      "name": "bash",
      "state": "running",
      "arguments": {"command": "ls -la"}
    }
  }
}
```

后续类型为 `tool_update`、`tool_result`，包含 `result.content` 和最终的 `isError`。以 `(taskId, toolCallId)` 关联，工具结果是累计快照，应替换而不是追加。`GetTask.history` 保存每次工具调用的最新快照。原生签名与加密推理不会返回；结构化凭据字段会脱敏，自由文本按原文返回。

### 审批与用户输入

等待输入时 Task 进入 `TASK_STATE_INPUT_REQUIRED`，`task.metadata.pendingInteraction` 包含交互 ID、方法和参数。客户端携带 `A2A-Extensions: urn:a2alinker:interaction:v1`，向原 Task 发送 data Part：

```json
{"data":{"interactionId":"待回复的交互ID","response":{"confirmed":true}}}
```

pi confirm 使用上述 `confirmed`；文本输入使用 `value` 或 `cancelled`。Codex 命令和文件审批使用 `{"decision":"accept"}`、`decline` 或 `cancel`；输入请求使用原生 `answers` 结构。回复必须带新的 messageId、原 taskId 和匹配的 interactionId；普通文本不会被当作授权。

## 作为 Rust 库使用

二进制只负责 clap 参数解析、日志和进程信号。`Server` 负责监听和 HTTP 生命周期，`runtime` 负责任务调度与恢复，`adapters` 负责原生进程和协议，`storage` 负责持久化。

```rust,no_run
use a2alinker::{Options, Server};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = Options::default();
    let server = Server::bind(options).await?;
    println!("listening on {}", server.local_addr());
    server.run(async {
        let _ = tokio::signal::ctrl_c().await;
    }).await
}
```

库不安装信号处理器，由调用方提供关闭 Future。`options.rs` 定义 `Options` 及各组运行选项，可以直接用 Rust 字段定制，不需要 CLI 或配置文件。

## 部署与验证

非回环监听使用 `--auth bearer`，通过 `A2ALINKER_TOKEN` 提供至少 32 字节的凭据。Agent Card、`/healthz` 公开，任务接口要求认证。每个实例对应一个信任域。

日志写入 stderr；`RUST_LOG=a2alinker=info` 显示中文生命周期日志，`debug` 显示更多事件信息。日志不记录提示词、思考正文、工具参数和输出。TLS 可由反向代理提供，SSE 需要关闭代理缓冲。

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

需要 Rust 1.88+，已安装的 pi / Codex，以及用于测试的 Python 3。集成测试使用独立子进程，不调用真实模型。本包使用 `unsafe_code = "forbid"`，不包含自身的 unsafe 实现。学习笔记保留在本地 `docs/`，不纳入 Git 跟踪。
