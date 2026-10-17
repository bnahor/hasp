# Edge agents + personal assistant: integration report

Date: 2026-10-18. Scope: local implementation, tests, staged setup docs and
**local commits** (authorized mid-task). Nothing was pushed, merged, deployed,
installed or restarted; no account was linked, no credential created, no real
message sent. All branches below exist **only locally**.

## 1. Repositories and commits

| Repo | Branch (local only) | Base (upstream) | Ahead/behind | Commits | Uncommitted, not ours |
|---|---|---|---|---|---|
| harness (bnahor/hasp) | `feat/edge-agent` | origin/main 61cc311 | 7 / 0 | b2b5e66, eec576b, 54408e3, ff850ed, aa21297, 71b1040, 23e8c50 (+ the report commit) | `bench/*` (pre-existing benchmark work, untouched) |
| homelab-mesh | `feat/edge-agent` | origin/main f8a10f6 | 3 / 0 | a1921d5, 7bbbee7, 486fe2a (+ the report commit) | `.DS_Store` files; `workers/mac/hasp-web-setup.sh` (earlier staged M1 setup, deliberately left out) |
| threadline (bnahor/threadline) | `feat/lease-fencing` | origin/main 4626b6d | 1 / 0 | 76d1ac8 | none |
| plutus (bnahor/plutus) | `fix/daemon-replay-on-start` | origin/main 5bda60c | 1 / 0 | 27aad69 | `.DS_Store` files |
| pool (bnahor/pool) | not touched | zig-v1 d6be307 | n/a | none | n/a |

Before committing, every remote was fetched; all four repos were level with
their upstreams, so no sync or conflict resolution was needed. harness also
keeps the local branch `auto-mode-gate` (b2b5e66), which `feat/edge-agent`
contains. Local checkouts for Threadline and Plutus were cloned under
`~/Desktop/projects/` from GitHub at the reviewed commits.

## 2. What each commit does

**homelab-mesh**
- a1921d5 (the eight earlier reliability fixes):
  - no automatic SSH fallback; client submission ids, hub-side dedup and reconciliation;
  - structured retry policy;
  - node-scoped agent credentials with ownership checks;
  - bounded, expiring and cancellable agent requests, GUI grants re-checked at dispatch;
  - schedule submissions tracked properly;
  - shared `lib/meshcommon.py`;
  - SQLite durable state with a migration;
  - shell commands admin-only.
- 7bbbee7: `/api/edge/submit|stop`, for node credentials only, for their own node, with a fixed command line and dedup by (task, fencing token); `edge/demo.py`; staged launchd/systemd templates.
- 486fe2a: `edge/assistant_demo.py` (gateway takeover); gateway and bridge service templates.

**threadline** 76d1ac8:
- leases bound to (peer, client, lease), with server expiry, renew/release, and fencing tokens;
- an explicit state machine;
- credentials bound to swarm, peer and capabilities;
- queued (offline) claims never authorize work;
- receipts and dedup by event id;
- restart-safe lease state;
- MCP tools sorted, plus get/renew/release/cancel/receipts;
- `docs/coordination.md`.

**plutus** 27aad69: the daemon now replays its event log into the CRDT store on start (this was a real bug: state was empty after a restart, and peer ops were lost); a torn log tail is set aside instead of later appends landing after garbage.

**harness (hasp)**
- b2b5e66: MCP tools gated, with an auto mode.
- eec576b, Hasp hardening and recall:
  - read-only bash gate plus `bash.allow` policy;
  - sandbox read-only carve-out;
  - journal durability with torn-tail handling;
  - resident restore after a daemon restart;
  - attenuated read-only children;
  - instruction snapshots;
  - the new `hasp-recall` crate: index, memory, workflows;
  - `session/steer` and a permission-aware wire client.
