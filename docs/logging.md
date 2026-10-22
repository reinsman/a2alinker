# 通过日志理解 A2A

默认 INFO 日志记录主流程，并附中文说明。DEBUG 会进一步记录 SSE 事件类型和游标、文本增量的字节数，以及原生 agent 命令名称；两种级别都不记录提示词、回复正文、审批内容或配置中的密钥。

## 启动与日志级别

修改代码、重新构建后，在原终端按 Ctrl+C 停止旧进程，再启动新进程，已经运行的服务不会自动加载新的日志代码。

```bash
cargo build --release --locked
RUST_LOG=a2alinker=info ./target/release/a2alinker serve --config examples/pi-nvm.toml
```

需要观察逐条事件时：

```bash
RUST_LOG=a2alinker=debug ./target/release/a2alinker serve --config examples/pi-nvm.toml
```

使用已配置好 PATH 的普通 pi，可将配置换为 `examples/pi.toml`。`RUST_LOG` 会覆盖默认级别；如果外部设置为 warn，INFO 日志就不会显示。日志写入 stderr。

时间末尾的 `Z` 表示 UTC，例如 `13:51:10Z` 对应北京时间 `21:51:10`。

## 沿着一条消息阅读

下面的 event 字段可用于搜索同类日志；每条日志后面还会带上任务或请求标识。

| event | 含义 | 学习重点 |
| --- | --- | --- |
| `config.native` / `config.models` | 原生配置已复制 | 只显示来源路径；models.json 决定自定义模型与端点 |
| `gateway.config` | workspace、storage、技能和队列准备完成 | 一个实例绑定一套执行配置 |
| `gateway.preflight` | 原生协议及技能预检完成 | 未发送模型请求，不能据此判断套餐权限 |
| `a2a.endpoints` | 服务地址和协议 | GET Agent Card 发现能力，POST /a2a 调用方法 |
| `a2a.discovery` | 客户端读取 Agent Card | 此处不创建任务，也不调用模型 |
| `a2a.request` | 收到 JSON-RPC 方法 | 观察 request_id、method |
| `task.queued` | 新任务已保存，准备进入队列 | 记录 message_id → task_id → context_id 的对应关系 |
| `task.started` | worker 开始执行 | SUBMITTED → WORKING，不再排队 |
| `session.opening` / `session.ready` | 创建或恢复原生会话 | action=create/resume；原生会话 ID 与 A2A contextId 不同 |
| `session.reused` | 复用存活的原生会话 | 同一 context 的后续任务能接着对话 |
| `agent.run` | 向 agent 提交本轮消息 | 开始等待模型、工具和交互结果 |
| `artifact.started` / `artifact.complete` | 产物开始及完整文本到达 | 一个 Task 可以产生多个 Artifact |
| `task.state` / `interaction.pending` | 状态变化，或等待输入/审批 | INPUT_REQUIRED 不是完成，需要结构化回复 |
| `interaction.reply` | 回复已校验并转交 agent | 关联 interactionId，普通聊天文本不会代替审批 |
| `task.finished` | 终态已保存 | COMPLETED、FAILED、CANCELED 等，不会重开 |
| `task.execution_end` | 本轮执行耗时 | 包含会话准备和等待交互，不包含排队时间 |
| `task.query` / `task.list` | GetTask / ListTasks | 读取历史状态，不重新执行 |

`a2a_request{...}` 是异步请求关联信息。例如在 SendMessage 的处理期间，提交和等待相关日志会带上该请求的 request_id 和 method。后台 worker 的日志用 task_id 和 context_id 关联到提交日志。

## 区分各种 ID

- `request_id`：JSON-RPC 外层 id，关联一次请求与响应；不控制消息去重。
- `message_id`：messageId，客户端生成。本实现将它作为持久化去重键。
- `task_id`：服务器生成，标识一次执行，用于 GetTask、CancelTask 和订阅。
- `context_id`：会话标识，用于多轮对话；同一 context 可以先后执行多个 task。
- `native_session_id`：pi 的 JSONL 文件路径，或 Codex thread ID，由适配器管理。
- `interaction_id`：一次待处理输入或审批的标识，回复时必须匹配。
- `cursor`：数据库事件序号；SSE 的事件 id 格式是 `taskId:cursor`。

新消息生成新 messageId；网络重试同一条消息时保留 ID 和消息内容。命中 `message.duplicate` 时，日志会明确说明“返回原任务，不会重新调用 agent”，并显示原任务状态。修复故障后想重新执行，需要新 messageId；重启服务或只更换外层 request_id 不会清除去重记录。

## 理解 SSE 与任务状态

`sse.opened` 表示订阅建立。第一条事件始终是当前 Task 快照，然后才发送新产生的状态与产物事件。`a2a.response` 的 `streaming=true` 只说明 HTTP 流式响应已建立，不代表任务完成。

DEBUG 下的 `sse.event` 会显示 kind=task/statusUpdate/artifactUpdate 等和 cursor。`artifact.delta` 只记录累计输出字节数；完整文本到达时的 `artifact.complete` 会显示 append=false、last_chunk=true，表示使用完整文本更新产物。产物结束不等于整个 Task 结束，应继续观察终态。

`sse.closed` 的 reason 有：

- `task_terminal`：发送任务终态或终态快照后结束。
- `client_disconnected`：订阅被关闭，任务仍可继续。需要停止执行时调用 CancelTask。
- `storage_error`：读取持久化事件失败。

其中 events 是交给响应流的事件数量，不代表客户端已逐条确认接收。多个订阅会分别记录自己的 opened/event/closed。

## 失败、取消与资源清理

`a2a.error` 表示 JSON-RPC 请求错误，rpc_error_code 对应响应的 error.code。HTTP 401 的 `a2a.auth_rejected` 表示实例 bearer 认证失败。日志不打印原始请求和任意错误参数，具体协议错误仍返回调用方。

`task.finished` 的 FAILED 表示请求已形成任务，但执行失败。具体原因保存在 Task.status.message，请使用 GetTask 查询；例如上游模型的 403 套餐权限错误。失败任务会保留以供查询。

`task.cancel_requested` 表示收到 CancelTask；`agent.interrupt` 表示通知原生 agent 中断。`task.timeout` 是任务超时，`agent.interrupt_timeout` 表示中断超过宽限时间，随后清理进程。取消排队任务不会发起原生执行。

`session.released` / `session.evicted` 表示释放进程以处理失败、空闲或缓存上限，不删除已生成的会话文件。`gateway.shutdown` 和 `worker.stopped` 表示服务退出与清理。会话文件按 context 保存，不是每次服务启动或每条消息都新建一个文件。
