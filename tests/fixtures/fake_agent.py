#!/usr/bin/env python3
"""Deterministic subprocess fixture implementing both agents' actual wire shapes.

This is used only by integration tests, never by the production adapter registry.
"""
import json
import os
import pathlib
import re
import subprocess
import sys
import threading
import time
import uuid

if "--version" in sys.argv:
    print("fixture-agent 1.0")
    sys.exit(0)

PI = "--mode" in sys.argv
LOCK = threading.Lock()
CANCEL = threading.Event()
ANSWER = threading.Event()
THREAD = None
TURN = None
PENDING = None
ROOTS = []
ENABLED = {}
STARTUP_TRACE = os.environ.get("A2ALINKER_TEST_TRACE")
if STARTUP_TRACE:
    with open(STARTUP_TRACE, "a") as f:
        f.write(json.dumps({"pid": os.getpid(), "startup": {"args": sys.argv[1:]}}) + "\n")
if PI:
    SESSION = pathlib.Path(sys.argv[sys.argv.index("--session") + 1])
else:
    HOME = pathlib.Path(os.environ["CODEX_HOME"])


def emit(value):
    with LOCK:
        print(json.dumps(value, ensure_ascii=False), flush=True)


def response(request, result=None):
    if PI:
        emit({"id": request.get("id"), "type": "response", "command": request["type"], "success": True, "data": result})
    else:
        emit({"id": request["id"], "result": result or {}})


def note(method, params):
    emit({"method": method, "params": {"threadId": THREAD, **params}})


def count_turn():
    path = SESSION if PI else HOME / ("fixture-" + THREAD + ".json")
    if path.exists():
        count = json.loads(path.read_text()).get("turns", 0)
    else:
        count = 0
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"turns": count + 1}))
    return count + 1


def finish(canceled=False, error=None):
    if PI:
        emit({"type": "agent_end", "messages": [], "willRetry": False})
        emit({"type": "agent_settled", "aborted": canceled})
    else:
        note("turn/completed", {"turn": {"id": TURN, "status": "interrupted" if canceled else "failed" if error else "completed", "error": {"message": error} if error else None}})


