# P8 — daemon-resident sessions

Status: COMPLETE (2026-09-20)

Result: 104 tests pass (4 new in `daemon_e2e`), fmt clean, release builds with no
warnings. The four new tests cover exactly what this plan promised: the schedule
keeps firing with no client (`detached_heartbeat_keeps_firing_with_no_client`,
which fails against the old per-connection design), a second client adopts
(`second_client_adopts_the_live_resident`), a detached approval is denied without
wedging the scheduler (`detached_approval_denies_instead_of_hanging`, which also
asserts the denied command did not run), and a turn outlives its client
(`turn_survives_the_client_that_started_it`).

## The problem

A session belongs to a **connection**, not to the daemon: `ConnState` holds the
engine, the agent bus, the heartbeat runtime and the workspace, and
`handle_connection` drops all of it when the client goes away. The daemon-wide
`busy` set exists only to stop a second client from owning the same workspace
("workspace busy"). Consequences:

- Heartbeats, goals and autonomous continuation die with the client that
  created them, so a "detached" daemon runs nothing. This is the honest scope
  call recorded in P4.
- A turn is killed mid-flight when its client dies, even though the journal
  would have let it finish.
- A second client on the same workspace is refused rather than joined.
- Turns run against an out-channel that dies with the client, so output written
  while nobody listens is lost even though the session is durable.

## The change

A session becomes a daemon-level object; a client becomes a *view* of it.

```text
registry: ws_key -> Resident { engine, bus, canceller, heartbeats, workspace
                               stack, stop flag, client: SharedClient }
```

1. **Registry replaces `busy`.** `Opening` and `Ready` slots. A second client on
   a ready slot **attaches** instead of failing; `session/new` on a live
   resident is still refused (with a message saying how to proceed) because
   silently evicting a running session's heartbeats would be worse.
2. **`SharedClient` is the swappable output slot.** It implements `Sink` and
   `TurnSink` and forwards to whichever client is attached, or drops the update
   when nobody is. The journal stays canonical, so nothing is lost — a detached
   turn is fully recoverable with `session/load`. This is also mid-turn
   detach/reattach for free.
3. **Permissions never hang on a ghost client.** `WireGate` asks the shared
   slot; no attached client means an immediate `'n'` (deny), not a blocked
   worker waiting for a client that no longer exists. Structural safety is
   untouched and `--yolo` still short-circuits before the question.
4. **One heartbeat loop per resident**, not per connection: it fires while
   detached, keeps claim-before-deliver and coalescing, and stops on daemon
   shutdown via a stop flag.
5. **Worktree switching moves the resident** to the new key in the registry and
   updates the shared slot's session id; failures restore the old key.

## Real-model check (DeepSeek, 2026-09-20)

Driven over the wire from a raw client, because that is the only surface that can
set a schedule without the TUI: one-shot heartbeat `in 9s`, then the client
disconnected. The daemon logged `client 1 detached from 83b98a89 — session stays
resident`, and the journal shows the heartbeat entry at `…733326` and its turn at
`…742396` — 9,070 ms later, roughly 8 s after the last client was gone — with a
real assistant reply (`thinking` + `text`). A detached daemon ran a scheduled
turn against a live provider.

## Deliberately not in this pass

- **Cron expressions.** Periodic (`everyMs`) and one-shot (`inMs`) scheduling
  already work and are what the roadmap needs; wall-clock `0 9 * * *` parsing is
  a follow-up.
- **A `/sessions` listing of residents.** Reattach needs no new surface:
  `session/continue` now joins the live resident instead of erroring.
- **Fan-out to several clients at once.** The shared slot holds one client, so
  when a second client attaches it *takes over* the update stream: both can
  prompt and both get their own replies, but only the newest one sees the
  streaming `session/update`s. A broadcast is the natural next step, and the
  slot is the seam for it.

## Verification

- `daemon_e2e`: a heartbeat keeps firing after the client disconnects, proven by
  reading the session JSONL with no client attached; a reattaching client sees
  the detached turn in the journal; a second client attaches rather than getting
  `workspace busy`; a detached approval denies instead of hanging.
