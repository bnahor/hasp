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

harness web                    # browser UI on 127.0.0.1 (prints a token URL)
harness --acp                  # Agent Client Protocol on stdio — editors (Zed, …) drive it
harness --mcp-serve            # serve the same tools over MCP on stdio
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
| `/theme light\|dark` | color palette (`HARNESS_THEME` sets the default); dark is the historical look |

Any `/name` that is not built in runs a prompt template (below); they show in
`/help` and tab-completion.

Keys: `enter` send · `ctrl+j` newline · `esc` cancel a turn · `↑`/`↓` history ·
`pgup`/`pgdn` scroll · `ctrl+c` exit.

## Tools

`read`, `write`, `edit`, `bash`, `grep`, `glob`, `todo`, `web_fetch`,
`symbol_search`, plus `skill` when skills are installed and the agent-bus
tools below.

- **`grep` / `glob`** use ripgrep's own walker in process: gitignore-aware,
  confined, capped, newest files first, and never an approval prompt.
- **Read-only calls run in parallel.** When the model batches independent
  reads and searches into one response, they run concurrently.
- **`esc` interrupts a running tool** (a hung `cargo test` included, whole
  process group). Every call still gets a result, so the transcript stays valid.
- `bash` keeps the head *and tail* of long output (where errors and test
  summaries are), keeps partial output on timeout, and survives non-UTF-8.
  `read` caps long lines and total size, refuses binaries, and lists
  directories. `edit` falls back to CRLF- and whitespace-tolerant matching,
  only when the match is unique.

## Commands, skills and hooks

Plain files, laid out so an existing Claude Code collection works unchanged.
Project beats home, `.harness` beats `.claude`:

- **Prompt templates**: `.harness/commands/review.md` (or `.claude/commands/…`,
  `~/.harness/commands/…`, `~/.claude/commands/…`) runs as `/review <args>`.
  `$ARGUMENTS` and `$1`…`$9` are substituted; `description:` front matter
  shows in completion; `git/pr.md` becomes `/git:pr`. They work in every front
  end (TUI, REPL, `-p`, ACP, web) because the engine expands them.
- **Skills**: `.harness/skills/<name>/SKILL.md` (or `.claude/skills/…`, and
  the home-directory twins) with `name:` and `description:` front matter. Only
  names and descriptions are in context. The model loads a skill's body, and
  any supporting file in its directory, through the `skill` tool when a task
  calls for it.
- **Hooks**: `~/.harness/hooks.json`, and `.harness/hooks.json` once you run
  `harness trust` in the project (a cloned repo's hooks never run on their
  own):

  ```json
  {"pre_tool":  [{"match": "bash", "command": "./scripts/policy.sh"}],
   "post_tool": [{"match": "edit|write", "command": "cargo fmt 2>&1 | tail -5"}],
   "stop":      [{"command": "cargo check -q 2>&1 | tail -20"}]}
  ```

  Hooks get a JSON event on stdin. Exit 2 is how a hook talks to the model: a
  `pre_tool` hook blocks the call (hooks can refuse, never approve), a
  `post_tool` hook appends feedback to the result, and a `stop` hook sends the
  model back to fix what it found (up to 3 times per turn).

`harness doctor` lists what it found.

## Sandbox

`HARNESS_SANDBOX=workspace-write` runs every `bash` command under the OS
sandbox (`sandbox-exec` on macOS, `bwrap` on Linux), so it can write only to
the workspace, the temp directories and `HARNESS_SANDBOX_WRITABLE`
(colon-separated). `read-only` allows temp writes only, and
`HARNESS_SANDBOX_NETWORK=deny` also cuts the network. If the platform tool is
missing, commands are refused with a message rather than run unsandboxed.
Approvals still apply on top. The sandbox limits what an approved command can
do, and it is what makes a long `--yolo` run reasonable.

## Agent bus

The model can spawn child agents with the `spawn` tool. Children get base tools
only (read/write/edit/bash, plus the read-only `web_fetch` and `symbol_search`) — one level deep, by construction — and a child that
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

## Editors and browsers

Two more front ends speak to the same daemon, so neither reimplements anything:

- **`harness --acp`** — Agent Client Protocol on stdio, for editors. `session/new`,
  `session/load` (replays the journal), `session/prompt` (streaming
  `agent_message_chunk` / `agent_thought_chunk` / `tool_call`), `session/cancel`,
  and gate questions arrive as `session/request_permission` — an editor's refusal
  is what the tool sees. Client-provided `fs/*` and `terminal/*` capabilities are
  unused; the harness's own confined tools do that work.
- **`harness web [port]`** — a localhost HTTP server serving one self-contained
  page (no build step, no dependencies) that streams `session/update` over SSE and
  sends commands to `/rpc`. One session per server, bound to the directory it was
  started in, token-guarded so another tab cannot drive it. Approvals have nobody
  to ask, so they are denied exactly as a detached daemon denies them.

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

Sessions are portable by construction: `harness export [id] > file.jsonl` writes
the journal as-is and `harness import file.jsonl` validates it and files it under
the workspace its header names, so history resumes wherever it lands.

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

`HARNESS_EFFORT` (`low`…`max`; Claude models default to `xhigh` where
supported), `HARNESS_THINKING=off`, `HARNESS_SANDBOX*` (above),
`ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL`, `ZAI_API_KEY`, `DEEPSEEK_API_KEY`,
`HARNESS_PROVIDER` (`anthropic` | `zai` | `deepseek` | `ollama` | `compat` |
`mock`), `HARNESS_SOCKET`, `HARNESS_MOCK_DELAY_MS`, `HARNESS_THEME`
(`dark` | `light`).

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
apply. Requests carry four cache breakpoints (tools, system, and two on the
conversation tail), so a long agent session re-reads its history from cache
instead of paying full price for it every sample. Signed thinking is sent back
unchanged to the model that produced it, which models that think across tool
calls require. On first-party Claude models, adaptive thinking and an effort
level are set per request, so `/model` can switch between model generations
mid-session. The output ceiling follows the model (up to 64k). DeepSeek defaults to `deepseek-v4-pro`; `--model deepseek-flash` is ~4×
cheaper.

## Tests and CI

`cargo test --workspace` (167 tests) plus `cargo fmt --all --check` and
`cargo clippy --workspace --all-targets -- -D warnings`, which
`.github/workflows/ci.yml` runs on macOS and Linux. Tagging `vX.Y.Z` builds
binaries for both macOS architectures and Linux via `release.yml`, and
`install.sh` installs a published release (`HARNESS_REPO=owner/name sh
install.sh`) — both are inert until the repo has a remote, which is why the
installer says so instead of failing obscurely.

## Tests

```sh
cargo test --workspace     # unit, e2e over a real socket, PTY-driven TUI tests
cargo fmt --all --check
```
