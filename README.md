# a2alinker

使用 Rust 将本地 pi、Codex 包装成 A2A 服务。每个实例绑定一种 agent、一套执行配置和 skills；实例内管理多个独立会话，串行执行任务。

```text
A2A client → HTTP JSON-RPC / SSE → a2alinker → Pi RPC / Codex app-server
```

当前实现 A2A 1.0 JSON-RPC 绑定，使用固定版本的 `ra2a` 协议类型。底层 agent 仍作为独立子进程运行。

## 快速开始

需要 Rust 1.88+、已安装并配置认证的 pi 或 Codex。pi 1.3.0 需要 Node.js 22.19+。集成测试另外使用 Python 3。主要支持 Linux、macOS。

```bash
cargo build --release --locked

# 检查配置和选中的技能，不启动 agent
./target/release/a2alinker config validate --config examples/codex.toml

# 检查版本、协议握手与技能加载，不发起模型推理
./target/release/a2alinker doctor --config examples/codex.toml

# 分别在 8080 和 8081 启动实例
./target/release/a2alinker serve --config examples/pi.toml
./target/release/a2alinker serve --config examples/codex.toml
```

也可以直接通过参数指定：

```bash
a2alinker serve \
  --agent codex \
  --agent-config /path/to/codex/config.toml \
  --workspace /work/project \
  --skill-dir /path/to/skills/common \
  --skill-dir /path/to/skills/review \
  --storage-dir /var/lib/a2alinker/reviewer \
  --listen 127.0.0.1:8081
```

`agents list` 查看内置适配器，`skills list --config ...` 列出实际选中的技能包。

若 pi 需要先执行 `nvm use` 才能找到，可使用 `doctor --config examples/pi-nvm.toml` 或 `serve --config examples/pi-nvm.toml`，通过配置自动选择 Node.js 版本。启动脚本、PATH 设置与错误排查见 [agent 启动环境](docs/agent-launch.md)。

## 配置

完整示例见 [pi.toml](examples/pi.toml) 和 [codex.toml](examples/codex.toml)。

- 优先级：命令行 > 对应的 `A2ALINKER_*` 环境变量 > TOML 配置 > 默认值。`--help` 显示各参数对应的环境变量。
- TOML 中的相对路径以配置文件所在目录为基准；命令行和环境变量中的路径以当前工作目录为基准。
- 重复的 `--skill-dir` 会替换配置文件中的目录列表；环境变量 `A2ALINKER_SKILL_DIRS` 使用分号分隔目录。
- `--agent-config` 对 Codex 可以是 `config.toml` 或专用配置目录，对 pi 可以是 `settings.json` 或专用配置目录。
- pi 省略 `agent.native_config` 时，自动读取 `PI_CODING_AGENT_DIR/settings.json`，未设置该变量则读取 `~/.pi/agent/settings.json`；`agent.env` 中的来源环境变量优先于网关进程环境。默认文件不存在时使用 pi 内置默认值，显式指定的文件不存在则报错。`doctor` 和 `config validate` 会显示实际配置来源。
- pi 会同时复制来源目录中的 `models.json`，保留自定义 provider 的模型、端点和 API 配置；它也参与执行指纹，副本权限为 `0600`。指定单个 settings 文件时使用其同目录的 models 文件，指定完整配置目录时一并复制。
- 原生配置复制到实例私有运行目录；配置内容或选中技能变化后，已有 context 不会静默换用新配置，需要新建 context。
- `agent.env` 可以配置子进程环境变量；推荐由部署环境注入密钥。服务不会自动读取 `.env`。
- `agent.inherit_auth = true` 会在私有运行目录尚无 `auth.json` 时复制当前 agent 的登录文件。不会复制用户的全部配置，也不会写回原始登录文件。API key 也可以使用 agent 原生环境配置。更换账号时应使用新的 storage 目录或停服后清理对应私有凭据。
- `agent.args` 用于显式传入原生扩展参数。请保留适配器所需的 RPC/stdio 模式。

