# Proposed Architecture — Granular

Working name: **harness**. Everything here is a placement decision inside the invariant core from `02-patterns.md` §1. Rust types are sketches (decision on language in §9), not commitments.

---

## 1. Design principles (each traceable to a source)

1. **The local event log is canonical** (Earendil portability rule 1). Every state change is an appended entry; UI is a projection.
2. **Append-only transcript; rewrites are events** (pi). Mid-history mutation is forbidden at the API level.
3. **Workspace identity is resolved once, pinned, verified on resume** (anti-Cursor; Codex resume lesson).
4. **Stable prefix is sacred** (pi/aider): static context first, volatile last; tool changes additive-only; cache read/write tracked and surfaced.
5. **Two wires, never conflated** (Grok Build): ACP-shaped JSON-RPC for client↔agent; mailbox protocol for agent↔agent.
6. **Every lossy or delegating transition emits a readable, persisted record** (Grok brief; Earendil rule 5): compactions, branch switches, subagent tasks and replies.
7. **Minimal core, protocol-based extension** (pi): 4 tools, sub-1k-token prompt; everything else is MCP/skills/subprocesses.
8. **Processes are for lifecycle containment, not security** (Prime); real isolation is worktrees + opt-in OS sandbox.

## 2. System shape

```
┌─────────────┐  ┌─────────────┐  ┌──────────────┐
│ TUI client  │  │ headless    │  │ 2nd window / │
│ (ratatui)   │  │ script      │  │ IDE adapter  │
└──────┬──────┘  └──────┬──────┘  └──────┬───────┘
       │    ACP-shaped JSON-RPC 2.0       │
       │  (stdio embedded | unix socket → core) │
       ▼                                 ▼
┌──────────────────────────────────────────────────┐
│ core (single optional long-lived process)        │
│  ┌────────────┐  ┌─────────────────────────────┐ │
│  │ router     │  │ session workers (per root   │ │
│  │ (attach,   │  │ session tree)               │ │
│  │ agent bus, │  │  turn engine ──► sampler    │ │
│  │ roster)    │  │  tool exec ────► workspace  │ │
│  └────────────┘  │  compactor                 │ │
│                  └─────────────────────────────┘ │
│  provider adapters │ MCP clients │ skills        │
└──────────────────────────────────────────────────┘
        │                          │
        ▼                          ▼
  ~/.hasp/sessions/...    workspace checkouts
  (JSONL trees, per          (main cwd + managed
   workspace identity)       worktrees for children)
```

