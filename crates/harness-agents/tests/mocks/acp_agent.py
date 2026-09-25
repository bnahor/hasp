#!/usr/bin/env python3
"""A scripted ACP agent: streams a reply, asks permission for one tool call,
reports what the client decided, and honours session/cancel."""
import json, sys

def send(o):
    o["jsonrpc"] = "2.0"
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()

def update(sid, u):
    send({"method": "session/update", "params": {"sessionId": sid, "update": u}})

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

while True:
    m = read()
    if m is None: break
    meth = m.get("method")
    if meth == "initialize":
        send({"id": m["id"], "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": False}}})
    elif meth == "session/new":
        send({"id": m["id"], "result": {"sessionId": "s-1"}})
    elif meth == "session/prompt":
        sid = m["params"]["sessionId"]
        text = m["params"]["prompt"][0]["text"]
        if text == "hang":
            # Wait for the client's cancel, then end the turn as cancelled.
            while True:
                c = read()
                if c and c.get("method") == "session/cancel":
                    send({"id": m["id"], "result": {"stopReason": "cancelled"}})
                    break
            continue
        update(sid, {"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}})
        update(sid, {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "rm -rf build", "kind": "execute", "status": "pending"})
        send({"id": 99, "method": "session/request_permission", "params": {"sessionId": sid,
              "toolCall": {"toolCallId": "t1", "title": "rm -rf build"},
              "options": [{"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                          {"optionId": "no", "name": "Reject", "kind": "reject_once"}]}})
        r = read()
        chosen = r["result"]["outcome"].get("optionId")
        status = "completed" if chosen == "yes" else "failed"
        update(sid, {"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": status,
                     "content": [{"type": "content", "content": {"type": "text", "text": "chose " + str(chosen)}}]})
        update(sid, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "done: "}})
        update(sid, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": status}})
        send({"id": m["id"], "result": {"stopReason": "end_turn", "usage": {"inputTokens": 5, "outputTokens": 2}}})
