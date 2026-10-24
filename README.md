# a2alinker

使用 Rust 把本地 pi、Codex 提供为 A2A 1.0 服务。安装并配置好原生 agent 后，在项目目录运行即可。

```bash
cargo build --release --locked

./target/release/a2alinker serve --name Alan --desc "项目开发助手"  # 默认 pi
./target/release/a2alinker serve pi --name Alan --desc "项目开发助手"
./target/release/a2alinker serve codex --name Bob --desc "代码审查助手"
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
| `--name` | 必填，无默认值；实例名字，也是 Agent Card 中的简短名字 |
| `--desc` | 新实例必填；已有实例省略时沿用本地描述，显式填写时更新描述 |
| `--workspace` | 当前进程的工作目录 |
| `--storage-dir` | 共享根目录 `~/.rein/storage`，每个实例保存到其中的 `{name}/` |
| `--listen` | `10100`，默认监听 `127.0.0.1`；支持 `IP:端口` |
| `--executable` | 省略或空白时从 PATH 查找 `pi` / `codex`；填写时使用指定文件 |
| `--prepare` | 可选的 Bash 环境准备命令，环境变量为 `A2ALINKER_PREPARE`；适用于 nvm、代理等 |
| `--skill-dir` | 省略时沿用原生技能发现规则；显式指定时直接加载附加目录，可重复，不复制或禁用其他原生技能 |
| `--agent-config` | 直接使用原生配置目录，或标准配置文件 `settings.json` / `config.toml` 的所在目录；省略时由 agent 自行发现 |
| `--model` / `--provider` | 沿用原生配置，填写时覆盖 |
| `--public-url` | 实际监听地址；反向代理部署时可以指定对外 origin |
| `--auth` | `none`；非回环监听必须设置 `bearer` |

端口占用时依次尝试 10101、10102 等，日志和 Agent Card 会显示实际地址。只有端口占用会触发递增，权限或地址错误会直接报错；`--listen 0` 由系统分配端口。

`--workspace` 决定 agent 在哪里读写文件和执行命令，`--storage-dir` 指定共享存储根目录。实例实际使用 `--storage-dir/{name}/`：

```text
~/.rein/storage/
├── Alan/
│   ├── a2alinker.db
│   └── instance.lock
└── Bob/
    ├── a2alinker.db
    └── instance.lock
