# harness

A personal coding agent: a TUI attached to a daemon that owns one workspace at a
time, sessions as append-only JSONL, prompt-cache-disciplined requests, and a
lightweight agent bus for spawning child agents.

```sh
cargo build --release          # binary at target/release/harness
```

## Quickstart

```sh
cargo install --path crates/harness-cli   # from a checkout → ~/.cargo/bin/harness
harness --version                         # → harness 0.1.0
harness login                  # Anthropic key → ~/.harness/credentials.json (0600)
harness login deepseek         # …or DeepSeek (Anthropic-compatible endpoint, 1M ctx)
harness login zai              # …or a z.ai / GLM coding-plan key
harness doctor                 # what's configured, what's missing

harness                        # TUI (auto-spawns the daemon and attaches)
harness --plain                # line REPL, embedded (no daemon)
harness -p "explain src/lib.rs"  # one-shot, prints the answer
harness --provider mock        # keyless: echoes prompts, for poking at the UI
```

The install command is the whole story: the root of this repo is a virtual
workspace, so point `cargo install` at `crates/harness-cli` (not `.`). After
that, `harness update` rebuilds and reinstalls from the checkout (remembered
in `~/.harness/checkout`, so it works from the installed binary too),
`harness update --models` refreshes the model catalog, and `harness doctor`
always shows the running binary's path and whether it is the one on `PATH`.

Inside the TUI:

| command | what it does |
| --- | --- |
| `/goal <text> [--budget N\|--unlimited]` | persistent objective; the harness keeps prompting itself until the model calls `goal_complete`. Defaults to a 2M-token budget |
| `/goal status\|pause\|resume\|clear` | as you'd expect |
| `/autonomous on [--max-turns N] [--max-tokens N] [--gate "cmd"]` | keep working until the gate command passes or a limit trips |
| `/heartbeat every 10m <text>` | recurring nudge; `in 30m <text>` one-shot; `cron 0 9 * * 1-5 <text>` wall-clock, local time; `list`, `pause`, `resume`, `clear` |
| `/compact`, `/branch [n]`, `/fork`, `/resume [n]` | context and session control |
| `/worktree [name]`, `/worktree-exit [rm]` | isolate the session in a git worktree |
| `/model [id]`, `/stats`, `/name <text>`, `/new`, `/clear`, `/exit` | the rest |

Keys: `enter` send · `ctrl+j` newline · `esc` cancel a turn · `↑`/`↓` history ·
`pgup`/`pgdn` scroll · `ctrl+c` exit.

## Agent bus

The model can spawn child agents with the `spawn` tool. Children get base tools
only (read/write/edit/bash) — one level deep, by construction — and a child that
declares `mutation: true` runs in its own git worktree (`harness/<name>`).
Children never block the parent: exits arrive as notices between tool calls, and
`message` supports follow-ups with `delivered`/`queued` receipts.

A child that declared `mutation: true` leaves its worktree and branch behind so
you can inspect the work. To clean up:

```sh
harness worktrees                      # name, branch, and whether it is merged
harness worktrees prune <name>         # refuses unmerged work; --force overrides
```

The branch always survives a prune, so a forced cleanup is still recoverable with
`git branch -D harness/<name>` once you are sure.

## MCP servers

Add tools from any MCP server (stdio). Project file wins over user file:

```json
// .harness/mcp.json  (or ~/.harness/mcp.json)
{"servers": {"git": {"command": "uvx", "args": ["mcp-server-git"],
                    "env": {"GIT_DIR": "."}}}}
```

Remote tools appear to the model as `mcp__<server>__<tool>` with their schemas
unchanged, so the tool list stays cache-stable. A server that fails to start is
reported and skipped — it never blocks startup.

The harness can also *be* a server, so other agents can use its tools:

```sh
harness --mcp-serve     # MCP over stdio, tools: read, write, edit, bash
```

That serves the same tool layer, so workspace confinement (relative paths only,
no `..`, symlink checks) and the bash metacharacter rule still apply. Because
there is no human attached to approve anything, writes and non-allowlisted
commands are refused unless you set `HARNESS_MCP_YOLO=1`. Point a client at it
with `{"servers": {"harness": {"command": "…/harness", "args": ["--mcp-serve"],
"cwd": "/path/to/project"}}}`.

## Where state lives

- `~/.harness/sessions/<workspace-key>/*.jsonl` — append-only session trees
  (messages, compactions, goals, heartbeats, model changes), written, never
  mutated. `record` entries are the canonical state; restarting resumes the tail.
- `~/.harness/credentials.json` — API keys, mode 0600.
- `<project>/.harness/artifacts/<run>/` — truncated tool output that the model can
  page back in with `read`.
- `<project>/.harness/traces/*.jsonl` — per-turn API and tool timings.

## Daemon

The TUI attaches to a daemon by default, and sessions are **daemon-resident**: a
client is a view, not the owner. Closing the TUI detaches it; the session keeps
running, so goals, heartbeats and any in-flight turn continue. Output written
while nobody is attached is dropped rather than lost — the journal is canonical,
so the next client to attach sees all of it. Starting the TUI in that workspace
rejoins the running session, and a second client joins the same one rather than
being refused. `session/new` is refused while a live session holds the
workspace (use `harness shutdown` to end it), because quietly evicting a session
that may be mid-goal would be worse. Approval questions with no client attached
are denied immediately instead of waiting for a client that is gone.

`--no-daemon` runs everything embedded, `--serve` runs the daemon in the
foreground. Transport is newline-delimited JSON-RPC 2.0 over `$HARNESS_SOCKET`
or `~/.harness/daemon.sock`.

## Environment

`ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL`, `ZAI_API_KEY`, `DEEPSEEK_API_KEY`,
`HARNESS_PROVIDER` (`anthropic` | `zai` | `deepseek` | `ollama` | `compat` |
`mock`), `HARNESS_SOCKET`, `HARNESS_MOCK_DELAY_MS`.

**Any other model or vendor, with no code change**: `--provider compat` speaks
the OpenAI wire format to whatever `HARNESS_COMPAT_URL` points at (plus optional
`HARNESS_COMPAT_KEY`) — Ollama and vLLM/LM Studio locally, OpenRouter/Groq and
most new vendors remotely. `--provider ollama` defaults to
`http://127.0.0.1:11434/v1` (`HARNESS_OLLAMA_URL` overrides; windows via
`HARNESS_OLLAMA_WINDOW`/`HARNESS_COMPAT_WINDOW`). Model lists are discovered
from the endpoint — `harness models ollama` shows what `ollama pull` gave you —
and `--model` accepts any id they serve. Without `--model`, the first served
model is used.

Providers other than Anthropic are driven through their Anthropic-compatible
endpoints, so the same client, streaming, tool-calling and cache breakpoints
apply. DeepSeek defaults to `deepseek-v4-pro`; `--model deepseek-flash` is ~4×
cheaper.

## Tests

```sh
cargo test --workspace     # unit, e2e over a real socket, PTY-driven TUI tests
cargo fmt --all --check
```