每个实例使用独立 storage 目录。文件锁防止多个服务同时使用同一数据库。配置中的未知字段会报错。

### Skills 与能力声明

`skills.directories` 扫描包含 `SKILL.md` 的完整技能目录，保留其参考资料、脚本和资源。`enabled` 为空时选择这些目录中的全部技能，否则按名称筛选。

启动时校验 YAML frontmatter、重复名称及包内容，按内容摘要建立副本，并在 agent 握手时检查实际加载结果。技能包和原生配置目录内部的符号链接会被拒绝，单个技能包上限为 64 MiB。

`inherit_user = false` 默认关闭 agent 自动发现的用户、项目及系统技能，只加载显式选中的包；设为 `true` 则允许 agent 继续发现其他技能，`enabled` 仅筛选显式目录中的包。

`[[capabilities]]` 声明对外的 A2A AgentSkill，`required_skills` 校验其依赖的内部技能。技能文件与对外能力不会自动按名称一一映射。

技能、配置副本和白名单不是执行沙箱。Codex 的 `sandbox` 与 `approval_policy` 通过原生协议设置；pi 依赖其工具配置和操作系统权限，不提供等价的文件系统沙箱。pi 默认关闭自动加载的扩展，可通过 `allow_extensions` 或显式扩展参数启用。

## A2A 调用

发现能力：

```bash
curl http://127.0.0.1:8081/.well-known/agent-card.json
```

流式执行新消息（示例使用 `uuidgen` 每次生成新的请求 ID 和消息 ID）：

```bash
curl -N http://127.0.0.1:8081/a2a \
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
        "parts": [{"text": "Review this repository and summarize the main risks."}]
      }
    }
  }
JSON
```

外层 `id` 用于关联 JSON-RPC 请求与响应，`messageId` 用于本实现的持久化去重。每条新消息使用新 `messageId`；网络重试同一条消息时，应复用第一次生成的消息 ID 和消息内容。

新任务接受 `text/plain` 文本。图片、URL 和二进制输入目前返回不支持错误，服务不会自动下载远程文件。

| 方法 | 行为 |
| --- | --- |
| `SendMessage` | 默认等待完成或需要输入；`configuration.returnImmediately: true` 立即返回 |
| `SendStreamingMessage` | 首个事件是 Task，之后发送状态和产物事件 |
| `GetTask` | 查询状态、产物和历史，支持 `historyLength` |
| `ListTasks` | 按 context、状态、更新时间过滤；支持分页和选择是否带产物 |
| `CancelTask` | 取消排队或执行中的任务 |
| `SubscribeToTask` | 重新订阅未结束任务，首先返回当前完整快照 |

使用响应中的 `contextId` 创建后续任务，复用底层 agent 会话。`taskId` 用于查询、取消或回复正在等待输入的任务。终态 task 不会重开。

同一个 `messageId`、同一份消息内容重试会返回原任务；更改内容后复用该 ID 会报错。此规则包括审批回复，跨重启保留。修复模型或凭据后，若希望重新执行失败任务，必须提交新的消息 ID；仅修改外层 JSON-RPC `id` 或重启服务，都不会重新执行旧消息。

SSE 断开不会取消任务。重新调用 `SubscribeToTask` 会用当前 Task 快照恢复状态，然后接收后续事件。可以传入之前的 `Last-Event-ID` 进行游标归属校验；服务不会在最新快照后重放已经包含在快照里的旧增量。已经结束的任务使用 `GetTask` 查询。

审批与输入使用显式扩展，详见 [交互协议](docs/interaction.md)。普通聊天文本不会被解释成授权。

pi 默认返回上游公开的思考文本，以及工具名、参数、执行中的输出和最终结果。
思考使用独立的 `artifactUpdate`，工具过程使用 `statusUpdate` 中的结构化 `data`；
客户端可按 `metadata.a2alinker.kind` 和 `data.a2alinker.type` 分类展示。
`GetTask` 和重连快照也包含已保存的过程；字段、配置开关与客户端示例见
[执行过程协议](docs/execution.md)。