```

不同名字可以共享根目录并同时启动，端口占用时自动递增。同一根目录内的同名实例使用相同目录锁，不能同时运行。重启时保留相同根目录和名字，即可读取该实例原有的任务与会话。名字必须是单个目录名，例如 Alan、Bob，不能包含路径分隔符或 `..` 路径。

启动时会提示“新实例”或“本地已有实例”。实例的 agent 类型和描述保存在该目录的 `a2alinker.db` 的 `instance` 表中，不新增配置文件。同一个名字绑定一种 agent：例如 Alan 首次使用 Pi，之后用 Codex 启动 Alan 会报错，应使用原 agent 或换名。

```bash
# 首次创建必须提供名字和描述
a2alinker serve pi --name Alan --desc "项目开发助手"
# 再次启动可以省略描述
a2alinker serve pi --name Alan
```

新实例缺少 `--desc`，或描述只有空白字符时，会直接报错。旧版本未保存描述，首次升级启动需补充一次 `--desc`；程序会核对旧任务和原生进程目录中的 agent 类型，发现不一致或无法确定类型时会报错，不会覆盖原有实例。

每个参数还支持对应的环境变量，命令行优先；完整列表见 `serve --help`。例如：

```bash
A2ALINKER_NAME=Alan A2ALINKER_DESC="项目开发助手" A2ALINKER_WORKSPACE=/work/project a2alinker serve pi
```

## 沿用本地 agent

默认不覆盖 Pi 的 `PI_CODING_AGENT_DIR`、Codex 的 `CODEX_HOME`，也不探测、复制或改写原生配置、认证和技能文件。原生 agent 按自己的环境变量和默认规则读取配置；通常分别使用 `~/.pi/agent` 和 `~/.codex`。项目技能、扩展、发现与信任规则都交给 agent 管理。

显式 `--agent-config` 会直接把指定目录作为原生配置目录。指定标准配置文件时使用其所在目录，不支持通过复制重命名来加载任意文件名；配套的认证、模型等文件继续按 agent 的目录规则读取。Codex 的沙箱与审批规则默认沿用原生配置，审批请求通过 A2A 转发给客户端。库调用方可以显式覆盖执行策略。

显式 `--skill-dir` 只把原目录传给 Pi 的 `--skill` 或 Codex 的 `skills/extraRoots/set`，不生成技能快照，不改写技能启用和禁用设置。技能内容和格式由原生 agent 处理。技能加载并不是文件权限隔离。

新 Pi 会话不指定 `--session`，由 Pi 选择原生保存位置，a2alinker 从 `get_state.sessionFile` 记录 `contextId` 到实际路径的映射；继续会话时按该路径恢复。Codex 使用原生 thread ID。a2alinker 不再创建实例私有的 `home/`、`skills/` 或 `sessions/` 目录。

模型、技能及其他执行设置变化后，可以继续原 `contextId`；不再计算或校验配置指纹。新配置何时生效遵循原生 agent 的行为，重新启动服务可以让新创建或恢复的进程使用当前启动选项。数据库保留旧 `fingerprint` 列用于兼容已有数据，但不再用它拒绝续聊。

旧版本生成的目录和会话文件不自动搬动、复制或删除。旧 Pi 会话继续按数据库中的原路径恢复；旧 Codex 会话若保存在原私有 home 中，需显式把 `--agent-config` 指向该目录，原生 agent 才能找到这些 thread。

如果 agent 需要先切换 nvm 或设置代理，可使用 `--prepare`：

```bash
a2alinker serve pi --name Alan --desc "项目开发助手" \
  --prepare 'source "$HOME/.nvm/nvm.sh" --no-use && nvm use --silent 24.20.0'

a2alinker serve codex --name Bob --desc "代码审查助手" \
  --prepare 'export HTTPS_PROXY=http://127.0.0.1:7890 HTTP_PROXY=http://127.0.0.1:7890'

# 多个准备动作使用 && 连接；也可以 source 自己的环境脚本
a2alinker serve pi --name Alan --prepare 'source /path/to/agent-env.sh'
```

准备命令目前使用 Unix 的 `/bin/bash`，不自动读取 shell 启动文件；初始工作目录为 `--workspace`。每次创建原生进程时，先在 Bash 中执行准备命令，再通过 `exec` 启动 `--executable`（省略时为 `pi` / `codex`）。两者共享环境，因此需要用 `export` 导出变量；只是单独执行一个脚本，脚本内的环境修改不会传回当前 Bash，应使用 `source`。

准备命令的非零退出状态会阻止 agent 启动，多步骤中需要失败即停止的操作请用 `&&` 连接。准备阶段不能读取协议 stdin，其输出不会混入 JSONL stdout，也不会原样写入日志。启动超时和进程组清理同样覆盖准备阶段。版本探测、启动预检和之后创建的新会话进程都会各自执行准备；复用现有会话时不会重复执行。

`--prepare` 只改变原生子进程环境，不修改 a2alinker 或当前终端的环境，也不改变 a2alinker 在启动前发现原生配置文件的规则。只需代理时，也可以在启动 a2alinker 前设置 `HTTPS_PROXY` 等环境变量，子进程会继承。

本地便捷启动可执行 `./run.sh`：默认名字为 Alan，工作目录为项目根目录，准备动作会加载 nvm 并执行 `nvm use node`。脚本使用已构建的 release 二进制，支持追加参数，例如 `./run.sh --name Bob --desc "代码审查助手" --listen 10101`。

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
