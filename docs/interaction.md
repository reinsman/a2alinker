# Input and approval extension

URI: `urn:a2alinker:interaction:v1`.

This optional A2A extension forwards supported native approval/input requests. The
gateway never interprets ordinary text as an approval. Only a holder of the instance
credential can read tasks or respond. An instance is a single trust domain.

## Receive a request

When the agent pauses, the task enters `TASK_STATE_INPUT_REQUIRED`. The request is
available both in `task.metadata.pendingInteraction` and the status message's data
part:

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

Display the operation or question to the user. The `params` shape depends on the
native method. Multiple simultaneous native interactions fail explicitly in this
version. Waiting time counts toward the task timeout.

## Send a response

POST `/a2a` with `Content-Type: application/json`, the instance authorization if
enabled, and `A2A-Extensions: urn:a2alinker:interaction:v1`:

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

The gateway validates the task/context binding, outstanding interaction ID and
response shape before recording it and forwarding it to the native request ID.
Repeating the same message ID and content returns the task instead of granting the
operation again. A late, mismatched or duplicate interaction with a new message ID
is rejected. New native turns use a new task and the existing `contextId`.

## Supported response shapes

| Native method | `response` |
| --- | --- |
| Codex `item/commandExecution/requestApproval` | `{"decision":"accept"}`, `decline`, or `cancel` |
| Codex `item/fileChange/requestApproval` | Same choices |
| Codex `item/tool/requestUserInput` | `{"answers":{"question-id":{"answers":["answer"]}}}`; answer every question |
| Pi `extension_ui_request`, method `confirm` | `{"confirmed":true}` or `false` |
| Pi method `select`, `input`, `editor` | `{"value":"answer"}` or `{"cancelled":true}` |

Session-wide permission grants and policy amendments are intentionally absent from
this extension. Unsupported Codex server requests receive a protocol error and fail
the task rather than being silently approved.

For continued monitoring, keep the stream open, issue a new `SubscribeToTask`, or
poll `GetTask`. Streaming connections remain open through these interrupted states
and close at a terminal task state.
