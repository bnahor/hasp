#!/usr/bin/env python3
"""A scripted `codex app-server`: shapes as in its generated JSON schema."""
import json, sys

def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

def note(method, params):
    send({"method": method, "params": params})

total_in = 0
while True:
    m = read()
    if m is None: break
    meth, mid = m.get("method"), m.get("id")
    if meth == "initialize":
        send({"id": mid, "result": {"userAgent": "mock"}})
    elif meth == "thread/start":
        send({"id": mid, "result": {"thread": {"id": "th-1"}, "model": "mock-5"}})
    elif meth == "thread/resume":
        send({"id": mid, "error": {"code": -1, "message": "no such thread"}})
    elif meth == "turn/start":
        text = m["params"]["input"][0]["text"]
        send({"id": mid, "result": {"turn": {"id": "tu-1", "status": "inProgress"}}})
        note("turn/started", {"threadId": "th-1", "turn": {"id": "tu-1"}})
        if text == "hang":
            while True:
                c = read()
                if c and c.get("method") == "turn/interrupt":
                    send({"id": c["id"], "result": {}})
                    note("turn/completed", {"threadId": "th-1", "turn": {"id": "tu-1", "status": "interrupted"}})
                    break
            continue
        note("item/started", {"item": {"type": "agentMessage", "id": "m1", "text": ""}})
        note("item/agentMessage/delta", {"itemId": "m1", "delta": "on it"})
        note("item/started", {"item": {"type": "commandExecution", "id": "c1", "command": "cargo test"}})
        send({"id": 0, "method": "item/commandExecution/requestApproval",
              "params": {"threadId": "th-1", "turnId": "tu-1", "itemId": "c1", "startedAtMs": 0}})
        r = read()
        ok = r["result"]["decision"] in ("accept", "acceptForSession")
        note("item/completed", {"item": {"type": "commandExecution", "id": "c1", "command": "cargo test",
              "aggregatedOutput": "passed" if ok else "", "exitCode": 0 if ok else None,
              "status": "completed" if ok else "declined", "durationMs": 7}})
        total_in += 100
        note("thread/tokenUsage/updated", {"tokenUsage": {"total": {"inputTokens": total_in, "cachedInputTokens": 40, "outputTokens": 9}}})
        note("item/started", {"item": {"type": "agentMessage", "id": "m2", "text": ""}})
        note("item/completed", {"item": {"type": "agentMessage", "id": "m2", "text": "all green" if ok else "skipped"}})
        note("turn/completed", {"threadId": "th-1", "turn": {"id": "tu-1", "status": "completed"}})
    elif mid is not None:
        send({"id": mid, "result": {}})
