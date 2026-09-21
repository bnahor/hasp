# P5 Plan — Extension Substrate: MCP client

Implements `research/03-architecture.md` §9 (progressively-loaded context + external
capability surface). 2026-09-19.

> **Status: COMPLETE** (the consuming half of P5). 49 tests green (18 suites), fmt clean,
> zero warnings, release build ok.
>
> Shipped: `harness-mcp` crate — stdio transport (newline-delimited JSON-RPC 2.0),
> `initialize` → `notifications/initialized` → `tools/list` handshake, per-request 30s
> timeout, `kill_on_drop` child, response routing by id with notifications ignored;
> bridged tools named `mcp__<server>__<tool>` with schemas passed through; config merged
> `~/.harness/mcp.json` ← `<root>/.harness/mcp.json` (project wins); wired into embedded
> and daemon engines (root only — children keep `builtins_base()`, so the P3 containment
> rule holds); `harness doctor` now lists configured servers and the tools they resolve to.
>
> Verified: 5 tests — project-overrides-user + malformed-file-ignored (config merge),
> handshake/tools-list/namespace-sort against a real child process, a full engine turn
> whose tool call reaches the transcript with the server's text, `isError` → tool error,
> and an unstartable server being skipped without failing startup. Plus a manual run of
> the real binary (`harness doctor` in a project whose `mcp.json` points at the test
> double) reporting `1 server(s) → 2 tool(s)`.
>
> Bug found while verifying: the footer's context estimate and the compaction threshold
> used two different measurements (messages-only vs. assembled system+tools), so a manual
> `/compact` could report `tokensBefore` smaller than the estimate that triggered it — the
> assertion that caught this is `tokensBefore >= stats.estTokens` in `daemon_e2e`. Now one
> number, owned by the engine and passed into `compaction::compact`.

## Why this slice first

P5 has two halves: expose the harness *as* an MCP server, and *consume* MCP servers.
Consuming is the one that changes what the agent can do on day one — it is the
canonical way to add tools (filesystem, git, issue trackers, browsers) without
touching this repo. Self-as-server can land later on the same transport.

## In

- **New crate `harness-mcp`** — stdio transport, newline-delimited JSON-RPC 2.0
  (same framing the wire crate already uses).
- **Config** `mcp.json`, merged from `~/.harness/mcp.json` then
  `<workspace>/.harness/mcp.json` (project wins on server-name collision):
  ```json
  {"servers": {"name": {"command": "npx", "args": ["-y", "server"], "env": {"K": "V"}}}}
  ```
- **Handshake**: spawn child with piped stdio, `initialize`
  (`protocolVersion: 2024-11-05`, clientInfo `harness`), `notifications/initialized`,
  `tools/list`. Per-request timeout 30s; a failing server is skipped with a warning
  rather than blocking startup. Child is `kill_on_drop`.
- **Bridging**: each remote tool becomes a `harness_core::Tool` named
  `mcp__<server>__<tool>`, with the server's own description and input schema passed
  through unchanged. `tools/call` results are rendered to text; `isError` maps to
  `ToolOutput::is_error`.
- **Wiring**: MCP tools append to `builtins()` for root engines (embedded, daemon, and
  the bus's root); children keep `builtins_base()` so the one-level-deep containment
  rule from P3 still holds. Tool list stays name-sorted — cache contract intact.

## Out (tracked)

- ~~Harness-as-MCP-server~~ **landed** (`harness --mcp-serve`): stdio server over the
  same `Tool` layer, so confinement and the bash rule apply unchanged; the gate is
  non-interactive, so writes/non-allowlisted commands are refused unless
  `HARNESS_MCP_YOLO=1`. Covered by `cli/tests/mcp_server_e2e.rs`, which drives the
  server with the harness's own MCP client.
- MCP resources/prompts/sampling, SSE + HTTP transports,
  per-server enable/disable flags, hot reload, OAuth.
- A fake server script is the test double; no network is required by any test.

## Verification

- Config merge (project overrides user, malformed file ignored without panicking).
- Handshake + `tools/list` against a real child process (python fake server).
- A full engine turn that calls a bridged tool and asserts the result reached the
  transcript, plus an `isError` case.
- Failing server (bad command) is skipped and startup still succeeds.
