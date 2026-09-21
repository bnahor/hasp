# P2.5 Plan — TUI over the daemon (default-on completed)

> **Status (2026-09-19): COMPLETE.** All 14 suites green (36 tests), fmt clean. The headline verification is `tui_auto_spawns_daemon_and_attaches` (PTY-level, real binary, 3/3 stable): the TUI spawns the daemon, attaches over the wire (`▪ session … via daemon`), exits on `/exit`, and the daemon **survives the TUI and still answers** `initialize` on the socket afterward.
>
> Bugs found and fixed by that test, worth remembering:
> - The auto-spawned daemon didn't receive the TUI's provider/model selection — `--serve` never consumed them and `build_provider` only reads env. Flags are now forwarded as env pairs on the spawn command (`HARNESS_PROVIDER`/`ANTHROPIC_MODEL`).
> - `embedded_backend` took the engine's canceller with `blocking_lock()` inside the async runtime → panic. The canceller is now taken under `.lock().await` in the (async) constructor path.
> - The daemon inherited the TUI's process group and died with the pty (SIGHUP). `ensure_daemon` now spawns it with `process_group(0)` — its own session, which is the correct production behavior for a daemon.
>
> Also new: **Esc cancels** the running turn (both backends).

Implements the deferred half of P2: the interactive TUI runs as a wire client of the daemon, and `harness` auto-spawns the daemon when absent. 2026-09-19.

## Scope

**In:**
- **New wire methods** (mechanical daemon handlers over existing engine ops): `model/set`, `session/listings`, `session/branch` (list | move), `session/fork`, `session/compact`, `session/worktree` (new), `session/worktree-exit` (back, optional remove). `session/stats` gains `model` + `window`.
- **Backend abstraction in the TUI**: `trait Backend { submit_prompt, submit_compact, cancel, call(method, params) -> Value }`.
  - `EngineBackend` — current embedded path (`--no-daemon`), implements `call` by mapping method strings to the same JSON shapes the daemon returns (single source of truth for the contract, and the reference implementation).
  - `WireBackend` — a connection **actor** owning the socket: multiplexes requests/responses by id, converts `session/update` notifications into UI events, routes `permission/request` to the existing prompt UI, reports turn/compact completions as `UiEvent::Done`.
  - One code path in `command()`: both backends speak identical JSON.
- **Auto-spawn**: `harness` (interactive, no `--plain`/`--no-daemon`) connects or spawns `--serve` detached (stale-socket cleanup, ≤5s wait), then `initialize` + `session/continue {cwd}` (or `--new`). Failure → embedded fallback with a note. This completes "daemon default-on": turns survive TUI exit, one process owns the workspace.
- **Esc cancels** the running turn (both backends) — closes the P2 cancellation-UI gap.
- Worktree bookkeeping: `session/worktree` swaps the engine's workspace+session *and* the connection's busy-map key (old released, new claimed); exit reverses (busy old-root → error, session kept).

**Out:** multi-client fan-out to >1 subscriber, detach/reattach *mid-turn* UI (the turn survives; reattach resumes at next prompt), strict-ACP framing, remote (TCP) transport.

## Verification

- Daemon e2e extended: compact (echo summary → est shrink, entries grow), listings, fork (new id + parent path), branch list + move, model/set validation, worktree new/exit round-trip in a git repo.
- Existing suites stay green (PTY e2e covers the embedded path; plain REPL unchanged).
- Manual: default `harness` (mock provider) boots daemon automatically; kill TUI mid-`HARNESS_MOCK_DELAY_MS` turn; `harness -p "…" --daemon` sees the session continue; `harness shutdown`.
