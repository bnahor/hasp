# Harness vs the other harnesses

Measured 2026-09-20 on this machine (M5 Pro, 52 GB), release build, against the
coding agents actually installed here: pi, Claude Code, Codex CLI, opencode,
Gemini CLI and cursor-agent. Each tool was run the way it is configured on this
machine — latency therefore includes each tool's own default model, and that
confound is stated rather than hidden.

## Measured

| | cold start (`--version`) | one-shot "Reply OK" | peak RSS | install footprint |
| --- | --- | --- | --- | --- |
| **harness** | **3 ms** | **1.5 s** (deepseek) · 0.1 s (mock) | **16 MB** · 5 MB (mock) | **6.5 MB**, one static binary |
| Claude Code | 10 ms | 6.0 s → "OK" | 304 MB | 208 MB |
| Codex CLI | 9 ms | *(see note)* | 184 MB | 210 MB |
| pi | 149 ms | 4.9 s → "OK" | 250 MB | 434 MB package |
| opencode | 220 ms | *(see note)* | 807 MB | 137 MB |
| cursor-agent | 237 ms | *(see note)* | 247 MB | versioned dir |
| Gemini CLI | 733 ms | *(see note)* | 275 MB | JS package |

Notes: Codex initialized (model `gpt-6-astra`, 7.4 s of real work) but its API
rejected the call — that model needs a newer CLI than 0.149.1; not a harness
verdict. opencode hit a server error in its configured backend; Gemini CLI has
no auth configured here; cursor-agent refuses non-interactive runs in an
untrusted directory. Their startup and RSS are still measured fairly.

Reading: the native binaries all start in single-digit milliseconds; the
Node-based ones take 150–730 ms. For a whole one-shot turn — process, session
setup, provider round trip, reply — ours is 4× faster than pi and Claude Code
here and uses ~20× less memory than Claude Code's peak RSS. Some of that gap is
model choice (DeepSeek vs their defaults); the mock row (0.1 s, 5 MB) shows how
much is *not* the model.

## Capability surface (from each tool's `--help`, 2026-09-20)

All six advertise MCP support, some form of subagents/agent delegation, and
session resume — these are table stakes now. What differs in kind, as far as
this project's own docs and source can attest:

- **Ours**: daemon-**resident** sessions (schedules and in-flight turns survive
  the client), cron wall-clock heartbeats, MCP *server* mode (`--mcp-serve`) as
  well as client, git-worktree isolation with safe sweeps, append-only JSONL as
  the only state, one 6.5 MB binary with no runtime.
- **Theirs (as verified here)**: mature ecosystems around them — pi's extension
  and theme system, Claude Code's breadth of integrations, opencode's
  client/server architecture with third-party UIs, Codex's sandboxing. These are
  deeper than ours; a fair comparison does not pretend otherwise.

## What this comparison cannot say

Latency rows are model-entangled (each tool used its own default). No agentic
benchmark (SWE-bench Verified, Terminal-Bench) was run through any of these
harnesses here — that is the only kind of number that would compare *harness*
quality rather than startup economics, and it needs Docker, hours, and a real
budget per tool. Capability rows come from `--help` text, which advertises
rather than proves.
