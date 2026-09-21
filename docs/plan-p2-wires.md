# P2 Plan — The Wires (JSON-RPC, daemon, cancellation, e2e)

> **Status (2026-09-19): COMPLETE.** All 14 test suites green, fmt clean. Verified: wire codec roundtrip; daemon e2e over a real socket (initialize → continue → prompt → stats → shutdown, against the real binary); **cancellation e2e** with a 10s-delayed echo (prompt cancelled mid-flight, `aborted:true`, samples 0); **PTY e2e** (real binary in a real pty, mock provider: prompt → streamed echo → /exit). Manual smoke: `harness --serve` + `harness -p "…" --daemon --provider mock` + `harness shutdown` (socket removed on exit).
>
> Bugs found and fixed by the e2e work, worth remembering:
> - `session/cancel` originally locked the engine to reach the canceller — the running turn held that lock, serializing cancellation behind the turn it was cancelling. Canceller now lives outside the lock in the connection state.
> - Cancel-before-turn-start race: a cancel arriving between request-parse and task-start was erased by the in-turn reset. Replaced the watch channel with `AtomicBool` + `Notify::notify_one` (permit survives no-waiter windows); the daemon resets the canceller *before* spawning the turn task.
> - Shutdown raced the writer: `serve()` exited (removing the socket, ending the process) before the final replies flushed. Connection tasks now drain in a JoinSet (5s cap) before exit.
>
> Deferred to P2.5 as planned: TUI-over-daemon (auto-spawn + attach, full command surface over the wire), strict-ACP framing, multi-client fan-out.

Implements `research/03-architecture.md` §12 **P2**. Scope agreed 2026-09-19.

## Scope

**In:**
- **`harness-wire` crate**: newline-delimited JSON-RPC 2.0 (LSP/MCP-adjacent; ACP-compatible method *shapes*, framing swap to strict ACP is mechanical later). `Wire::{Request,Response,Notification}` + `RpcError` + method constants.
- **Daemon** (`harness --serve`): Unix socket at `$HARNESS_SOCKET` or `~/.harness/daemon.sock`; stale-socket cleanup on bind (connect-probe before remove); per-workspace exclusivity (second attach → `-32001 workspace busy`); engines built per connection; `session/update` notifications fan out to the owning client; **permission round-trip** (`permission/request` → y/a/n response) through a wire gate; `shutdown` method + `harness shutdown` subcommand.
- **Wire methods v1**: `initialize`, `session/continue {cwd}`, `session/new {cwd}`, `session/load {path}`, `session/prompt {prompt}` → `{samples, toolCalls, aborted, usage, text}`, `session/cancel`, `session/name`, `session/stats`, `permission/request`, `shutdown`.
- **Engine cancellation**: watch-channel canceller exposed as `Engine::cancellable()`/`Canceller::cancel()`; resets per turn; aborts the in-flight provider call via `select!` and breaks between samples; `TurnStats.aborted`. (Tools run to completion — bash timeout caps them; noted as v1 semantics.)
- **`EchoProvider`** (`--provider mock`, env `HARNESS_PROVIDER`, delay via `HARNESS_MOCK_DELAY_MS`): keyless runs for tests and demos; enables e2e without credentials.
- **`-p --daemon`**: one-shot prompts route through the daemon (client auto-answers permission prompts with `n`, matching non-interactive semantics). Plain REPL and TUI stay embedded this milestone; **TUI-over-daemon (auto-spawn, detach/reattach) is P2.5** — default-on completes there.
- **Tests**: wire codec roundtrip; daemon e2e (initialize → continue → prompt → result, stats, shutdown) against the real binary over a temp socket; cancellation e2e with a delayed echo; **PTY e2e** (portable-pty, real binary, `--provider mock --plain`: prompt → streamed echo → `/exit`).
- `harness doctor`: daemon status line (socket probe).

**Out (tracked):** TUI-over-daemon + auto-spawn (P2.5), strict-ACP framing/compliance, multi-client event fan-out to >1 subscriber, cancellation of running tools, MCP client surface, self-as-MCP-server.

## Verification

All new tests green; `cargo test --workspace`; fmt clean; manual: `harness --serve & harness -p "hi" --daemon --provider mock; harness shutdown`.

## Notes

- Framing choice recorded: newline-delimited JSON-RPC. ACP (Zed) compat = method renames + header framing later; the shapes are already aligned (`initialize`, `session/prompt`, `session/update`, permission round-trip).
- Daemon engines use the same `GateCore`; the wire gate blocks a runtime worker awaiting the human, same as the TUI gate — acceptable at personal scale, documented in code.
