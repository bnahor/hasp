# P3 Plan — The Agent Bus (spawn, notices, message+trigger)

> **Follow-up (2026-09-20): notice delivery inside a turn.** The claim below that
> notices deliver "at turn start and after each tool batch" was only half true —
> the engine delivered at turn start and in `post_turn_check`, never inside the
> sample loop. A parent therefore could not see a child's report for the rest of
> a long turn. Testing the bus against a real model made it obvious: asked to
> have a child count lines, the parent polled `agents` (which showed `completed`
> but no result), then spent **24 bash calls** grepping traces and even messaged
> the child to "please restate your final report", burning 32 samples, ~3 minutes
> and $0.08 to answer "5". Two fixes: notices are now delivered after every tool
> batch, and `ChildInfo.report` carries the child's latest final text so polling
> the roster actually answers "what did it say?". Same prompt after the fix:
> **4 samples, 3 tool calls, 8.5s, $0.005**. Regression tests:
> `a_notice_arriving_mid_turn_reaches_the_very_next_sample` (ordering proves
> mid-turn delivery) and report assertions in the bus and daemon e2e suites.
>
> **Status (2026-09-19): COMPLETE.** All 17 suites green (39 tests), fmt clean. Verified end-to-end: the LocalBus integration test (spawn → completed status + finished notice carrying the child's final text → `message(trigger)` → replied notice → mutating child in a managed worktree `sa-002-*`), the core test (notices deliver at turn boundaries; the `spawn` tool reaches the bus with task+mutation intact), and the daemon e2e (`agent/spawn` → child-exit notice on the wire → `agent/list` → `agent/message` trigger → receipt + replied note). 8/8 stable after two fixes below.
>
> Flakes found and fixed by the loop, worth remembering:
> - Registry status stamps *before* the notice lands — tests must poll the notice, not the status.
> - `HashMap.values()` iteration order is arbitrary — roster assertions look up by id, never by index.
>
> Architecture note: `LocalBus` lives in its own `harness-bus` crate (the binary crate can't be imported by integration tests; the crate map in `03-architecture.md` called for a `bus` crate anyway). One implementation serves embedded and daemon: children are fresh engines in fresh sessions, `builtins_base()` only (one level deep by construction), child approvals route to the parent's channel (TuiGate/WireGate) or run yolo in non-interactive modes (structural confinement still applies).

Implements `research/03-architecture.md` §8.2. 2026-09-19.

## Scope

**In:**
- **Core `Bus` trait + types**: `spawn_child(SpawnRequest{task, name, mutation}) → SpawnHandle{child_id, session_id}` (admission handle, **never the answer** — codegraff/Prime semantics); `message_child(child, text, trigger) → receipt Queued|Delivered` implementing the steering table (running → queued at its next tool boundary; idle + `trigger` → rehydrated new turn; idle + no trigger → queued, waits); `list_children()`. Harness-assigned stable ids (`sa-<n>-<hex>`), family-scoped.
- **Notice inbox** (`Arc<Mutex<Vec<String>>>` shared past the engine lock — a watcher never blocks on a running turn): child-exit notices push the child's final text into the parent's inbox; `run_turn` drains (cap 8 per drain) **at turn start, after every tool batch, and in `post_turn_check`**, appending `[child …]` user messages (volatile-state-rides-the-tail). If the parent went idle with no goal active, notices surface on the next turn — idle-wake is explicitly P4 (goals/schedules).
- **Model-facing tools** (one level deep — children get base tools only): `spawn`, `message`, `agents`.
- **`LocalBus`** (one impl serving both embedded and daemon): children are fresh `Engine`s in fresh sessions; **mutating children get managed worktrees** (`sa-<id>` branches, kept after completion — cleanup sweeps are P4); child gates route to the same approval channel as the parent; `on_notice` callback fans out to the TUI / daemon `session/update`.
- **Wire methods**: `agent/spawn`, `agent/message`, `agent/list`; child exits arrive as `session/update` notes on the owning connection.
- Provider/model/workspace captured at bus construction via a **factory** (`Fn() -> Box<dyn Provider>`), never by locking the parent.

**Out (tracked):** sync `subagent` tool (fire-and-forget is the researched design; a sync wrapper is a trivial later addition), idle parent wake on child exit (P4), opt-in `retained` + tombstones (P4: children live for the connection lifetime), message size/rate limits, cross-session addressing.

## Verification

- Core: inbox drain ordering/cap; spawn/message/agents tools against a scripted mock bus.
- Integration (`cli/tests/bus.rs`): real LocalBus + EchoProvider — spawn → poll to completed → parent inbox contains `echo:`-shaped final text; `message(trigger)` runs a second child turn; mutation spawn lands in a managed worktree.
- Daemon e2e: `agent/spawn` → `agent/list` poll → `agent/message` trigger → receipt; a `session/update` note with `[child` observed on the raw stream.

## Notes

- Provider factory threaded through `embedded_backend`/daemon open (replaces single-instance provider construction — children each need one).
- `builtins()` splits: `builtins_base()` (read/write/edit/bash) for children, `builtins()` = base + agent tools for roots.