- 54408e3: `hasp-edge` (worker, exec, dispatch).
- ff850ed: MCP client tests made independent of the user's `~/.hasp/mcp.json` (they failed before on any machine with a user MCP server).
- aa21297, 71b1040, 23e8c50: `hasp-gateway` and `bridges/whatsapp-baileys` (see 3.4).

## 3. Architecture and behaviour

### 3.1 Ownership (no duplicate canonical state)
| Concern | Owner | Where |
|---|---|---|
| Sessions, transcripts, local execution, recall | hasp | each node's own `~/.hasp` (journals canonical; index rebuildable, node-local) |
| Shared task intent, ownership (leases, fencing), results | Threadline coordinator | Mac only (single authority); `threadlined` on each node |
| Runs, queue, concurrency | Dagu via the mesh hub | Railway hub |
| Device identity | mesh hub (node-scoped tokens) | hub SQLite |
| Messaging edge: inbound/outbound, approvals, waiting items, event inbox | hasp-gateway | the active gateway's SQLite (Mac); the standby holds no state until it leads |

Correlation ids: Threadline `task_id` ↔ hub `submission_id` (from task and fencing token) ↔ Dagu `dag_run_id` ↔ hasp `session_id`. The artifact `hasp-session://<node>/<id>` is in every completion result. Gateway turns record the session key and origin chat; approvals record the session; the audit log has every decision.

There is no second scheduler: the edge worker claims tasks and hands them off, and Dagu runs them. The gateway's timer only resumes or reminds about waiting items; recurring schedules stay in hasp heartbeats and Dagu. No SQLite file is written by two machines: the standby restores a `VACUUM INTO` snapshot only after it wins the lease.

### 3.2 Hasp safety, restarts, recall
These are covered in `harness/docs/edge-agent.md`. The reviewer's probe now returns "command not approved" and writes no file.

### 3.3 Edge execution
- A worker claims a task whose `needs:<cap>` labels the node satisfies, persists the lease, and submits a hub edge run.
- When the job actually starts, `hasp-edge exec` re-validates the lease and fencing token with the coordinator.
- A stale job exits 3 without running Hasp.
- Cancellation is followed while it runs, and the result and artifact are published.

### 3.4 Personal assistant (hasp-gateway)
Implemented and tested locally:
- **Adapters behind one interface:**
  - Telegram (Bot API 10.3, long polling, inline buttons, answerCallbackQuery, 429 `retry_after`, deleteWebhook);
  - Signal (signal-cli 0.14.x HTTP daemon: SSE with `Last-Event-ID`, JSON-RPC send, Note to Self in inbox mode, sync-message echoes);
  - WhatsApp (local Baileys 7.0.0-rc14 linked-device bridge with a durable deduplicated log and LID/PN self-chat mapping);
  - the local web view.
  - Bot conversations are kept separate from personal-inbox access (inbox chats become third-party events, never commands).
- **Identity:** owner-issued one-time link codes bound to platform-authenticated sender ids; display names are never used.
- **Sessions:** one personal conversation per person across channels and the web, plus selectable named sessions, `/task` sessions and `/edge` dispatch to a node. Commands: `/status /stop /resume /steer /followup /session /task /edge`, plus owner commands.
- **Messages:** text, attachment references, and a voice-transcription hook (`Transcriber`; none configured means a reference plus a note). Replies are split per platform limit; buttons only where supported, otherwise `/approve ID CODE`.
- **Delivery:**
  - events and cursor are persisted before the ack;
  - dedup by platform event id;
  - per-session serialized turns;
  - responses are saved before delivery;
  - a crash re-delivers the saved text and never re-runs the turn;
  - running turns are reported as interrupted (`/resume` re-runs one deliberately);
  - outbound sends are stamped with the fencing token, with backoff and per-chat pacing;
  - an uncertain send is resent once, visibly marked;
  - echo-loop prevention;
  - no exactly-once claims.
