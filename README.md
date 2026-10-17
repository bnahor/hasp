# hasp

A personal coding agent: a TUI attached to a daemon that owns one workspace at a
time, sessions as append-only JSONL, prompt-cache-disciplined requests, and a
lightweight agent bus for spawning child agents.

```sh
cargo build --release          # binary at target/release/hasp
```

## Renamed from `harness`

This project was called `harness` until October 2026. On first run `hasp`
copies its own files from `~/.harness` (sessions, credentials, provider,
agent, MCP and hook config, skills, commands) into `~/.hasp` and leaves the
originals in place — `~/.harness` is shared with other tools, so it is never
moved. Projects keep working: `.harness/` worktrees, reviews, skills and
config are still read (new state goes to `.hasp/`), `harness/<name>`
worktree branches are still found, `HARNESS_*` environment variables still
work (`HASP_*` wins when both are set), and `HARNESS.md` is still read.
Rebuild the container guest agent once with `hasp env build-guest`.

## Install

```sh
# macOS / Linux — the `hasp` CLI and the `hasp-desktop` native app
curl -fsSL https://raw.githubusercontent.com/bnahor/hasp/main/install.sh | sh

brew install bnahor/hasp/hasp       # Homebrew (macOS, Linux)
npm install -g hasp-cli             # npm (fetches the same release binaries)
```

