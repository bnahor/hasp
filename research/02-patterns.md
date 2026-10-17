# Cross-Cutting Synthesis

What the sources agree on, where they genuinely differ, and what each difference buys. Mapped to your three goals: context robustness, workspace robustness, lightweight inter-agent comms.

---

## 1. The invariant core (your intuition was right)

Every harness studied — pi, Prime, Grok Build, Codex — is the same machine wearing different clothes:

```
client(s) ──wire──▶ runtime ──▶ session(s) ──▶ turn loop ──▶ provider(s)
                      │             │              │
                   storage      tool registry   compaction
                   (event log)  + permissions   + cache stats
```

1. **Event-sourced session log** — append-only JSONL; the log is canonical; UI state is derived, never primary.
2. **Turn loop** — one user prompt → N model samples with tools executed between samples → done. "Turn ≠ sample" is universal.
3. **Async submission/event queue** between clients and the loop (Codex's core pattern; ACP's session/update; Prime's daemon protocol). This is what makes cancel, stream, multi-client, and background work possible.
4. **Provider translation layer** with usage accounting where cache read/write are first-class fields.
5. **Tool registry + permission/sandbox path** around execution.
6. **Context assembly pipeline**: system prompt + context files + tool schemas + history → request; with compaction as the pressure valve.
7. **Process topology choice** (in-process vs daemon) that determines whether agents outlive terminals.

The differences are all *placement decisions* within this skeleton.

## 2. Context lifetime — the real design space

Four distinct strategies appear, and they compose:

| Strategy | Who | What survives | Cost |
|---|---|---|---|
| **Append-only transcript + cache discipline** | pi | Everything, in-window, mostly cache-read-priced | Window fills; needs compaction eventually |
| **Checkpointed compaction** | pi, Grok Build, Codex, Prime | Plain-text handoff summary + retained tail (+ kernel state in Prime) | One counted cache reset per compaction |
| **Externalized working state** | Prime (kernel), CodeGraff (tool-result handles + paging) | Variables/fat results outside the model context entirely, addressable on demand | State lives where the model must call back into it |
| **Provider-sealed state** | (hosted Codex/Responses multi-agent, anti-pattern per Earendil) | Hidden server-side continuity | Zero portability, zero audit — rejected |

Rules that fall out (adopt as invariants):

- **Append-only by default; rewrites are events, not edits.** Mid-history mutation wrecks the KV prefix and destroys evidence. Compaction/branching are *entries* in the log with their own identity.
- **Every lossy transition emits a readable handoff.** Plain text, structured (Grok's 9-section brief is the best template), carrying prior summaries forward.
- **The local log is canonical; providers mirror at most.** No opaque blob is ever the sole carrier of meaning — including in our own inter-agent messages (Codex's encrypted multi-agent v2 is the cautionary tale).
- **Stable prefix is sacred**: static context (system prompt, context files, tool schemas) first and never reordered (aider); tool changes additive-only where the model supports it (pi); no timestamps/randomness in the prefix; cache read/write tracked per message and surfaced (pi's R/W/CH, miss notices with cause).
- **Fat payloads never enter context** (CodeGraff): tool outputs above a bound become out-of-line handles paged via `read_tool_result` — the fix for the dense-payload residency problem (colbymchenry/codegraph measured ~80% more residual occupancy from verbatim graph dumps). Keep a context ledger of resident handles so compaction can target them.
- **Prune exactly when the cache is already dead**: when the idle gap exceeds the provider's cache TTL (60 min vs the 1 h TTL — "guaranteed expired for all users, so we never force a miss that wouldn't have happened"), clear old compactable tool results *before* the request. The full rewrite is happening anyway; shrink it. This dissolves Earendil's prune-vs-cache break-even rather than picking a side.
- **Compaction output is memory, not just a reset** (CodeGraff's summary-tree): every compaction summary becomes a retrievable node in a hierarchical episodic store; `recall` is a model-*pulled* meta-tool — retrieved memory is append-only content, never prefix mutation.

## 3. Session model — tree, not list

Pi's tree-structured session (every entry has `id`/`parentId`, active leaf, branch summaries, labels, fork/clone with parent pointers) is the strongest of the formats studied:

- Rewind/branch without file duplication; alternatives stay inspectable.
- Compaction entries with `retainedTail` become **self-contained checkpoints** — context rebuild never walks pre-compaction history.
- Extension state (`custom`) vs extension-injected context (`custom_message`) cleanly separated.
- Header carries `cwd` (and `parentSession`) — the seed of workspace binding, which pi underuses (see §5).

Codex adds the failure lesson: resume must reconstruct *world* state — files, env, running processes — not just replay the transcript, or you get task drift and duplicate edits after forks/rollbacks.

## 4. Inter-agent comms — the design space

| Dimension | Options observed | Best fit for us |
|---|---|---|
| Topology | Hierarchical delegation (parent→child→parent); peer handoffs; supervisor-mediated | Hierarchical by default (fresh context per child = pi/Prime/Codex consensus), peer messaging as a capability, not the default |
| Child context | Fresh minimal task (pi subagents, Prime, Grok worktrees; CodeGraff hard-caps depth at one level, everything-in-the-prompt) vs full history handoff (LangChain-style) | Fresh. Context isolation is the point |
| Isolation | Shared cwd (dangerous with parallel writers) vs managed worktree per mutating child (pi-subagents, Grok `xai-fast-worktree` COW) | Worktree-per-mutating-child, one writer per checkout |
| Message delivery | fire-and-forget; queued mailbox; **auto/steer/follow_up modes with delivered/queued receipts** (Prime); always-queued, drains at receiver's next tool round, no receipts (Claude Code SendMessage); one call + `trigger` flag, queued-never-awaited (CodeGraff steering design) | Prime's receipts + CodeGraff's `trigger` flag and never-await invariant (deadlock-proof by construction); CC's UDS cross-session addressing as the interop escape hatch |
| Wake-ups | polling; child-exit notices as steering (Prime background-command notices); held continuations while children run (Prime autonomous mode); bg_wait-style subscriptions | Event-driven: child completions and notices are the wake-ups; timers only as keep-alive valves |
| Auditability | Plaintext in both transcripts (pi) vs encrypted payloads (hosted Codex anti-pattern) | Every message persisted in *both* sender's and receiver's logs, plaintext |
| Wire | in-process channels; JSON-RPC over stdio (**ACP**); Unix-socket daemon (Prime supervisor, Grok leader); WebSocket | JSON-RPC 2.0 everywhere; ACP for client↔agent; a small mailbox protocol for agent↔agent |

Two complementary wires, don't conflate them (Grok's clearest lesson):

- **Client ↔ agent**: ACP (or an ACP-shaped subset): session/new, session/prompt, session/update streaming, permission requests, cancel. Makes TUI/headless/IDE/second-window all just clients.
- **Agent ↔ agent**: spawn (admission handle, never the answer), message (receipt: delivered|queued), observe (roster), broadcast within family. Routed by a supervisor/leader process. Size/rate/pending limits enforced at the router.

## 5. Workspace robustness — killing the Cursor failure mode

Your complaint decomposes into three distinct bugs, with three distinct fixes:

1. **Ambient cwd** — the agent infers "the repo" from wherever it happens to be pointed.
   → **Workspace identity is resolved once, pinned in the session header, and verified on every resume**: canonical path + git root + worktree + branch + HEAD at creation. Mismatch on resume (moved checkout, switched branch, different worktree) = hard warning + explicit reconfirm, never silent adoption. Session lists are keyed by workspace identity, so `/resume` in a worktree shows that worktree's sessions. Codex's resume pain shows why verification must cover world state (dirty files, running processes), not just transcript presence.
2. **Escapes** — tools operating outside the intended checkout, colliding with sibling features.
   → Path policy in the tool layer: relative paths resolve against the session-pinned root; escaping the root requires an explicit permission event (which is protocol-visible per ACP, so any client renders it). Subagents that mutate get **managed worktrees**; the main checkout has exactly one writer.
3. **Re-derived context** — every session re-discovers AGENTS.md/conventions from scratch.
   → Deterministic discovery chain (workspace root → ancestors → home) assembled in a **stable order**, cached in the prefix; plus an optional per-workspace durable memory file (Prime's continual-harness idea, minus the self-modifying system prompt: keep it supplemental and reviewable).

The trust boundary is the folder (Grok's folder-trust) — trust is a property of the workspace, stored with its identity.

## 6. Process topology — how much daemon?

| | pi (none) | Prime/Grok (daemon) | Codex (per-mode) |
|---|---|---|---|
| Agent outlives terminal | No | Yes (detach/reattach) | Partial (resume from disk) |
| Multi-client on one live agent | No | Yes (leader/attach) | app-server |
| Cost | Simplest | Background service to manage | Middle |

Given your goals (state maintenance, background agents, inter-agent routing), a single long-lived core process is justified — **decided (2026-09): daemon default-on**, Prime-style: first invocation auto-spawns it, TUI/headless attach over the socket, `--no-daemon` for embedded one-shots. Grok's opt-in framing supplies the failure-mode playbook (stale-socket detection, crash recovery from JSONL, binary-upgrade handoff), and since the TUI is already an ACP client, multi-client is "more clients on the same wire", not a second IPC design.

## 7. What to steal from whom (summary)

- **pi**: session tree format + checkpointed compaction; minimal toolset & <1k-token prompt; cache observability; additive tool loading; extension entry types; per-cwd session storage.
- **Earendil rules**: portability invariants; cache break-even math; miss diagnostics.
- **Prime**: daemon/worker split; kernel-persisted working state (worth considering *later* — it's the biggest divergence from pi-light); agent messaging semantics (modes/receipts/limits); goals/heartbeats/schedules; held-continuation wake-up logic.
- **Grok Build**: ACP-first wire; leader mode; sampler as an isolated concern; 9-section successor brief with verbatim-quote anti-drift; worktree subagents; "one approval is not a blank check" doctrine; PTY e2e tests.
- **Codex**: submission/event queue as *the* internal protocol; one core, many faces (incl. self-as-MCP-server); rollout + SQLite persistence; the resume-must-rebuild-world lesson.
- **CodeGraff**: tool-result handles + paging; comptime-stable tool rendering; structural path confinement that `/yolo` cannot lift; one-level subagents; `execTool` as the single hook choke point; self-readable trace file; eval rig with honest list$; the steering design (opt-in `retained` spawns, harness-assigned `sa-` ids, `trigger` delivery, queued-never-awaited); signed fitness channel (HMAC over score tuples, key outside cwd); memory layer with compaction-as-memory-write.
- **aider**: cache-friendly ordering; keepalive pings.

## 8. Language — decided: Rust (2026-09)

Rust is the majority implementation language among the references (Codex, Grok Build; oMLX) — but CodeGraff is the existence proof for **Zig**: one 3.7 MB binary, zero deps, ~12 MB RSS per turn, structured `std.Io` concurrency with no async runtime, where their prior Rust implementation reportedly used 4.3× the memory for the same turn. It delivers the properties you named (type safety, no GC pauses next to long-lived daemons, single static binary) and has the exact crate ecosystem this needs (tokio, ratatui, JSON-RPC, Landlock/Seatbelt bindings). Pi and Prime are TypeScript — the main thing TS buys is a dynamically loadable extension story; in Rust the equivalent is protocol-based extension (subprocess/MCP/ACP, skills-as-markdown), which all sources converge on anyway. Detail in `03-architecture.md` §9; crate map and performance analysis in `04-performance.md`. Decision: **Rust** — ecosystem breadth (tokio, ratatui, wasmtime, gix) outweighs Zig's footprint edge for a solo build; Zig remains the existence proof that the whole thing fits in megabytes, which keeps us honest about dependency sprawl.