- **Event inbox:**
  - Threadline task results and mesh calendar/reminders are real connectors;
  - email has the interface and a fake only;
  - deterministic rules and quiet hours, an hourly cap on notifications, no model call per poll;
  - third-party content is wrapped as untrusted.
- **Follow-through:** waiting-for-user, waiting-for-third-party and scheduled items, with due times, cancel, restart recovery and the original reply destination. Answers resume the right session in any order (platform reply or `W…` code).
- **Approvals:**
  - a deterministic authority in the gateway process;
  - owner-only decisions via buttons, `/approve` or the web, all through one code path;
  - each decision is bound to card id, one-time nonce, action hash, session, scope and expiry;
  - replays, expiry, wrong code, edited actions and non-owners are refused and audited;
  - drafting is separate from sending, and sending to others also needs the connector's write scope;
  - Hasp permission prompts become owner cards;
  - credentials live only in 0600 files referenced by path (inline secrets are refused), never in agent workspaces or model context.
- **Memory and workflows:** owner `/remember /memory /forget`, plus web edit/confirm/forget; provenance includes the channel. Workflow drafts are activated by hash into ordinary skills; members can't activate them.
- **Web view:** inbox, tasks and turns, waiting items, approvals, delivery, memory, workflows and audit, using the hasp app's design tokens. Owner and agent token scopes are separate.
- **Standby:** a Threadline `gateway-leader` lease, renewed at TTL/3. The standby holds no state, restores the snapshot on takeover, and exits when the lease is lost.

## 4. Tests run (fresh, after the final commits)
| Suite | Command | Result |
|---|---|---|
| hasp workspace (clean checkout of 23e8c50) | `cargo test --workspace --no-fail-fast` | **43 test binaries, 306 passed, 0 failed** |
| Review probe | `python3 /private/tmp/mesh-system-review.NjCCWX/hasp_gate_probe.py` | refused ("command not approved"), `marker_written: false` |
| homelab-mesh | `./tests/run.sh` | **47 passed** |
| Edge demo (2 nodes, real Threadline + hasp) | `python3 edge/demo.py` | **13/13 PASS** (routing, unavailable PC + stale job, cancel, lost reply, worker restart) |
| Assistant demo (2 gateways) | `python3 edge/assistant_demo.py` | **10/10 PASS** (leader-only, takeover with fencing 2 + snapshot, no duplicates, no flapping, returning Mac standby) |
| Threadline targeted | `bun test tests/{threadline,coordination,stress,eval}.test.ts` | **29 pass, 0 fail** |
| Threadline full | `bun test` | 36 pass, 2 fail. The same 2 fail on untouched `main` (22 pass, 2 fail there): `Bun.spawnSync` deadlocks with an in-process coordinator in the enroll and wrapper tests |
| Plutus | `uv run pytest tests/ -q`; `ruff check`; `mypy src` | **68 passed**; clean; clean |
| WhatsApp bridge | `npm test`; `npm run typecheck` | **36/36**; clean |

Gateway acceptance coverage, within the 306 Hasp tests:
- inbound duplicates and replays (Telegram, Signal daemon restart);
- session continuity across channels and a real Hasp daemon restart;
- gateway restart (interrupted turns, saved replies);
- uncertain-send recovery without re-running the turn;
- out-of-order replies;
- cancellation and steer;
- expired approvals, cross-user denial, wrong codes and replays;
- follow-up recovery after a restart;
- memory edit/forget and workflow permissions;
- leadership (standby never polls; the old leader stops sending);
- token scopes, the MCP binary, inline-secret refusal;
- the bridge contract fixture.

The unavailable-PC case for the gateway path is covered by the two demos.

Mutation checks were run on the key protections (gate, sandbox, journal guard, read-only children, mesh retry/dedup/schedule/credentials). Each test fails when its protection is removed.