Prebuilt for macOS (Apple silicon and Intel) and Linux x86_64; every `v*`
tag publishes them on the [releases page](https://github.com/bnahor/hasp/releases/latest)
with SHA-256 checksums. On Linux the native app needs the usual GPU and
windowing libraries (fontconfig, Wayland/X11, Vulkan); `hasp` needs nothing.

From source (Rust stable):

```sh
git clone https://github.com/bnahor/hasp && cd hasp
cargo install --locked --path crates/hasp-cli       # → ~/.cargo/bin/hasp
cargo install --locked --path crates/hasp-desktop   # → ~/.cargo/bin/hasp-desktop
```

## Quickstart

```sh
hasp --version                         # → hasp 0.1.0
hasp login                  # Anthropic key → ~/.hasp/credentials.json (0600)
hasp login deepseek         # …or DeepSeek (Anthropic-compatible endpoint, 1M ctx)
hasp login zai              # …or a z.ai / GLM coding-plan key
hasp doctor                 # what's configured, what's missing

hasp                        # TUI (auto-spawns the daemon and attaches)
hasp --plain                # line REPL, embedded (no daemon)
hasp -p "explain src/lib.rs"  # one-shot, prints the answer
hasp --provider mock        # keyless: echoes prompts, for poking at the UI

hasp app                    # the desktop app, in its own window
hasp web                    # the same app on 127.0.0.1 (prints a token URL)
hasp --acp                  # Agent Client Protocol on stdio — editors (Zed, …) drive it
hasp --mcp-serve            # serve the same tools over MCP on stdio
```

The install command is the whole story: the root of this repo is a virtual
workspace, so point `cargo install` at `crates/hasp-cli` (not `.`). After
that, `hasp update` rebuilds and reinstalls from the checkout (remembered
in `~/.hasp/checkout`, so it works from the installed binary too),
`hasp update --models` refreshes the model catalog, and `hasp doctor`
always shows the running binary's path and whether it is the one on `PATH`.

Inside the TUI:

| command | what it does |
| --- | --- |
| `/goal <text> [--budget N\|--unlimited]` | persistent objective; the hasp keeps prompting itself until the model calls `goal_complete`. Defaults to a 2M-token budget |
| `/goal status\|pause\|resume\|clear` | as you'd expect |
| `/autonomous on [--max-turns N] [--max-tokens N] [--gate "cmd"]` | keep working until the gate command passes or a limit trips |
| `/heartbeat every 10m <text>` | recurring nudge; `in 30m <text>` one-shot; `cron 0 9 * * 1-5 <text>` wall-clock, local time; `list`, `pause`, `resume`, `clear` |
| `/compact`, `/branch [n]`, `/fork`, `/resume [n]` | context and session control |
| `/worktree [name]`, `/worktree-exit [rm]` | isolate the session in a git worktree |
| `/model [id]`, `/stats`, `/name <text>`, `/new`, `/clear`, `/exit` | the rest |
| `/theme light\|dark` | color palette (`HASP_THEME` sets the default); dark is the historical look |

Any `/name` that is not built in runs a prompt template (below); they show in
`/help` and tab-completion.

Keys: `enter` send · `ctrl+j` newline · `esc` cancel a turn · `↑`/`↓` history ·
`pgup`/`pgdn` scroll · `ctrl+c` exit.

## Tools

`read`, `write`, `edit`, `bash`, `job`, `grep`, `glob`, `todo`, `web_fetch`,
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
- **Background jobs**: `bash` with `background: true` starts a dev server,
  watcher or long build and returns at once. It goes through the same gate,
  rules and sandbox as any command. `job` lists jobs, pages each job's *new*
  output with its exit status, and kills a job's whole process group. Jobs
  belong to the session: at most 8 run at once, and any still running are
  stopped when the session ends.

## Images

Drag a screenshot into the terminal (the pasted path is picked up, escaped
spaces and quotes included) or mention one as `@path/to/shot.png`. The image
is attached to the prompt and the path in the text becomes `[image 1: …]`.
`read` shows PNG/JPEG/GIF/WebP files to the model as images. MCP tools that
return images pass them through, `--mcp-serve` returns them, and ACP editors
can send image blocks. Images over 3.75 MB are downscaled with `sips` (macOS)
or ImageMagick. Models that cannot see images (DeepSeek, text-only GLM,
`HASP_COMPAT_VISION=off`) get a placeholder instead of an error, so a
session with an image in it keeps working after a model switch.

## Commands, skills and hooks

Plain files, laid out so an existing Claude Code collection works unchanged.
Project beats home, `.hasp` beats `.claude`:

- **Prompt templates**: `.hasp/commands/review.md` (or `.claude/commands/…`,
  `~/.hasp/commands/…`, `~/.claude/commands/…`) runs as `/review <args>`.
  `$ARGUMENTS` and `$1`…`$9` are substituted; `description:` front matter
  shows in completion; `git/pr.md` becomes `/git:pr`. They work in every front
  end (TUI, REPL, `-p`, ACP, web) because the engine expands them.
- **Skills**: `.hasp/skills/<name>/SKILL.md` (or `.claude/skills/…`, and
  the home-directory twins) with `name:` and `description:` front matter. Only
  names and descriptions are in context. The model loads a skill's body, and
  any supporting file in its directory, through the `skill` tool when a task
  calls for it.
- **Hooks**: `~/.hasp/hooks.json`, and `.hasp/hooks.json` once you run
  `hasp trust` in the project (a cloned repo's hooks never run on their
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

`hasp doctor` lists what it found.

## Environments

Run an agent's tools in their own container, with limits you declare:

```toml
# .hasp/environments/py312.toml   (or ~/.hasp/environments/)
image   = "python:3.12-slim"
cpus    = 2
memory  = "4G"
pids    = 512
network = "none"                 # or "full" (default)
runtime = "runsc"                # optional: gVisor; or a Kata microVM runtime
setup   = ["pip install -e ."]   # once, as root, before the agent starts
[env]
PYTHONDONTWRITEBYTECODE = "1"
```

```sh
hasp env build-guest          # once: the static Linux guest agent (≈30 s, via Docker)
hasp env check py312          # start it, print the limits the kernel enforces, stop it
hasp --env py312              # this session's tools run inside it (also HASP_ENV)
hasp env list | prune         # definitions · remove containers whose session died
```

`runtime` picks the container runtime: the default (`runc`, a shared
kernel), `runsc` (gVisor: about +12% on a whole agent session and 19 MB per
sandbox, measured in `docs/runtime-benchmark.md`), or a Kata runtime (a
microVM per container, which needs bare-metal Linux to be practical).

`read`, `write`, `edit`, `bash`, `job`, `grep`, `glob` and `symbol_search`
run **inside** the environment. The session journal, the model connection,
approvals and the agent bus stay on the host. The workspace is mounted at its own path,
so edits are visible on the host immediately, and a git worktree works inside
too.

Child agents can each get one. `spawn` takes `environment: "py312"`, and a
child otherwise inherits its parent's environment, so a session running in an
environment can't spawn its way out of it. A mutation child gets its own
container with its own worktree mounted.

How it works: the guest agent is this hasp's own static build, run inside
as `hasp --mcp-serve`. The host's tools become proxies with the same
names and schemas, so the model sees no difference. A proxy asks the host's
approval gate before forwarding a call; the guest then enforces the usual
structural rules. Calls run concurrently, and esc cancels a command inside the
environment too. Backends sit behind one trait
(`crates/hasp-env/src/backend.rs`); Docker is the first, and microVMs
(Firecracker, Apple Virtualization) are designed to be the next.

## Bring your own plan

Two ways to run on a plan you already pay for.

**Coding plans and other endpoints, on the hasp's own engine.** Most
coding plans ship an Anthropic-compatible endpoint; the hasp speaks that
wire, so a plan is just a key:

```sh
hasp providers                  # built-ins, which have keys, default models
hasp login kimi                 # stored in ~/.hasp/credentials.json (0600)
hasp --provider kimi            # or KIMI_API_KEY in the environment
```

Built in: `zai` (GLM coding plan), `kimi` (Kimi Code), `minimax` /
`minimax-cn`, `qwen` (Alibaba Model Studio coding plan — Qwen, GLM, Kimi,
MiniMax), `moonshot`, `deepseek`. Anything else Anthropic-compatible (a
company gateway, OpenRouter, a new plan) goes in `~/.hasp/providers.json`:

```json
{ "providers": { "gateway": { "base_url": "https://llm.example.com/anthropic",
  "key_env": "GATEWAY_KEY", "model": "glm-5.1", "auth": "bearer" } } }
```

**Subscriptions, through their own CLI.** A Claude Pro/Max or ChatGPT plan
can only be used by its vendor's own client, so the hasp drives that
client — the one you already installed and logged into — and puts its own
layer on top: the TUI, approvals in the prompt line, `/review` and
`hasp review`, worktrees.

```sh
hasp agents                         # which agents are installed
hasp --agent claude                 # Claude Code, on your Claude plan
hasp --agent codex --model gpt-5.5  # Codex, on your ChatGPT plan
hasp --agent opencode -p "fix the failing test"
```

| agent | protocol | plan |
|---|---|---|
| `claude` | `claude -p` stream-json; permissions asked over the same pipe | Claude Pro / Max |
| `codex` | `codex app-server` (JSON-RPC) | ChatGPT Plus / Pro |
| `gemini`, `opencode` | ACP (Agent Client Protocol) | Google account; OpenCode's providers |

Any other ACP agent is one entry in `~/.hasp/agents.json`
(`{"agents": {"name": {"command": "…", "args": ["…"]}}}`). Each agent's
session continues across runs in the same workspace (`--new` starts fresh);
`--yolo` lifts its approval prompts, and its own sandbox still applies. The
agent runs its own tools, so the hasp's tool-level guarantees (path
confinement, the bash rule) are the agent's to keep in this mode; reviews
still see everything it changed.

## Review

Review what an agent did the way you review a colleague's change — in your
editor, over rounds — without leaving the terminal.

```sh
hasp review new "add retries"    # snapshot the workspace, open the diff in $EDITOR
hasp review send                 # hand your comments to the agent (continues its session)
hasp review edit                 # next round: only what changed since your last look
hasp review approve --land       # commit exactly the snapshot you approved
hasp review                      # list: needs review · changes requested · approved · landed
```

In the TUI, `/review` does the same for the session's workspace or worktree;
if you request changes, your comments land in the prompt and enter sends them.

The document is a diff with line numbers. Comment on any line by adding a
line below it that starts with `>> `; earlier comments show up as
`<< #n open` next to the code they were about — they follow that code as the
agent edits the file — and changing `open` to `resolved` closes them. Set
`verdict: approve | changes` at the top.

- **Only new work each round.** Every look commits the target with a private
  index (your staging area and branches never move); the next look diffs from
  the snapshot you last reviewed.
- **Only reviewed work lands.** `land` refuses if the target changed after the
  approval; a workspace lands as a commit of the approved tree, a child
  worktree (`--worktree NAME`, or `/review` inside it) as a merge of the
  approved snapshot, including work the agent never committed.
- **State** is an append-only log per review in `.hasp/reviews/`;
  snapshots are kept alive by refs under `refs/hasp/reviews/`.

### Diffs

Diffs everywhere (the review document, `write`/`edit` tool results) come from
`hasp-diff`: interned lines, histogram diff for readable hunks, and git's
indent heuristic. Histogram's cost grows with file length × number of separate
changes, so it runs only within a budget (~50 ms of work); past it the diff is
Myers, which stays near-linear. A 200,000-line file with 20,000 scattered
edits diffs in milliseconds instead of four seconds; nothing is ever
"too large to diff". Profile with
`cargo run --release -p hasp-diff --example profile`.

## Sandbox

`HASP_SANDBOX=workspace-write` runs every `bash` command under the OS
sandbox (`sandbox-exec` on macOS, `bwrap` on Linux), so it can write only to
the workspace, the temp directories and `HASP_SANDBOX_WRITABLE`
(colon-separated). `read-only` allows temp writes only, and
`HASP_SANDBOX_NETWORK=deny` also cuts the network. Under `read-only` the
workspace itself stays read-only even when it lives inside a temp directory.
If the platform tool is missing, commands are refused with a message rather
than run unsandboxed.
Approvals still apply on top. The sandbox limits what an approved command can
do, and it is what makes a long `--yolo` run reasonable.

Without a prompt, the gate runs only commands it can prove read-only: `ls`,
`cat`, `head`, `tail`, `wc`, `pwd`, `diff`, `which`, `grep`, `rg` (not `--pre`)
and reading `git` subcommands (no `-c`, `--output`, `--ext-diff`). Interpreters
and build tools (`python3`, `node`, `make`, `cargo`, `pytest`) always ask, or
are allowed by name in `~/.hasp/policy.json`:
`{"bash": {"allow": ["cargo test", "make check"]}}` (word prefixes; the
metacharacter and path rules and the sandbox still apply).

## Agent bus

The model can spawn child agents with the `spawn` tool. Children get base tools
only (read/write/edit/bash, plus the read-only `web_fetch` and `symbol_search`) — one level deep, by construction — and a child that
declares `mutation: true` runs in its own git worktree (`hasp/<name>`). A child
without `mutation` shares your checkout read-only: no `write`/`edit`/`job`, and
`bash` limited to provably read-only commands under its own gate (it never asks).
Children never block the parent: exits arrive as notices between tool calls, and
`message` supports follow-ups with `delivered`/`queued` receipts.

A child that declared `mutation: true` leaves its worktree and branch behind so
you can inspect the work. To clean up:

```sh
hasp worktrees                      # name, branch, and whether it is merged
hasp worktrees prune <name>         # refuses unmerged work; --force overrides
```

The branch always survives a prune, so a forced cleanup is still recoverable with
`git branch -D hasp/<name>` once you are sure.

## Editors and browsers

Two more front ends speak to the same daemon, so neither reimplements anything:

- **`hasp --acp`** — Agent Client Protocol on stdio, for editors. `session/new`,
  `session/load` (replays the journal), `session/prompt` (streaming
  `agent_message_chunk` / `agent_thought_chunk` / `tool_call`), `session/cancel`,
  and gate questions arrive as `session/request_permission` — an editor's refusal
  is what the tool sees. Client-provided `fs/*` and `terminal/*` capabilities are
  unused; the hasp's own confined tools do that work.
- **`hasp app`** / **`hasp web [port]`** — the desktop app. `app` opens it
  in its own window (a Chromium-family browser's app mode, else the default
  browser); `web` serves it and prints the URL.
  - **Sidebar:** every project and session on the machine, newest first, with a
    live dot that pulses while a turn runs; search; new session (⌘N); open any
    folder as a project.
  - **Conversation:** streamed markdown with code blocks, folded reasoning, tool
    cards with coloured diffs, a live chip while a tool runs, turn stats, and
    approvals as cards (allow once / always / deny) instead of silent denials.
  - **Composer:** Enter to send, Esc to stop, image paste and drop, a model
    picker, and the context meter in the header.
  - **Changes pane (⌘J):** the working tree against HEAD through the review
    engine's diff; click a line number to comment, and the comments go to the
    agent with your next message.
  - Light and dark themes; no build step or dependencies (one embedded page).
    One daemon connection per open project, token-guarded so another tab cannot
    drive a session. Projects under the system temp directory are left out of
    the sidebar unless opened directly.

## MCP servers

Add tools from any MCP server (stdio). Project file wins over user file:

```json
// .hasp/mcp.json  (or ~/.hasp/mcp.json)
{"servers": {"git": {"command": "uvx", "args": ["mcp-server-git"],
                    "env": {"GIT_DIR": "."}}}}
```

Remote tools appear to the model as `mcp__<server>__<tool>` with their schemas
unchanged, so the tool list stays cache-stable. A server that fails to start is
reported and skipped — it never blocks startup.

The hasp can also *be* a server, so other agents can use its tools:

```sh
hasp --mcp-serve     # MCP over stdio, tools: read, write, edit, bash
```

That serves the same tool layer, so workspace confinement (relative paths only,
no `..`, symlink checks) and the bash metacharacter rule still apply. Because
there is no human attached to approve anything, writes and non-allowlisted
commands are refused unless you set `HASP_MCP_YOLO=1`. Point a client at it
with `{"servers": {"hasp": {"command": "…/hasp", "args": ["--mcp-serve"],
"cwd": "/path/to/project"}}}`.

## Where state lives

- `~/.hasp/sessions/<workspace-key>/*.jsonl` — append-only session trees
  (messages, compactions, goals, heartbeats, model changes), written, never
  mutated. `record` entries are the canonical state; restarting resumes the tail.
- `~/.hasp/credentials.json` — API keys, mode 0600.
- `~/.hasp/residents.json` — sessions the daemon restores at start
  (`hasp resident enable|disable|list`).
- `~/.hasp/recall/index.db` — full-text index over the journals; node-local,
  disposable (`hasp recall rebuild`).
- `~/.hasp/memory/memory.jsonl`, `~/.hasp/workflows/` — curated memory and
  workflow drafts (`hasp memory …`, `hasp workflow …`). See docs/edge-agent.md.
- `<project>/.hasp/artifacts/<run>/` — truncated tool output that the model can
  page back in with `read`.
- `<project>/.hasp/traces/*.jsonl` — per-turn API and tool timings.

Sessions are portable by construction: `hasp export [id] > file.jsonl` writes
the journal as-is and `hasp import file.jsonl` validates it and files it under
the workspace its header names, so history resumes wherever it lands.

## Daemon

The TUI attaches to a daemon by default, and sessions are **daemon-resident**: a
client is a view, not the owner. Closing the TUI detaches it; the session keeps
running, so goals, heartbeats and any in-flight turn continue. Output written
while nobody is attached is dropped rather than lost — the journal is canonical,
so the next client to attach sees all of it. Starting the TUI in that workspace
rejoins the running session, and a second client joins the same one rather than
being refused. `session/new` is refused while a live session holds the
workspace (use `hasp shutdown` to end it), because quietly evicting a session
that may be mid-goal would be worse. Approval questions with no client attached
are denied immediately instead of waiting for a client that is gone.

A turn holds the engine only for moments (building a request, recording a
sample, recording tool results). The model stream, tools, hooks, child waits
and the compaction summary run without it, so `/stats`, `/model`, `/goal` and
renames answer while a turn is working. Turns run one at a time. Requests that
would swap the session under a running turn (`/branch <id>`, `/worktree`,
`/worktree-exit`, and `/new` and `/resume` when embedded) are refused with a
message rather than queued. Cancel first.

`--no-daemon` runs everything embedded, `--serve` runs the daemon in the
foreground. Transport is newline-delimited JSON-RPC 2.0 over `$HASP_SOCKET`
or `~/.hasp/daemon.sock`.

## Environment

`HASP_EFFORT` (`low`…`max`; Claude models default to `xhigh` where
supported), `HASP_THINKING=off`, `HASP_SANDBOX*` (above),
`ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL`, `ZAI_API_KEY`, `DEEPSEEK_API_KEY`,
`HASP_PROVIDER` (`anthropic` | `zai` | `deepseek` | `ollama` | `compat` |
`mock`), `HASP_SOCKET`, `HASP_MOCK_DELAY_MS`, `HASP_THEME`
(`dark` | `light`).

**Any other model or vendor, with no code change**: `--provider compat` speaks
the OpenAI wire format to whatever `HASP_COMPAT_URL` points at (plus optional
`HASP_COMPAT_KEY`) — Ollama and vLLM/LM Studio locally, OpenRouter/Groq and
most new vendors remotely. `--provider ollama` defaults to
`http://127.0.0.1:11434/v1` (`HASP_OLLAMA_URL` overrides; windows via
`HASP_OLLAMA_WINDOW`/`HASP_COMPAT_WINDOW`). Model lists are discovered
from the endpoint — `hasp models ollama` shows what `ollama pull` gave you —
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

`cargo test --workspace` (195 tests) plus `cargo fmt --all --check` and
`cargo clippy --workspace --all-targets -- -D warnings`, which
`.github/workflows/ci.yml` runs on macOS and Linux. Tagging `vX.Y.Z` builds
binaries for both macOS architectures and Linux via `release.yml`, and
`install.sh` installs a published release (`HASP_REPO=owner/name sh
install.sh`) — both are inert until the repo has a remote, which is why the
installer says so instead of failing obscurely.

## Tests

```sh
cargo test --workspace     # unit, e2e over a real socket, PTY-driven TUI tests
cargo fmt --all --check
```
