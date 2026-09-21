# Tier 2 — capabilities beyond the TUI

Tracker for the remaining roadmap items. 2026-09-20.

## Done

- **Harness as an MCP server** (`harness --mcp-serve`). Stdio MCP over the same
  `Tool` layer, so workspace confinement and the bash metacharacter rule apply
  unchanged; the gate is non-interactive, so writes and non-allowlisted
  commands are refused unless `HARNESS_MCP_YOLO=1`. Verified by
  `cli/tests/mcp_server_e2e.rs`, which drives the server with the harness's own
  MCP client and checks the refusals leave nothing on disk.
- **Context accounting: measured, not guessed.** `SessionManager::last_input_tokens`
  returns the provider's own input+output count for the latest sample while no
  compaction has landed since; `Engine::context_tokens()` prefers that number and
  falls back to the calibrated estimate. The compaction trigger and the context
  bar now use it, and the UI marks an estimate with `~` (`ctx ~12k/200k`).

  Deviation from the original plan, deliberately: the item said "tiktoken-rs". A
  bundled BPE table is exact for one model family, and the models in use here are
  Claude and DeepSeek — so it would be a differently-shaped guess with a
  megabyte attached, and still wrong. Instead the estimate is calibrated
  (`estimate_text_tokens`: ~4 chars/token for words, ~2 for punctuation and
  digits, per-character for non-ASCII, fractional whitespace) and is superseded
  by the provider's own count wherever that is available. 5 tests pin the
  behaviour, including that prose stays near chars/4 and that punctuation costs
  more than words of the same length.

## Hygiene fix found while cleaning up

Testing left **47 orphaned daemons** behind (my PTY probes SIGKILL the client, and a
daemon outliving its client is the feature, not a bug). The real defect was that a
daemon whose socket file had been unlinked kept running forever, unreachable and
still holding the workspace lock. `serve` now checks its own socket every couple of
seconds and exits when it is gone; covered by
`daemon_exits_when_its_socket_disappears`.

## Remaining

- ~~Cron schedules + daemon-resident sessions~~ **daemon-resident sessions
  landed** (`docs/plan-p8-daemon-resident.md`): a session now belongs to the
  daemon, not to a connection — clients attach and detach, a second client
  joins rather than being refused, a turn outlives its client, and heartbeats
  keep firing with nobody attached (that was the reason this item existed).
  Detached approvals are denied rather than left hanging. Cron expressions landed too
  (`docs/plan-p8b-cron.md`): five-field local-time schedules, `/heartbeat cron
  0 9 * * 1-5 <text>`, validated at set time and re-armed by next occurrence.
- ~~Synchronous `subagent`~~ **landed**: `subagent` spawns one child and waits for
  its answer (default 300s, then it points at the notice and lets the parent carry
  on). `spawn` remains the default recommendation — this is for when the next step
  depends on the result. Tests cover both the inline report and the timeout.
- ~~Worktree cleanup sweeps~~ **landed**: `harness worktrees` lists the child
  worktrees with their branch and whether that branch is already contained in
  `HEAD`; `harness worktrees prune <name>` removes a merged one and *refuses* an
  unmerged one unless `--force`, whose message says exactly what is at stake. The
  branch is always kept, so even a forced prune leaves the child's commits
  recoverable. Verified with the real binary: merged → prune, unmerged → refused
  with exit 1, `--force` → checkout gone and branch kept.
- **Strict ACP framing** (currently ACP-shaped JSON-RPC), **multi-client
  fan-out** (>1 subscriber), **TCP transport**.
- **Code-graph indexing as MCP** (the last P5 item).

## Verification debt — cleared 2026-09-20

All three paths had only ever run against the mock. Each now has a real-model
run (DeepSeek, ~$0.10 total):

- **Compaction**: a 93 KB history compacted to `~33k tokens → brief + 5 retained
  message(s)`, and the following `/stats` showed `ctx ~2908` *with* the estimate
  marker — i.e. the provider measurement was correctly discarded once a
  compaction reshaped the context.
- **MCP bridging**: the model called `mcp__fake__echo` by name, the call reached
  the child server over stdio, and `echoed: hello from mcp` came back into the
  transcript.
- **Worktree session**: `/worktree demo` then a real turn wrote `made.txt` inside
  `.harness/worktrees/demo/` while the main checkout stayed clean — isolation
  held with a live model driving the tools.

No defects surfaced in these three runs; the bugs they were expected to find
(notices, tool-result shape, window sizes) had already been found and fixed by
earlier real-model sessions.
