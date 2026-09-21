# P4 Plan — Long-Running Surfaces (goals, autonomous mode, heartbeats)

Implements `research/03-architecture.md` §8.3 (Prime-derived semantics). 2026-09-19.

> **Status: COMPLETE.** 44 tests green (17 suites), fmt clean, zero warnings.
>
> Shipped: journaled `goal` / `autonomous` / `heartbeat` entries in the session tree (latest-wins folds on `SessionManager`); `Engine::post_turn_check()` as the one continuation primitive all three callers loop (daemon `run_turn_loop`, `EngineBackend::submit_prompt`, plain REPL, `-p`); the `goal_complete` meta-tool intercepted by the engine before dispatch; the autonomous gate runner (`bash -lc`, 60s, process-group kill on timeout, 2 KB output tail); the daemon heartbeat scheduler with claim-before-deliver; `goal/set|status`, `autonomous/set`, `heartbeat/set|list` over the wire and in the TUI/plain slash surface.
>
> Verified: engine tests for the goal loop (`goal_complete` ends it), budget auto-pause, and gate fail→continue / pass→stop; daemon e2e for goal state over the wire plus a heartbeat firing its own turn with nobody prompting; plain-REPL demo of the loop + budget pause through the real binary.
>
> Bugs found and fixed along the way: `creds::path()` returns the credentials *file* while `load_at` takes its directory, so composing them double-joined the name and file-stored API keys were never read (doctor said MISSING for a key that existed); a heartbeat that arrives during a busy turn now coalesces to at most one pending steer instead of accumulating a tick backlog; test helpers must accumulate streamed deltas (the mock emits token-sized chunks, so no single frame contains a multi-word needle).
>
> **Honest scope calls (differs from the ambition below):**
> - Heartbeats are **per-connection**: they fire while a client is attached, and are journaled + re-armed on reattach. A fully detached daemon does not run them (the daemon owns a session only while a client holds it) — daemon-resident sessions are a P5-scale change, not a P4 one.
> - One-shots (`in <dur>`) fire once and are removed from the runtime map; they do not survive a daemon restart (recurring heartbeats do, via the journal).
> - Busy engines take the steering path for both `steer` and `follow_up` modes; `follow_up` is preserved in the journal and the prompt is labelled a scheduled task, but the "run after the current turn" queue distinction is deferred.

## Scope

**In:**
- **Persistent goals** (engine-level, embedded + daemon): journaled `goal` entries in the session tree (state `set|paused|completed|cleared`, text, optional token budget — latest entry wins, states survive restarts). `/goal <text> [--budget N]`, `/goal status|pause|resume|clear`. After each turn, if a goal is active: budget check (session totals vs budget → auto-pause + note), then inject a continuation prompt ("Continue working toward your goal… call goal_complete when done"). **`goal_complete` meta-tool** (intercepted by the engine, like codegraff's meta tools) marks completion — only the model completes a goal; continuation count hard-capped (100/session) even without a budget.
- **Autonomous mode** (engine-level): `/autonomous on [--max-turns N] [--max-tokens N] [--gate "cmd"]`. After each turn, while limits unhit: run the gate (bash, workspace root, 60s timeout, tail-capped output) — pass ⇒ stop; fail ⇒ continuation prompt carrying the bounded gate output. Journaled `autonomous` entries. Goal and autonomous compose (goal wins when both would continue).
- **Held continuations while children run**: `post_turn_check` waits for bus children to settle before deciding (the P3 deferred wake-on-child-exit — the child-exit notices are already in the inbox by then).
- **Heartbeats + one-shots** (daemon-level): journaled `heartbeat` entries (`add|pause|resume|clear`, `every <dur>` recurring or `in <dur>` one-shot, instruction); a per-connection scheduler ticks every 250 ms, **claims before delivery** (next_due set before firing; missed ticks coalesce into the next due), re-arms from session entries on `session/continue`. Delivery: engine idle (`try_lock`) ⇒ real turn through the full prompt path (incl. post-check loop); busy ⇒ steering via the notice inbox at the next boundary. User-facing: `/heartbeat every 10m text`, `/heartbeat in 30m text`, `/heartbeat list|pause|resume|clear`. **Embedded TUI: heartbeats return a clear "daemon-mode" error** (documented limitation).
- **Wire methods**: `goal/set`, `goal/status`, `autonomous/set`, `heartbeat/set`, `heartbeat/list` — same JSON shapes served by the engine backend (reference impl) and daemon.
- `post_turn_check()` is one shared engine method; all three callers loop it (daemon prompt task, TUI backend, plain/-p path).

**Out (tracked):** cron expressions (interval + one-shot delay only), agent-created heartbeats, worktree cleanup sweeps, gate-rerun dedup on unchanged workspace, goals/heartbeats surfaces in `harness -p`.

## Verification

- Core: goal lifecycle (continuation fires, `goal_complete` stops it, budget auto-pause, cap); autonomous gate fail→continue/pass→stop, limits; children-running hold (delayed echo child).
- Daemon e2e: `goal/set` → prompt → automatic continuation turn observed → script completes goal → status `completed`; `heartbeat/set every 100ms` → tick turns on the wire; one-shot fires once; busy-target steering via inbox.
- Plain pipe demo (real binary): `/goal` … prompt … `/goal status` shows the loop.

## Notes

- Duration parsing: `90s`/`10m`/`1h` (client-side; wire takes ms).
- Internal (heartbeat) prompts have no response id — notes only.
