# 思考与工具执行过程

pi 通过 RPC 的 stdout 主动发送过程事件；a2alinker 实时转换为 A2A SSE，
不依赖轮询 session 文件。当前 pi 适配器支持本页的思考与工具事件；
Codex 适配器继续提供回答、普通进度和审批交互。

## 配置与能力发现

以下开关默认均为 `true`，可以独立关闭：

```toml
[runtime]
forward_reasoning = true
forward_tools = true
max_output_bytes = 8388608
```

只转发模型提供方经 pi 公开输出的思考文本或摘要；上游没有输出时就没有该事件。
不提取隐藏推理，不返回 `thinkingSignature`、签名或加密内容。
关闭的过程类型既不返回客户端，也不写入网关的任务及事件数据库；pi 自己的
原生 session 文件仍由 pi 管理。

pi 的 Agent Card 在启用任一开关时声明可选扩展
`urn:a2alinker:execution:v1`，`params.reasoning`、`params.tools` 表示启用情况。
本扩展使用 A2A 的 Artifact、状态消息和 data Part；下文的 `kind`、`type` 等
字段是 a2alinker 的约定。客户端无需额外请求头即可收到，普通 A2A 客户端
可以忽略不认识的元数据与 data Part。

## 思考文本与回答

思考使用独立 Artifact，`metadata.a2alinker.kind = "reasoning"`。
每条 assistant 消息的每个思考块有独立 ID，例如 `reasoning-pi-1-0`。
最终回答的 ID 仍为 `output-pi-2` 等，标记 `kind = "answer"`。

以下为 SSE `data:` 中的 JSON 示例，省略了 JSON-RPC 外层：

```json
{
  "artifactUpdate": {
    "taskId": "task-id",
    "contextId": "context-id",
    "artifact": {
      "artifactId": "reasoning-pi-1-0",
      "parts": [{"text": "检查仓库结构。"}],
      "metadata": {"a2alinker": {"kind": "reasoning"}},
      "extensions": ["urn:a2alinker:execution:v1"]
    },
    "lastChunk": true
  }
}
```

客户端以 `artifactId` 为键保存文本。`append: true` 时追加；`append` 为 false
或省略时，使用本次文本替换。最后一个完整文本块是权威结果，不能再追加一次。
`lastChunk: true` 只表示该 Artifact 结束，Task 可能还需要继续执行工具。
只有思考或工具调用的 assistant 消息不会产生空的回答 Artifact。
思考产物保存在 `Task.artifacts`，最终回答的聊天历史仅包含回答文字。

## 工具开始、执行输出和结束

工具过程位于 `statusUpdate.status.message.parts`。文字 Part 提供简要说明，
data Part 携带结构化记录。例如 bash 开始执行：

```json
{
  "statusUpdate": {
    "taskId": "task-id",
    "contextId": "context-id",
    "status": {
      "state": "TASK_STATE_WORKING",
      "message": {
        "messageId": "tool-message-id",
        "taskId": "task-id",
        "contextId": "context-id",
        "role": "ROLE_AGENT",
        "parts": [
          {"text": "Running tool: bash"},
          {"data": {"a2alinker": {
            "kind": "tool",
            "type": "tool_call",
            "toolCallId": "call-123",
            "name": "bash",
            "state": "running",
            "arguments": {"command": "ls -la"}
          }}}
        ],
        "extensions": ["urn:a2alinker:execution:v1"]
      }
    }
  }
}
```

三种 `data.a2alinker.type` 的含义：

| type | state | 内容 |
| --- | --- | --- |
| `tool_call` | `running` | 工具名、原生 toolCallId、完整参数 |
| `tool_update` | `running` | 执行期间的累计结果 `result` |
| `tool_result` | `completed` 或 `failed` | 最终 `result`、`isError`，并保留开始时的参数 |

结果保留 pi 原生结构，例如：

```json
{
  "kind": "tool",
  "type": "tool_result",
  "toolCallId": "call-123",
  "name": "bash",
  "state": "completed",
  "arguments": {"command": "ls -la"},
  "result": {
    "content": [{"type": "text", "text": "文件A\n文件B\n"}],
    "details": {"exitCode": 0}
  },
  "isError": false
}
```

`result.content` 按 pi 实际返回的内容块传递，`details` 为工具相关信息；并非每种
工具都会提供 `exitCode`。pi 提供的截断信息也会保留。对于 bash 文本，客户端可
展示 `result.content` 中 `type = "text"` 的 `text`。

用 `(taskId, toolCallId)` 关联调用，支持多个交错执行的工具。`tool_update` 的
`result` 是截至该次更新的累计快照，应替换原结果，不能当作增量追加。
工具执行失败只表示这次调用失败；agent 可以根据错误继续工作，Task 是否成功
以最终任务状态为准。工具后台通知不会使等待用户输入的任务恢复成 WORKING。

## 查询、重连和输出限额

`GetTask` 的 `artifacts` 包含思考与回答，`history` 中每个工具调用保留一条最新
快照，更新时沿用同一个 `messageId`。因此工具从 running 变成 completed 后，
history 中对应记录也更新为最终结果，不堆积每个累计输出版本。
`SubscribeToTask` 首先提供这些最新快照，之后再发送新事件。客户端应先恢复
快照，再按 ID 更新。请求 `historyLength: 0` 会省略工具历史；需要完整过程快照
时请不要限制历史长度。已结束任务通过 `GetTask` 查询，重启后仍可读取。

思考、回答以及工具参数、结果的当前快照共同受 `runtime.max_output_bytes`
限制，超限时任务明确失败并释放会话，超限事件不落库。该值限制当前输出快照，
不代表整个事件数据库的大小上限。

工具参数和结果中的结构化凭据字段（如 `Authorization`、`api_key`、`password`）
递归替换为 `[REDACTED]`。shell 命令和工具输出等自由文本无法可靠识别凭据，
仍会按原文返回；需要限制过程可见性时可以关闭 `forward_tools`。
服务日志仅记录事件类型、工具名和关联 ID，不记录思考正文、工具参数或结果。
