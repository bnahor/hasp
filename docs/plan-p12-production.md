# P12 — the five remaining tranches

Status: tranches 1, 3a and 5 COMPLETE (2026-09-20); 2, 3b and 4 remain

Landed and verified:

- **1. Robustness** — retry/backoff with `Retry-After` (before anything streams
  only), stream stall guard, catalog staleness refresh, `harness sessions
  [list|rm|prune]` with the live-resident guard, failing schedules backing off
  (cron keeps its wall clock). 11 new tests, including two fake-endpoint tests
  and one that a stray `content-length` would have made vacuous.
- **3a. Fan-out** — `SharedClient` is a set now; every attached client gets the
  turn stream and a permission question goes to all with first answer winning.
  `both_attached_clients_see_the_turn_stream` proves a passive watcher is no
  longer starved.
- **5. CI** — `ci.yml` (fmt, clippy `-D warnings`, tests on macOS + Linux),
  `release.yml` (tag → three targets + checksums), `install.sh`. clippy is now
  genuinely clean (40 findings fixed, one of which — a no-op `ascii` branch in
  the tool renderer — was silently doing nothing), so CI can enforce it.

## Still open

- **2. Engine lock across provider I/O.** `run_turn_loop` holds the engine lock
  for a whole turn, so `/stats`, `/model` and goal changes queue behind a slow
  model. The fix is a prepare → provider → apply split with `Engine::run_turn`
  kept as a wrapper; it is a real refactor of the turn loop and was not started
  rather than left half-done.
- **3b. Strict ACP framing + TCP transport.** ACP works but is not
  conformance-audited (`jsonrpc` is not validated, no `available_commands_update`,
  client `fs/*` unused); the daemon is unix-socket only, so a TCP listener with a
  required token is still missing.
- **4. Sandbox and extension surface.** `HARNESS_SANDBOX` (sandbox-exec/bwrap
  wrapping for bash) and project prompt templates in `.harness/commands/*.md`
  are both unstarted — the largest remaining piece, and deliberately not
  rushed.


## 1. Robustness (provider + schedules + sessions)

- **Retry/backoff** on 429/5xx (and Anthropic 529) in both provider clients,
  honoring `Retry-After`, capped attempts. Retried only while *nothing has been
  streamed yet* — a turn that already emitted deltas must not be replayed into
  the sink.
- **Stream stall timeout**: no bytes for N seconds ends the turn instead of
  hanging forever (test-tunable).
- **Catalog staleness**: on startup, refresh the models.dev catalog in the
  background when the stored copy is older than a week. Keeps `--model`
  validation and pricing current without a manual command.
- **`harness sessions [list|rm <id>|prune --older-than D|--keep N]`**: sessions
  are files nobody could clean up. Deletion refuses ids that a live daemon
  resident holds (new `session/residents` wire method), because unlinking a file
  a daemon is appending to loses that data silently.
- **Heartbeat failure backoff**: a schedule whose turns keep failing backs off
  (double, capped at 1h) instead of retrying at its interval forever; one
  success resets it.

Acceptance: fake-server tests for retry (2 requests, second succeeds) and stall
(timeout error); unit test for backoff; CLI test for `sessions rm` refusing a
resident-held session; catalog staleness unit test.

## 2. Contention: no engine lock across provider I/O

`run_turn_loop` holds the engine lock for the whole turn (daemon.rs:1390), so
`/stats`, `/model`, goals and heartbeat checks queue behind a slow model.

Fix: split the turn into a **prepare → provider → apply** cycle the daemon
drives, locking only around the state changes and releasing the lock for the
provider call (the multi-second-to-minute part) and tool I/O. `Engine::run_turn`
stays as a thin wrapper so embedded callers are untouched.

Acceptance: a new daemon e2e test proves `session/stats` answers *while* a slow
turn (mock delay) is in flight.

## 3. Protocol

- **Broadcast fan-out**: `SharedClient` holds one client today, so a second
  attacher takes over the stream. Make it a set: every attached client receives
  updates; a permission question goes to all and the first answer wins.
- **Strict ACP framing**: require `jsonrpc: "2.0"` (else -32600), ACP-shaped
  error codes, and `available_commands_update` after `session/new`.
- **TCP transport**: an optional `HARNESS_TCP=host:port` listener beside the
  unix socket, with a required token (a TCP socket is reachable by other local
  processes, so the unix socket's filesystem permissions no longer protect it).

Acceptance: daemon e2e asserts both attached clients see a turn's deltas; ACP
test asserts -32600 and the commands update; TCP e2e asserts a token-gated turn
and a rejected bad token.

## 4. Sandbox and extension surface

- **Sandbox**: `HARNESS_SANDBOX=off|workspace-write|read-only` wraps `bash`
  commands in the platform sandbox (macOS `sandbox-exec`, Linux `bwrap`) with a
  profile that permits the workspace and denies the rest, degrading to a clear
  refusal when the tool is missing rather than pretending.
- **Extensions, first slice**: project prompt templates in `.harness/commands/*.md`
  exposed as `/name` everywhere a command runs, with `$ARGUMENTS` substitution —
  the smallest extension point that is genuinely useful, and the seam the rest
  (hooks, tool plugins) grows from.

## 5. CI and release

`.github/workflows/ci.yml` (fmt, clippy -D warnings, tests on macOS + Linux),
`release.yml` (tag → binaries + checksums), and `install.sh` (`curl | sh`).
No remote exists yet, so these are inert until one does; `cargo clippy` is run
locally now so CI would actually pass.