## 5. Compatibility references
- hasp-edge and hasp-gateway speak Threadline's daemon API as of 76d1ac8: `leaseId`, `fencingToken`, `grant`, receipts, `claimable`.
- The hub edge API needs homelab-mesh a1921d5 and 7bbbee7.
- Bun 1.4.0 (Threadline); Rust edition per workspace.
- Telegram Bot API 10.3; signal-cli 0.14.9 (JRE 25); `@whiskeysockets/baileys` 7.0.0-rc14 (Node ≥ 22.18 for `node src/index.ts`).
- Platform facts: `edge-agent-platform-notes.md`.
- Pool: deferred, not touched. The reviewer flagged a placeholder coordinator/enrollment and timestamp-derived tokens; I didn't re-verify that.
- Plutus: only for mergeable notes/facts later, never leases or approvals.

## 6. Limits (honest)
- **No exactly-once anywhere.**
  - An effect a running Hasp turn already started isn't undone when a lease is lost or a task cancelled; fencing makes the late completion fail.
  - A send whose reply was lost may arrive twice; it is marked.
- **Standby takeover** restores the last snapshot. Anything the old leader did after it is unknown to the new one: a few platform events may be processed twice if they weren't in the snapshot.
- **Same-OS-user processes** could tamper with gateway or Hasp files. Run gateway sessions with `HASP_SANDBOX=workspace-write`, which keeps the agent out of the gateway's directory.
- **Not verified live:**
  - real Telegram, Signal and WhatsApp delivery and account linking;
  - Baileys v7 behaviour notes flagged [UNVERIFIED] in the platform notes;
  - Apple database layouts for the mesh calendar/reminders sources (they need Full Disk Access on the M1).
- **Not implemented:**
  - a live email connector (needs IMAP/OAuth);
  - a speech-to-text engine (only the hook);
  - Telegram streaming drafts and edit-in-place progress;
  - Signal attachment download;
  - the WhatsApp Business API (judged a poor fit).
- **Operational:** the Threadline coordinator is a single authority on the Mac. The bridge keeps its whole log in memory, which is fine at personal volume.

## 7. Staged rollout and what you need to do
1. **Review and push** the four local branches (`feat/edge-agent` ×2, `feat/lease-fencing`, `fix/daemon-replay-on-start`) and open PRs. Nothing has been pushed.
2. **Hub:** deploy homelab-mesh `feat/edge-agent`. The first start migrates `state.json` to SQLite; snapshot the volume first. Then run `workers/deploy-nodes.sh`, enroll the nodes (`mesh enroll --kind node --node mac|pc`), and set `MESH_AGENT_LEGACY_TOKEN=0` once no legacy-token events appear.
3. **Threadline on the Mac:**
   - coordinator with a 0600 env file holding `THREADLINE_AUTH_TOKEN`, reachable from the PC over the tailnet only;
   - one credential per node;
   - threadlined, the Hasp daemon and the hasp-edge worker on both nodes (`homelab-mesh/edge/staging`).
4. **Messaging accounts, yours to create and link:**
   - a Telegram bot token from BotFather;
   - a signal-cli registration (dedicated number recommended);
   - the WhatsApp QR scan of the bridge (secondary number if losing the account would hurt).
   - Put each credential in a 0600 file, then write `~/.hasp-gateway/config.json` (harness `docs/gateway.md`).
5. **Gateway setup:**
   - `hasp-gateway agent-token` and add the printed MCP entry to `~/.hasp/mcp.json`;
   - `hasp-gateway owner-token` for the web view;
   - `hasp-gateway link-code`, then send `/link CODE` from your phone.
   - Install `staging/mac/com.hasp.gateway.plist` and the PC standby unit, with the snapshot path on Mesh Drive.
6. **M1 Full Disk Access** for the node agent's Python (needed for the calendar, reminders and Messages sources).
7. **Live checks** after linking: a message round trip per platform, an approval card per platform, and a forced takeover.
8. **Still open from earlier work:** revoke the exposed Tailscale API key.