Default run mode is **daemon** (decided 2026-09): the first invocation auto-spawns `harnessd` (Unix socket under `~/.hasp/`, mode 0600) and attaches as its first client; later windows, `hasp -p`, schedules, and peer agents all attach to the same core — one MCP liveness set, one provider connection pool, detach/reattach, in-flight turns survive terminals (Prime's default-on precedent). Failure modes are first-class from day one: stale-socket detection, worker crash recovery from JSONL + artifacts, binary-upgrade handoff, `hasp doctor` / `shutdown`, and `--no-daemon` embedded mode as the escape hatch for one-shots and sandboxes.

## 3. Workspace layer (kills the Cursor failure mode)

### 3.1 Identity resolution

```rust
struct WorkspaceIdentity {
    canonical_root: PathBuf,     // git rev-parse --show-toplevel (or cwd if no repo)
    worktree_path: PathBuf,      // --show-toplevel of THIS worktree
    is_linked_worktree: bool,    // git worktree list membership
    branch: Option<String>,
    head: Option<Oid>,
    trust: TrustLevel,           // FirstUse | Trusted | Untrusted (folder trust, Grok)
}

fn resolve(path: PathBuf) -> Result<WorkspaceIdentity>;  // runs once per session start/resume
```

### 3.2 Binding + verification

- Session header stores the full identity (§4.1). Resume flow:

```
resolve(cwd) ──► compare with header ──► match ──────────► load, continue
                    │
                    ├─ path moved/symlinked ─► offer rebind (update header entry)
                    ├─ branch/HEAD changed ──► warn: "session was on main@a1b2c,
                    │                           now on feat/x@d4e5f" → reconfirm
                    └─ different worktree ────► refuse silently continuing; list
                                                sessions *for this worktree*
```

- Session listing is **keyed by workspace identity** (pi keys by cwd string; we key by resolved identity so linked worktrees never share a session pool unless identical).
- **World-state note in header**: dirty-file set + untracked count at session start (Codex lesson: resume drift). On resume with a diverged tree, surface the diff summary to the user before the first prompt.

### 3.3 Path policy (tool layer)

```rust
enum PathCheck { InsideRoot, OutsideRoot }  // relative paths resolve against pinned root
// OutsideRoot (or escaping via symlink) ⇒ PermissionRequest event over the wire —
// rendered by whatever client is attached (ACP: permissions are protocol-visible)
```

- One writer per checkout: the root session may mutate the main worktree; **mutating subagents get managed worktrees** (`harness-wt/<child-id>`, removed on success, kept on failure with a pointer entry). Review/scout children run read-only against the parent root.
- Confinement is **structural, not a trust toggle** (codegraff's rule): relative-path-only file tools; absolute paths and `..` rejected; not bypassable by any yolo/autopilot mode. Bash approvals use a metacharacter rule — commands containing `;|&><$`, backticks, or control whitespace never prefix-match an approval (chaining smuggles second commands).
- Session-level worktree mode (Claude Code's `EnterWorktree`, explicit user request only): creates `.hasp/worktrees/<name>` on a new branch from HEAD and *switches the session's pinned root into it*; exit keeps or removes. Complements child isolation — "isolate me" vs "isolate the child".

## 4. Session model

### 4.1 File format (JSONL tree, pi-derived)

```jsonc
// line 1 — header
{"type":"session","format":1,"id":"uuid","created":"...","workspace":{...WorkspaceIdentity...},
 "dirty_state":{"modified":3,"untracked":1},"parent_session":null}

// entries — every line: {type, id, parentId, ts}
{"type":"message","id":"a1b2","parentId":null,"ts":"...",
 "message":{"role":"user","content":"..."}}
{"type":"message","id":"c3d4","parentId":"b2c3","ts":"...",
 "message":{"role":"assistant","content":[...],"provider":"anthropic",
            "model":"...","usage":{"input":0,"output":0,"cacheRead":91234,"cacheWrite":0,
            "cost":{"total":0.04}},"stopReason":"toolUse"}}
{"type":"tool_result","id":"e5f6","parentId":"d4e5","toolCallId":"call_1",...}
{"type":"compaction","id":"...","summary":"<9-section brief>","tokens_before":148000,
 "retained_tail":[ /* AgentMessage[] — self-contained checkpoint */ ],
 "details":{"read_files":[...],"modified_files":[...]}}
{"type":"branch_summary","from_id":"...","summary":"abandoned path digest"}
{"type":"spawn","child_session":"path","task_digest":"readable task","model":"..."}   // audit rule 5
{"type":"agent_message","direction":"in|out","peer":"child:auth-reviewer",
 "mode":"auto","receipt":"delivered","content":"plaintext"}
{"type":"model_change","provider":"...","model_id":"..."}
{"type":"custom","custom_type":"extension-id","data":{...}}          // NOT in context
{"type":"custom_message","custom_type":"...","content":"..."}       // in context
{"type":"label","target_id":"...","label":"checkpoint-1"}
```

- Tree walk = context build: leaf→root; `retained_tail` makes compactions checkpoints; branch summaries attach at the branch point. Same algorithm as pi's `buildContextEntries` (proven, simple).
- Storage: `~/.hasp/sessions/<workspace-key>/<yyyymmdd-hhmmss>_<uuid>.jsonl` where `<workspace-key>` hashes from the identity (stable across worktree moves via rebind entries).

### 4.2 Context assembly order (cache contract)

```
[system prompt] → [context files: AGENTS.md chain, workspace memory] → [tool schemas]
→ [history / compaction brief + retained tail] → [new user input]
```

- AGENTS.md discovery: workspace root → each ancestor → `~/.hasp/AGENTS.md`; concatenated in that fixed order; **byte-stable between turns**; changes are an explicit `/reload` event (a counted cache reset, not a silent miss).
- Tool schemas: fixed order, sorted by name; additive loading only where the provider supports deferred tools (pi's mechanism); otherwise a tool-set change is an explicit event.
- No timestamps, no random IDs, no "dynamic" preamble in the prefix.
- **Tool outputs above a size bound never enter history**: stored as out-of-line per-run artifacts, referenced in context as a bounded handle the model pages via a `read_result` tool (codegraff's `read_tool_result`). Keeps transcripts lean, sessions portable, and compaction cheap.
- **Volatile state rides the tail**: todos, progress, environment notices are append-only reminders near the conversation end — never rewritten into the frozen prefix.

## 5. Turn engine

```
Submission queue (per session worker):
  Prompt | Steer | Cancel | FollowUp(from schedule/goal/child notice)

turn(prompt):
  1. build context (§4.2)          — if projected tokens > threshold(0.85 × window): compact first
  2. sample (streaming)            — sampler owns retry, cancel, backpressure
  3. for each toolCall:            — parallel where independent (read-only batch)
       path check → permission policy → execute → append tool_result
  4. repeat 2–3 until stopReason == stop | aborted | length
  5. post-turn: auto-compact check, goal/schedule bookkeeping, cache stats update
```

- Steer semantics: queued input is delivered at the next safe boundary (mid-sample = buffered; mid-tool = injected after the batch) — Prime's modes (`auto`/`steer`/`follow_up`) apply to *all* inbound, not just agent messages.
- Cancellation propagates: client cancel → queue drop or in-flight abort (sampler cancels the stream; running tools are signalled; partials are appended as aborted entries — the log stays truthful).

## 6. Provider layer

```rust
trait Provider {
    fn stream(&self, req: AssembledRequest) -> Stream<SampleEvent>;
    fn capabilities(&self) -> Caps; // deferred_tools, cache_control, thinking_levels...
}
```

- AssembledRequest carries cache directives: explicit `cache_control` breakpoints after system/context/tools and after the last stable turn (Anthropic-style) when supported; otherwise automatic prefix caching + a stable session key header.
- Usage accounting: `cacheRead`/`cacheWrite` per assistant entry; session-level rollups; **miss detection**: if uncached_input > expected_prefix − epsilon on a request whose prefix should have hit, emit a `cache_miss` notice entry with best-guess cause (idle gap > TTL, model change, tool-set change) — pi's observability, made a log event so headless runs get it too.
- Keepalive: optional idle pinger (aider) off by default.
- **Cache-break attribution**: snapshot per-request {system hash, tools hash, **per-tool schema hashes**, cache-control hash (TTL/scope flips), beta list, effort, extra-body hash}; diff on change to *name the cause* of each break — 77% of breaks are one tool's description changing (dynamic agent/command lists are the usual culprits). Flip-prone flags (overage, auto-mode, cache-editing beta) are **latched session-stable** so they stop breaking the cache. Break diffs written to a debug dir. Our `cache_miss` entries carry a `cause` field from day one.

## 7. Compactor

- Trigger: post-turn projection > 85% (Grok) or overflow error mid-turn; manual `/compact [focus]`.
- Execution: standalone request, tools off, dedicated summarizer system prompt, *may use a cheaper model*; input = serialized older entries only.
- Output: 9-section successor brief (intent / concepts / files+snippets / errors+fixes / problem-solving / **all user messages** / pending / current work / next step with verbatim quote), with prior briefs carried forward (Grok's template is the best in class).
- Persisted as a `compaction` entry with `retained_tail` (token-budgeted recent messages, default ~20k) → checkpoint semantics.
- Cache accounting: compaction is booked as a *reset*, never a *miss* (pi's distinction).
- **Tiered, not binary**: tier 1 — microcompact: clear stale results of compactable tools (Read/Bash/Grep/Glob/Web/Edit/Write) to `[Old tool result content cleared]` markers; tier 1.5 — **timed pruning**: idle gap > provider cache TTL ⇒ clear before the request (the rewrite is happening anyway; never forces a miss that wouldn't have happened); tier 2 — the 9-section brief + retained tail. Warning and error thresholds each buffered ~20k tokens; warning suppression state so it doesn't nag.
- **Compaction is a memory write** (codegraff): every summary is appended to the workspace episodic store (`summary-tree`), retrievable later via `recall`; `/save` likewise. The lossy reset becomes an accumulating hierarchy — and `recall` is model-pulled, so the cached prefix is never mutated.

## 8. Inter-agent communication

### 8.1 Client wire (ACP-shaped, JSON-RPC 2.0)

Adopt ACP method shapes rather than invent: `initialize` (capability negotiation), `session/new`, `session/load`, `session/prompt`, `session/update` (streaming notification), `session/cancel`, permission request/response, `_meta` extension points. Stdio for embedded/headless; Unix socket for leader mode. Everything the TUI can do, a script can do (Grok's platform property; Codex's one-core-many-faces, including exposing the harness itself as an MCP server later — v2).

### 8.2 Agent bus (mailbox, routed by the core)

```rust
enum BusMsg {
    Spawn { name: String, task: String, mutation: bool, model: Option<ModelId> }, // → AdmissionHandle
    Message { peer: PeerId, content: String, mode: Mode },                        // auto|steer|follow_up
    Observe,                                        // roster: id, name, status, session path
    Notice { kind: ChildExit | BgCommandDone | GateResult, payload: String },
}
enum Receipt { Delivered /* reached idle context */, Queued /* accepted for later */ }
```

Rules (Prime-derived, with the audit fix):
- `spawn` returns an admission handle (child id, session path, model) — **never the answer**. Results arrive only as messages or artifacts.
- Children get fresh sessions with a minimal task brief; mutating children get managed worktrees (§3.3). Child usage is attributed to the parent.
- Every message is persisted **plaintext in both transcripts** as `agent_message` entries — auditable per Earendil rule 5 (explicitly rejecting hosted-Codex's sealed payloads).
- Router enforces size/rate/pending-queue limits; sender identity derived by the core, not asserted by clients.
- Wake-ups are events, not timers: while children run, the parent's timer-driven continuations are held; child-exit and background-completion notices steer the parent at the next safe boundary; one keep-alive valve per long window to unblock hung children (Prime autonomous-mode logic).
- The minimal viable version of all of this is codegraff's: subagent = the same Agent type, one level deep, verbatim final text as the tool result, structural gating — start there, and add the bus when a second long-lived agent actually exists.
- Identity is harness-assigned and **stable across restarts** (`sa-<ordinal>-<hash>` style); model-chosen names are aliases; sender identity is never model-supplied (codegraff steering design).
- **One delivery call with a `trigger` boolean** (codegraff, chosen over Codex's two-tool `send_message`/`followup_task` split): running child → queued into its next turn; idle + retained → `trigger:true` rehydrates from transcript + registry and runs a turn; completed + unretained → actionable error. The expensive case is visibly the exception.
- **Queued-and-never-awaited is the hard invariant** — no delivery call may block on the recipient; mutual sends between busy agents must not deadlock.
- **Retention is opt-in per spawn** (`retained: true`): only retained children persist transcripts and stay addressable after completion — persistence is a cost decision made at spawn time, when the parent knows whether a follow-up is likely.
- Cross-session / cross-machine addressing is the v2 interop path; v1 stays family-scoped.

### 8.3 Long-running surfaces (v1.5)

- `/goal` persistent objective + budget; `/heartbeat` user-owned recurring instruction; agent-created heartbeats (distinct namespace, cannot touch the user's); cron/one-shot schedules with claim-before-deliver and tick coalescing. All flow through the same submission queue — they're just more producers.

## 9. Language & extension model — decided: Rust

- **Rust workspace**, mirroring the reference class (Codex ~crates, Grok Build ~70 crates — we'll be ~10):
  `core` (session, context, engine) · `sampler` · `providers` · `workspace` (identity, worktrees) · `tools` · `acp` (wire) · `bus` (agent messaging) · `tui` · `cli` · `mcp-client`.
- Why not TS: you lose dynamic `require()` extensions. But every source converges on *protocol-based* extension anyway: **MCP** for tools, **skills** (markdown + scripts) for playbooks, **ACP** for alternate clients, subprocess tools for anything heavy. Pi's TS extension intimacy is the one thing Rust won't replicate — accept protocol-only extension in exchange for a static binary, a daemon you don't have to babysit, and types on the session format (the thing your whole design hangs on).
- **Zig counterpoint**: codegraff is the existence proof that the whole thing — TUI, providers, MCP, subagents, workflows, compaction — fits in one Zig binary with zero deps and ~12 MB RSS (their prior Rust version reportedly used 4.3× the memory). The trade is ecosystem maturity: Rust's tokio/ratatui/JSON-RPC crates vs Zig's `std.Io` plus hand-rolled everything (codegraff vendors exactly one dependency — the TUI). Deciding question: do you want crates or control?
- Alternative kept open: the session format and both wire protocols are JSON and language-agnostic by construction — a TS/Python client could be added later without touching core.

## 10. Trust, permissions, sandbox

- Folder trust on first open (Grok); stored per workspace identity.
- Permission events over the wire (ACP-shaped), rendered by any client; doctrine baked into the system prompt: reversible-local = free; irreversible/shared = confirm; **one approval is not a blank check**; investigate unfamiliar state before overwriting.
- Hooks: allow/deny/transform scripts on lifecycle events (before tool, after turn) — v1.5.
- OS sandbox (Landlock/Seatbelt profiles) — v2, opt-in, off by default.

## 11. Observability

- Per-request cache R/W + hit rate; session rollup (tokens, cost, re-billed estimate).
- `cache_miss` log entries with cause guesses; `/session` view.
- Every spawn/message/compaction visible in the tree — the log *is* the audit trail (inspection/export/replay/audit/deletion all satisfiable).

## 12. Build phases

**P0 — Walking skeleton (the pi core):** workspace resolve + session JSONL tree + 4 tools + turn loop + one provider + embedded TUI. Cache stats from day one (they shape habits). Per-run JSONL trace from day one too, with the system prompt pointing at it — self-debugging agents are cheap to buy early (codegraff).
**P1 — Context robustness:** compactor with briefs + retained_tail; branch/fork; AGENTS.md chain + stable assembly; resume verification incl. world-state diff; session-level worktree mode; timed pruning trigger (idle > cache TTL).
**P2 — The wires:** ACP over stdio (headless mode) → Unix socket (leader/detach); permission events; PTY-level e2e tests from here on (Grok's pager-pty-harness, codegraff's PTY probes — TUI regressions otherwise surface only when a human hits them).
**P3 — Multi-agent:** bus, spawn with worktrees, receipts, notices, held continuations; harness-assigned stable ids; opt-in `retained` children; single `trigger`-flag delivery primitive.
**P4 — Long-running:** goals, heartbeats, schedules, autonomous budgets + gates.
**P5 — Substrate (optional):** graph index à la CodeGraph as an MCP server with auto-sync + resident-context eviction policy; MCP client surface; self-as-MCP-server.

## 13. Open questions (for you)

1. ~~Which "Codegraph"~~ — resolved: `justrach/codegraff`. A follow-up pass over its `docs/` (ADRs, agents-panel, embedding, local-learning, skills) is worthwhile if you want more of its mechanisms catalogued; say the word.
3. ~~Language~~ — decided (2026-09): **Rust**. Crate map and performance analysis in `04-performance.md`.
4. KV-cache blog memory: Earendil's series fits ("cache validation", KV internals) — but if you actually meant aider's caching docs, say so and I'll re-weight.
5. ~~Daemon default~~ — decided (2026-09): **default-on** (Prime-style); §2 updated with the failure-mode list; `--no-daemon` stays as the escape hatch.
6. ~~Persistent kernel~~ — investigation approved: three ascending options (resident PTY → resident script worker over the typed host bridge → wasmtime component kernel) scoped in `04-performance.md` §6. Revisit when the first long session shows compaction losing working state — that's the measurable signal.
