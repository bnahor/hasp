#!/usr/bin/env python3
"""A scripted `claude -p --input-format stream-json` session."""
import json, sys

def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

turn = 0
while True:
    m = read()
    if m is None: break
    if m.get("type") == "control_request":
        if m["request"]["subtype"] == "interrupt":
            continue
        send({"type": "control_response", "response": {"subtype": "success", "request_id": m["request_id"], "response": {}}})
        continue
    if m.get("type") != "user": continue
    turn += 1
    text = m["message"]["content"]
    send({"type": "system", "subtype": "init", "session_id": "sess-9", "model": "mock-model"})
    if text == "hang":
        while True:
            c = read()
            if c and c.get("type") == "control_request" and c["request"]["subtype"] == "interrupt":
                send({"type": "result", "subtype": "error_during_execution", "is_error": True, "session_id": "sess-9", "usage": {}})
                break
        continue
    send({"type": "stream_event", "event": {"type": "message_start"}})
    send({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "working"}}})
    send({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "tu1", "name": "Bash", "input": {"command": "make test"}}]}})
    send({"type": "control_request", "request_id": "p1", "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "make test"}}})
    r = read()
    allowed = r["response"]["response"]["behavior"] == "allow"
    send({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "tu1",
          "content": [{"type": "text", "text": "ok" if allowed else "denied"}], "is_error": not allowed}]}})
    send({"type": "stream_event", "event": {"type": "message_start"}})
    send({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "finished"}}})
    send({"type": "result", "subtype": "success", "is_error": False, "session_id": "sess-9",
          "usage": {"input_tokens": 3, "output_tokens": 4, "cache_read_input_tokens": 10, "cache_creation_input_tokens": 1}})