## 执行与恢复

- 每个实例只有一个执行槽位，避免同一工作目录并发写入；队列有容量限制，同一 context 只能有一个未结束任务。
- pi 使用 RPC，当前协议以 `agent_settled` 确认结束；旧版无 `willRetry` 的 `agent_end` 也可处理。Codex 使用 app-server 的 thread/turn 协议。
- 取消先发送原生中断；超时会释放会话并清理进程组。子进程退出、非法 JSONL 和输出超限会使任务失败，释放执行槽位。
- 默认任务超时 1800 秒、取消宽限 5 秒、启动握手超时 30 秒。等待用户交互的时间计入任务超时。
- SQLite 保存任务、消息去重、事件与底层会话标识；空闲会话释放进程，后续通过原生会话恢复。
- pi 会话文件位于 `storage.directory/sessions/`，一个 context 对应一个 JSONL 文件；同一会话的多轮任务共用该文件。pi 通常在首次 assistant 消息后才写入新文件，启动预检与 `doctor` 不会生成完整对话记录。
- 网关重启会将未结束的任务标记失败，不会自动重跑可能产生副作用的操作。已完成任务、去重记录和可恢复的 context 保留。
- skill 副本与 agent 私有配置均保存在 storage 中；生产部署应持久化整个目录。

## 部署

默认只监听回环地址。非回环地址强制要求 bearer 认证：

```toml
[server]
listen = "0.0.0.0:8081"
public_url = "https://reviewer.example.com"

[auth]
mode = "bearer"
token_env = "A2ALINKER_TOKEN"
```

通过环境变量提供至少 32 字节的 token，客户端使用 `Authorization: Bearer ...`。Agent Card 和 `/healthz` 公开；任务接口需要认证。一个实例对应一个信任域，当前不提供多租户、按用户分配 token 或任务所有权隔离。

TLS 可由反向代理终止。代理应关闭 SSE 缓冲，并允许长连接。浏览器跨域访问默认未开启 CORS；Rein Console 可通过同源后端或开发代理连接 `/a2a`。

`SIGINT`、`SIGTERM` 会停止接收任务，取消执行并持久化最终状态。日志写入 stderr，`RUST_LOG=a2alinker=debug` 可调整级别。agent stderr 持续消费但不转发，避免将原生输出中的凭据带入服务日志。

默认 INFO 日志带中文流程说明，覆盖请求、去重、任务状态、会话复用、产物和 SSE 生命周期；DEBUG 增加事件游标及原生命令名称，不输出提示词或回复正文。新手可以按照 [日志阅读指南](docs/logging.md) 对照 curl 请求观察 A2A 的执行过程。

## 开发

不熟悉 Rust 时，可先读 [Rust 代码阅读指南](docs/rust-guide.md)，再按推荐顺序阅读源码中的中文注释。本包通过 `unsafe_code = "forbid"` 禁止自身源码和测试使用 unsafe；此限制不覆盖第三方依赖的内部实现。

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

集成测试运行独立子进程 fixture，覆盖两种真实协议的报文形状、会话恢复、审批、取消、超时、崩溃、去重、持久化和 SSE；不会使用真实模型或消耗推理额度。`doctor` 用于本机安装版本的真实握手验证。

开发验证版本：Codex CLI 0.162.1、pi 1.3.0（Node.js 24.19.0）。真实运行时已验证携带技能包的握手；完整任务生命周期由子进程协议测试覆盖，未对真实模型发起推理。

模块边界和新增适配器说明见 [架构文档](docs/architecture.md)。本版本不包含 gRPC、HTTP+JSON 绑定、推送通知、远程适配器加载或并发 worktree 调度。

参考：[A2A 1.0](https://a2a-protocol.org/v1.0.0/specification/)、[Codex app-server](https://developers.openai.com/codex/app-server)、[pi RPC](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md)。