def execution(prompt):
    """Public execution notifications; no actual shell command or model request."""
    emit({"type": "message_start", "message": {"role": "assistant", "content": []}})
    thinking = "x" * 2048 if prompt == "reasoning-large" else "检查仓库结构。"
    emit({"type": "message_update", "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 0, "delta": thinking[:3]}})
    emit({"type": "message_update", "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 0, "delta": thinking[3:]}})
    emit({"type": "message_update", "assistantMessageEvent": {"type": "thinking_end", "contentIndex": 0, "content": thinking}})
    # Second block deliberately has no thinking_end, exercising message_end fallback.
    emit({"type": "message_update", "assistantMessageEvent": {"type": "thinking_delta", "contentIndex": 1, "delta": "再查看"}})
    emit({"type": "message_end", "message": {"role": "assistant", "stopReason": "toolUse", "content": [
        {"type": "thinking", "thinking": thinking, "thinkingSignature": "SIGNATURE_MUST_NOT_BE_FORWARDED"},
        {"type": "thinking", "thinking": "再查看文件。"},
        {"type": "toolCall", "id": "call-bash", "name": "bash", "arguments": {"command": "ls -la"}},
    ]}})
    arguments = {"command": "x" * 2048 if prompt == "arguments-large" else "ls -la", "headers": {"Authorization": "TOOL_SECRET_MUST_BE_REDACTED"}}
    emit({"type": "tool_execution_start", "toolCallId": "call-bash", "toolName": "bash", "args": arguments})
    emit({"type": "tool_execution_start", "toolCallId": "call-read", "toolName": "read", "args": {"path": "missing.txt"}})
    for output in ["文件A\n", "文件A\n文件B\n"]:
        emit({"type": "tool_execution_update", "toolCallId": "call-bash", "toolName": "bash", "args": arguments,
              "partialResult": {"content": [{"type": "text", "text": output}], "details": {"truncation": None}}})
    if prompt == "execution-pause":
        CANCEL.wait(20)
        finish(canceled=True)
        return False
    emit({"type": "tool_execution_end", "toolCallId": "call-read", "toolName": "read", "isError": True,
          "result": {"content": [{"type": "text", "text": "File not found"}], "details": {"exitCode": 1}}})
    output = "x" * 2048 if prompt == "tool-large" else "文件A\n文件B\n"
    emit({"type": "tool_execution_end", "toolCallId": "call-bash", "toolName": "bash", "isError": False,
          "result": {"content": [{"type": "text", "text": output}], "details": {"exitCode": 0, "api_key": "RESULT_SECRET_MUST_BE_REDACTED"}}})
    return True


def run(prompt, count):
    global PENDING
    if prompt == "crash":
        os._exit(23)
    if prompt == "crash-with-child":
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        pathlib.Path(os.environ["A2ALINKER_TEST_CHILD_PID"]).write_text(str(child.pid))
        os._exit(23)
    if prompt == "malformed":
        with LOCK:
            print("not-json", flush=True)
        return
    if prompt in ("slow", "hang-ack"):
        CANCEL.wait(20)
        finish(canceled=True)
        return
    if prompt == "approval":
        PENDING = "fixture-ui" if PI else 42
        if PI:
            emit({"type": "extension_ui_request", "id": PENDING, "method": "confirm", "title": "Run fixture operation?"})
            emit({"type": "tool_execution_start", "toolCallId": "call-background", "toolName": "background-tool", "args": {}})
        else:
            emit({"id": PENDING, "method": "item/commandExecution/requestApproval", "params": {"threadId": THREAD, "turnId": TURN, "itemId": "command-1", "command": "echo fixture"}})
            note("item/started", {"turnId": TURN, "item": {"type": "commandExecution", "id": "background-tool"}})
        while not ANSWER.wait(0.01):
            if CANCEL.is_set():
                finish(canceled=True)
                return
    if prompt == "fail":
        if PI:
            emit({"type": "message_end", "message": {"role": "assistant", "content": [], "stopReason": "error", "errorMessage": "fixture provider failure"}})
        finish(error="fixture provider failure")
        return
    if prompt == "retry" and PI:
        emit({"type": "message_end", "message": {"role": "assistant", "content": [], "stopReason": "error", "errorMessage": "transient failure"}})
        emit({"type": "agent_end", "messages": [], "willRetry": True})
        time.sleep(0.05)
    if PI and prompt in ("execution", "execution-pause", "reasoning-large", "arguments-large", "tool-large"):
        if not execution(prompt):
            return
    text = "x" * 2048 if prompt == "large" else "hello\u2028world" if prompt == "unicode" else "Hello " + str(count)
    item = "message-" + str(count)
    if PI:
        emit({"type": "message_start", "message": {"role": "assistant", "content": []}})
        emit({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": text[:3]}})
        time.sleep(0.03)
        emit({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": text[3:]}})
        emit({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": text}], "stopReason": "stop"}})
    else:
        note("item/agentMessage/delta", {"turnId": TURN, "itemId": item, "delta": text[:3]})
        time.sleep(0.03)
        note("item/agentMessage/delta", {"turnId": TURN, "itemId": item, "delta": text[3:]})
        note("item/completed", {"turnId": TURN, "item": {"type": "agentMessage", "id": item, "text": text}})
    finish()


for line in sys.stdin:
    request = json.loads(line)
    trace = os.environ.get("A2ALINKER_TEST_TRACE")
    if trace:
        with open(trace, "a") as f:
            f.write(json.dumps({"pid": os.getpid(), "request": request}) + "\n")
    method = request.get("type") if PI else request.get("method")
    if method == "skills/extraRoots/set":
        ROOTS = request["params"]["extraRoots"]
        response(request)
    elif method == "skills/config/write":
        ENABLED[request["params"]["path"]] = request["params"]["enabled"]
        response(request)
    elif method == "initialize":
        response(request)
    elif method == "skills/list":
        skills = []
        for root in [HOME / "skills", *ROOTS]:
            for path in pathlib.Path(root).rglob("SKILL.md"):
                name = re.search(r"^name: (.+)$", path.read_text(), re.M).group(1)
                native_enabled = not re.search(r"^enabled: false$", path.read_text(), re.M)
                skills.append({"name": name, "path": str(path), "enabled": ENABLED.get(str(path), native_enabled)})
        response(request, {"data": [{"cwd": os.getcwd(), "skills": skills, "errors": []}]})
    elif method == "get_commands":
        paths = [pathlib.Path(sys.argv[i + 1]) for i, value in enumerate(sys.argv) if value == "--skill"]
        if "--no-skills" not in sys.argv:
            paths.extend((pathlib.Path(os.environ["PI_CODING_AGENT_DIR"]) / "skills").rglob("SKILL.md"))
        response(request, {"commands": [{"source": "skill", "name": "skill:" + re.search(r"^name: (.+)$", p.read_text(), re.M).group(1)} for p in paths]})
    elif method in ("thread/start", "thread/resume"):
        THREAD = request["params"].get("threadId", "fixture-" + str(uuid.uuid4()))
        response(request, {"thread": {"id": THREAD}})
    elif method == "get_state":
        if not SESSION.exists():
            SESSION.parent.mkdir(parents=True, exist_ok=True)
            SESSION.write_text('{"turns":0}')
        response(request, {"sessionFile": str(SESSION), "isStreaming": False})
    elif method in ("prompt", "turn/start"):
        CANCEL.clear()
        ANSWER.clear()
        prompt = request["message"] if PI else request["params"]["input"][0]["text"]
        TURN = str(uuid.uuid4())
        if prompt != "hang-ack":
            response(request, {"disposition": "started"} if PI else {"turn": {"id": TURN, "status": "inProgress"}})
        threading.Thread(target=run, args=(prompt, count_turn()), daemon=True).start()
    elif method in ("abort", "turn/interrupt"):
        CANCEL.set()
        if request.get("id") is not None:
            response(request)
    elif method == "clear_queue":
        if request.get("id") is not None:
            response(request)
    elif method == "extension_ui_response" or (not PI and request.get("id") == PENDING and "result" in request):
        ANSWER.set()
    elif method == "initialized":
        pass
    elif request.get("id") is not None:
        response(request)
