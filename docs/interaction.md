# 用户输入与审批扩展

扩展 URI：`urn:a2alinker:interaction:v1`。

这是一个可选的 A2A 扩展，用于转发受支持的原生审批和用户输入请求。
网关不会将普通文本解释为审批。在启用实例认证时，只有持有实例凭据的调用方
才能读取任务或回复交互请求。每个实例对应一个信任域。

## 接收交互请求

agent 因等待交互而暂停时，任务进入 `TASK_STATE_INPUT_REQUIRED` 状态。
可以从 `task.metadata.pendingInteraction` 或状态消息的 `data` 内容块中
获取交互请求：

```json
{
  "id": "interaction-id",
  "kind": "approval",
  "method": "item/commandExecution/requestApproval",
  "params": {
    "threadId": "native-thread-id",
    "turnId": "native-turn-id",
    "itemId": "native-item-id",
    "command": "echo example"
  }
}
```

客户端应向用户展示待审批的操作或需要回答的问题。`params` 的格式由原生方法
决定。本版本不支持同时处理多个原生交互请求，遇到这种情况会明确报错。
等待用户交互的时间计入任务超时。

## 回复交互请求

向 `/a2a` 发送 POST 请求，携带以下请求头；如果实例启用了认证，还需要携带
相应的认证凭据：

- `Content-Type: application/json`
- `A2A-Extensions: urn:a2alinker:interaction:v1`

请求体示例：

```json
{
  "jsonrpc": "2.0",
  "id": "reply-request-1",
  "method": "SendMessage",
  "params": {
    "message": {
      "messageId": "unique-reply-message-id",
      "role": "ROLE_USER",
      "taskId": "a2a-task-id",
      "parts": [{
        "data": {
          "interactionId": "interaction-id",
          "response": {"decision": "accept"}
        }
      }]
    }
  }
}
```

网关会先校验任务与会话的对应关系、待处理的交互 ID，以及响应格式，再记录
回复，并将其转发给对应的原生请求。使用相同的消息 ID 和内容重试时，会返回
原任务，不会再次批准操作。使用新消息 ID 提交已过期、ID 不匹配或已经处理过的
交互回复时，会被拒绝。需要开始新一轮原生执行时，应创建新任务，并沿用已有的
`contextId`。

## 支持的响应格式

| 原生方法 | `response` 格式 |
| --- | --- |
| Codex `item/commandExecution/requestApproval` | `{"decision":"accept"}`、`{"decision":"decline"}` 或 `{"decision":"cancel"}` |
| Codex `item/fileChange/requestApproval` | 与命令执行审批相同 |
| Codex `item/tool/requestUserInput` | `{"answers":{"question-id":{"answers":["answer"]}}}`；必须回答所有问题 |
| pi `extension_ui_request`，方法为 `confirm` | `{"confirmed":true}` 或 `{"confirmed":false}` |
| pi 方法 `select`、`input`、`editor` | `{"value":"answer"}` 或 `{"cancelled":true}` |

本扩展不支持对整个会话授予权限，也不支持修改权限策略。遇到不支持的 Codex
服务端请求时，会返回协议错误并将任务标记失败，不会静默批准。

需要持续跟踪任务时，可以保持事件流连接、重新调用 `SubscribeToTask`，
或通过 `GetTask` 轮询。任务处于这些等待交互的状态时，事件流连接保持打开；
任务进入终态后，连接关闭。
