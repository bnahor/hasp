# Source Notes

Per-source findings, with the mechanisms that matter for our design. Receipts inline.

---

## 1. Earendil / pi — the blog series + pi's own implementation

> The "harness with really good blog posts about KV caches and cache validation" is almost certainly this one — pi is the harness you use daily, and its author's blog (earendil.com) carries the definitive series: *Prompt Caching In Agents* (2026-07), *How Compaction Works in Pi* (2026-08), *The Session You Cannot Take With You* (2026-07), *What is a Harness?* (2026-08), *Pi, Minimal and Performant* (2026-08).

### 1.1 Prompt caching mechanics (Prompt Caching In Agents)

- Agents mostly **append** to input; the KV cache holds per-token attention K/V tensors for a *token prefix*. Two prompts that mean the same but tokenize differently share nothing. One changed token invalidates everything after it.
- Where caches live, two models:
  - **Session affinity**: KV stays on the GPU/worker that computed it; next request routed to same worker via session ID. Cheap, constrains scheduling.
  - **Distributed KV blocks**: cache addressable across workers (GPU mem / host mem / SSD, prefix-aware routing, eviction policies). Flexible, expensive to build.
- **Explicit vs automatic caching**: Anthropic's `cache_control` breakpoints (client marks stable boundaries; explicit pricing for writes/reads/TTL) vs automatic prefix caching (provider finds reusable prefix; prompt-cache key helps routing but "does not make different prefixes equal").
- **Tool loadouts trash caches**: tool schemas are prompt-prefix material. Adding/removing/reordering one tool moves the first mismatch near the *start* of the prompt → re-bills the whole conversation. Pi's mitigation: **message-anchored additive tool loading** — new tools recorded on a tool result and introduced mid-transcript (Anthropic deferred tools + `tool_reference`; OpenAI tool-search items), so the old prefix stays intact. Only purely *additive* changes are safe.
- **TTLs**: Anthropic default ~5 min — shorter than a test run or coffee. Claude Code's subscription bumps to 1h; pi exposes `PI_CACHE_RETENTION=long`. Aider's complementary trick: `--cache-keepalive-pings N` pings every 5 min to keep warm.
- **Economics of a miss**: 100k-token session + "continue" after TTL expiry = full re-prefill at uncached input price (+ cache-write premium). Gateway/reseller incentives can be misaligned (miss = bigger invoice) → **cache health must be observable**.
- **Why pi doesn't prune aggressively**: rewriting the middle of history invalidates the tail. Break-even:

  ```
  one-time rewrite cost ≈ surviving_tokens_after_edit × (uncached_price − cache_read_price)
  future savings/turn   ≈ pruned_tokens × cache_read_price
  ```

  Old tool results are also *evidence* the model used for later decisions — deleting degrades behavior even if a summary preserves the gist. Pi prefers a stable append-only transcript; compaction is a deliberate, *counted* cache reset, not a cache failure.
- **Cache observability**: pi footer shows cumulative reads/writes (`R`/`W`) + last-request hit rate (`CH`); `/session` reports totals, cost, and dollars re-billed by significant misses; optional per-miss notices naming the likely cause (model switch, idle gap).
- Common miss causes checklist (worth embedding as diagnostics): idling past TTL, model/provider switch, branch navigation, compaction, tool/reasoning-level changes, dynamic system prompts (timestamps!), extension payload transforms, provider routing/eviction.

### 1.2 Compaction (How Compaction Works in Pi)

- Triggered near context limit after a turn ends (or mid-turn on overflow error); manual `/compact`.
- Mechanism: standalone summarization request — **different system prompt** ("context summarization assistant"), different user message (structured handoff: goal, progress, key decisions), can use a *different, cheaper model*, sees only the serialized older history.
- Retains a recent tail unchanged (configurable token budget, default ~20k tokens ≈ 5–20 turns). Newer format stores `retainedTail` on the compaction entry — a self-contained checkpoint (no need to walk pre-compaction entries).
- Summary is **plain text** → readable, editable, portable across models/providers.
- Compaction breaks the cache by design; counted as reset, not failure.

### 1.3 Session portability (The Session You Cannot Take With You)

- Providers increasingly return **provider-sealed state**: encrypted reasoning blobs, hosted web-search evidence the client never sees, opaque server-side compaction, encrypted subagent messages, response/conversation IDs as foreign keys into provider DBs.
- The portability test: `newProvider.continueFrom(session.export())` should work with zero provider dereferences. Five properties: **inspection, export, replay, audit, deletion**.
- Their 7 rules for a portable harness (adopt as design invariants):
  1. The local event log is canonical; server storage only mirrors it.
  2. Storage is explicit (`store:false` easy/default).
  3. No opaque item is the sole carrier of meaning (readable handoff always exists alongside).
  4. Hosted tools have full-fidelity logs (queries, passages, timestamps, hashes).
  5. **Subagent communication is auditable** — persist exact readable task, messages, results, lineage, model, permissions.
  6. Compaction is inspectable (summary + instructions + lineage of what was discarded).
  7. Artifacts are exportable (content-addressed local archive).
- Concrete failure they document: Codex's multi-agent v2 encrypts inter-agent payloads (`InterAgentCommunication.content` empty in its own rollout; open issue #28058 asks for readable audit copies).

### 1.4 What a harness is (What is a Harness?)

Four functions, useful as a checklist:
1. **System prompt** — per-harness instructions injected every request.
2. **Tools** — described capabilities the model calls; harness executes; model decides when.
3. **Agentic loop** — the review-and-continue cycle until the request is satisfied.
4. **Translation layer** — provider/model independence; user owns the harness and the transcript.

### 1.5 Minimalism performance (Pi, Minimal and Performant)

- Pi: 4 tools (`read`, `write`, `edit`, `bash`), system prompt + tool defs < 1,000 tokens.
- Databricks multi-million-LOC benchmark (their numbers, Earendil's retelling): same model + same thinking effort, cost/task differed >2x across harnesses; "simple harnesses like Pi performed best"; Pi sent ~3x less context per turn ("context discipline").
- Lesson: models are now trained on terminal-style environments; the harness's job is a clean interface + no wasted context, not orchestration bloat. Complexity should be opt-in (extensions/skills), not default.
- Local-model angle: context discipline + stable prefix matter even more when prefill is minutes.

### 1.6 Pi's session format (from pi's docs — our structural reference)

- JSONL, one entry per line, every entry `{type, id, parentId, timestamp}` → **tree**, not list. Active leaf = current position. Branching in place; `/fork`/`/clone` create new files with `parentSession` pointer.
- Stored under `~/.pi/agent/sessions/--<path-flattened-cwd>--/<timestamp>_<uuid>.jsonl` — **sessions are keyed by working directory**.
- Entry types: `message` (AgentMessage union: user / assistant / toolResult / bashExecution / custom / branchSummary / compactionSummary), `model_change`, `thinking_level_change`, `compaction` (summary + `retainedTail` checkpoint + `usage` + `details{readFiles,modifiedFiles}`), `branch_summary` (summary of the abandoned path attached at the new position), `custom` (extension state, NOT in LLM context), `custom_message` (extension-injected, IN context), `label` (bookmarks), `session_info` (name).
- `buildContextEntries()` walks leaf→root honoring compaction; `buildSessionContext()` derives messages + current model + thinking level from the path. Versioned header (v1 linear → v3) with auto-migration.
- SessionManager API: create/open/continueRecent/forkFrom; append*; tree navigation (branch, branchWithSummary, getTree...).
- Usage recorded per assistant message incl. `cacheRead`/`cacheWrite` and cost — cache stats are first-class session data.

---

## 2. Prime agent (PrimeIntellect) — RLM harness on a pi fork

> Started as a hard fork of pi-mono; now independent. TypeScript host + persistent IPython kernel. MIT. ~1.4k stars at time of research.

### 2.1 Topology: client / daemon / worker / kernel

- **Client** (TUI or headless JSON/RPC) owns rendering only — not execution.
- **Daemon supervisor** owns discovery, routing, attachments, worker health, and **cross-agent message delivery**.
- **Session worker** (one per root session tree) owns `AgentSessionRuntime` + scheduler + kernels + all descendants.
- **AgentSession** owns provider calls, queues, tools, compaction, goals, child lifecycles, transcript writes.
- **Python kernel** is the *model-facing control environment*: the model's single built-in tool is `ipython`; typed host requests bridge back for authoritative state (goals, messages, compaction) — credentials/provider execution stay in the host.
- Workers/kernels are separate processes for *lifecycle & failure containment*, explicitly **not** security sandboxes.

### 2.2 RLM model (the interesting bet)

- Everything programmatic: file ops, shell (`bash()` with live handles), skills, subagents, context management all happen as Python in the persistent kernel. State (variables, imports, parsed results, task handles) **survives tool calls and compaction** — this is their answer to "context as variables": the kernel holds working state so the model's context stays small.
- Background commands: unawaited handles produce a "Background command finished" notice delivered as steering at the next safe turn boundary; reading the handle withdraws the notice (careful semantics around what counts as a read).
- `rlm.spawn(name=...)` returns an admission handle (`rlm_child_id`, `session_dir`, `model`) immediately — never the answer. Results arrive only via explicit `agent_message` replies or files. Child registry survives compaction, kernel restart, parent restoration; completed children stay addressable while parent is open; `rlm.delete_subagent` when done.
- Skills = Agent-Skills markdown + Python packages installed into the kernel, callable as typed functions; only metadata in the startup prompt.

### 2.3 Agent-to-agent communication (the reference design)

- Surfaces: CLI (`prime-agent send <agent> "..."`), kernel skills (`agent_observe.list_agents()`, `agent_message.send(...)`), parent-scoped child registry.
- Delivery modes: **auto** (steer busy / deliver to idle), **steer** (inject into active work), **follow_up** (wait for current work to finish).
- Receipts: `delivered` (reached an idle target's context) vs `queued` (accepted for later) — no pretending a message was consumed.
- `send("all", ...)` broadcasts within the **family roster**; the daemon derives sender identity and enforces message-size, rate, and pending-queue limits.
- Usage attribution: child usage attributed to parent session while distinguishable in reporting.

### 2.4 Long-running machinery

- Detach/reattach: closing the terminal detaches the client; worker keeps queue, schedules, kernel, descendants. Recoverable after worker/supervisor restart from JSONL + artifacts.
- Three scheduling surfaces: user `/heartbeat` (one visible recurring instruction), agent `rlm_heartbeat` (programmatic, multiple, can't touch the user's), `prime-agent schedule` (cron/one-shot at the daemon). Due ticks claimed before delivery (crash-safe), missed ticks coalesced.
- **Persistent goals**: `/goal` stores objective + progress (token/time/continuation counters, optional budget); harness keeps prompting after ordinary turns; only `goal.complete()` marks success.
- **Autonomous mode**: bounded continuation policy — gates (e.g. `npm run check`), budgets (continuations/turns/tokens/wall-clock), and the subtle bit: while subagents run, timer continuations are *held* (children's replies are the real wake-ups; a keep-alive valve fires only after ~25 min of continuous child activity so the parent can unblock hung children).

### 2.5 Compaction

- Same shape as pi's (summarize older, retain recent) **plus**: kernel state persists through compaction; child registries and artifacts survive; compaction never stops goals/heartbeats/schedules/children; agent can run `compact.run("preserve X")` programmatically with focus instructions.

---

## 3. Grok Build (xAI) — ACP-first Rust platform

> Open-sourced 2026-07 (reported). Rust monorepo, ~70 crates. Third-party architecture analysis (visionik) + public tree. "Copy of Claude Code" is the right product class, but the architecture is deliberately different: **the official TUI is itself a client on a public wire.**

### 3.1 The wire: Agent Client Protocol (ACP)

- ACP = JSON-RPC 2.0 between a **client** (UI holding "the remote") and an **agent** (process holding sessions, model calls, tools). Like LSP for coding agents; **not** the model API, **not** MCP (MCP = tools *into* agents; ACP = who holds the remote *over* an agent).
- Because the TUI is an ACP client, every surface — headless, IDE, WebSocket, multi-window — reuses the same session/prompt/update/permission shapes. Permissions are **protocol-visible events** any client can render, not TUI modals.
- MCP-over-ACP: MCP tools living in the client process are reached by passing MCP messages back over the ACP wire (v1 half-duplex).

### 3.2 Process topology

- Default: one process owns UI + agent for that window (private).
- **Leader mode** (opt-in): long-lived leader process over a Unix socket under `~/.grok/`; clients attach/detach; leader holds sessions, model calls, tools, MCP liveness, in-flight turns. UI crash ≠ agent death. Leader replaceable by newer binary; idle auto-update restarts. One model-session world + one MCP liveness set instead of N.
- Sessions are **actors**: ACP commands/events feed a session actor; a prompt runs handle_prompt → maybe compact → sample → dispatch tools (with permissions) → stream `session/update`. Subagents and monitors run in parallel lanes without blocking the TUI loop; the session serializes the main turn.

### 3.3 Crates that matter (granularity worth copying)

`xai-grok-pager` (TUI + composition root), `xai-grok-shell` (agent runtime: ACP agent, sessions, turn loop, leader), `xai-grok-tools`, `xai-grok-workspace` (filesystem/VCS abstraction), `xai-grok-sampler` (concurrent sampling, cancel, retry, stream parsing — isolated concern), `xai-grok-agent` (system prompt assembly from MiniJinja template + runtime context), `xai-grok-sandbox` + `nono` (Landlock/Seatbelt profiles), `xai-fast-worktree` (fast git worktrees for parallel subagents, copy-on-write/overlay where the OS allows), `xai-grok-compaction`, `xai-acp-lib`, plus a PTY harness for keypress-level e2e tests.

### 3.4 Compaction: full-replace successor brief

- Auto near ~85% context or `/compact`. Same chat model, **tools off**, structured prompt.
- Produces a **nine-section brief**: 1 primary request/intent, 2 key technical concepts, 3 files & code sections (full snippets of recent edits), 4 errors & fixes, 5 problem solving, 6 **all user messages verbatim**, 7 pending tasks (never invented), 8 current work (resume mid-stream), 9 optional next step with a **verbatim quote** of where work left off (anti-drift).
- Prior compaction summaries are carried forward as authoritative for early history (nothing leaks out across successive compactions).
- After heavy compaction the system side can shrink to a stub — the summary carries mission state.

### 3.5 Safety & helpers

- Action-safety doctrine in the system prompt: reversible-local = free; irreversible/shared = confirm; **one approval is not a blank check**; investigate unfamiliar state before overwriting (directly addresses "clashing with another feature").
- Folder trust, permission popups, yolo, plan mode, OS sandbox profiles (off by default), hooks (allow/deny/transform), subagent **worktrees** with isolated checkouts, monitors (stream stdout lines as chat notifications: CI tails, log watchers), goals, schedulers.

---

## 4. Codex (OpenAI) — the Rust reference for protocol discipline

> codex-rs workspace. Same core engine behind TUI, headless exec, IDE app-server, cloud.

### 4.1 Submission/event queue (the core pattern)

- Clients submit operations (with op ID, metadata, trace context) into an async queue; the core processes them and streams events back. Enables cancellation, interruption, streaming UX, human-in-the-loop uniformly across all surfaces.
- `Codex` orchestrates active **threads** (conversation sessions), each running turn tasks: spawn turn → TurnContext → model client streams responses → parsed into events → tool dispatch (built-in or MCP) → turn completion events.
- Session state + **SQLite-backed store** for durability; rollout (JSONL) persistence & replay; history compaction (`tasks/compact.rs`); multi-agent threads incl. a "Guardian" safety sub-agent; models manager, config layers/profiles; hooks system.

### 4.2 Execution modes share the core

TUI (in-process channels), `codex exec` (headless, terminal event processing), app-server (JSON-RPC 2.0 over stdio/unix for IDEs), and **Codex-as-MCP-server** (Codex itself exposed as a tool for other agents). One runtime, four faces.

### 4.3 Known pain (from field reports)

- Session resume historically the most-requested & least-reliable feature: task drift after rate-limit recovery, duplicate file updates on fork, lost env context on rollback. Lesson: **resume must reconstruct *world* state (files, env, running processes), not just the transcript** — transcript replay alone is insufficient.
- Multi-agent v2 message encryption (provider-sealed) makes its own rollout non-auditable (see 1.3) — a cautionary tale for comms design.

---

## 5. CodeGraff (justrach/codegraff) — a harness in Zig

> `justrach/codegraff` — "graff": agentic coding harness in Zig 0.17-dev. One 3.7 MB binary, zero deps, ~1.8 ms cold start, ~12 MB peak RSS per full agentic turn, +0.4 MB per parallel subagent (their own measurements). Terminal TUI + Electron desktop + iOS on the same core; ACP spawn (`graff acp` — Zed External Agents); MCP server mode (`graff mcp serve`); `graff serve` for remote; TS/Python SDKs generated from `graff --schema`. Modified AGPL-3.0, two authors (Rach Pradhan, Yu Xi Lim). The in-repo `architecture.md` is the primary source for this section — unusually candid and granular.

### 5.1 The Agent struct is the whole trick

- One `Agent` type is the unit of execution: owns message history, provider, arena, and the loop methods. **The root REPL agent and every subagent are the same type** — a subagent is just `Agent{sub=true}` with a fresh arena, empty history, and muted output. Runs to completion; final text returned verbatim as the tool result.
- Subagent depth capped at **one level**: children get the base tool set only (no `subagent`, no MCP, no meta tools) — no nested fan-out *by construction*. Children share nothing with the parent context; the orchestrator must put everything needed into the prompt.
- Tools in three kinds: **built-in** (bash, read/write/edit, subagent, workflow) on the thread pool; **meta** (`todo_write/read`, `attempt_completion`, `ask_user`) handled inline by the orchestrator — the distinction: a meta tool acts on the agent/conversation, an external tool touches the world; **MCP** (namespaced `mcp__<server>__<tool>`, one mutex per server since a stdio server is one pipe).
- `workflow` = workflows-as-data: sequential phases of parallel subagents, `{{prev}}` substitution carrying results between phases ("the pi-dynamic-workflows idea without a JS sandbox").

### 5.2 Providers

- Two decoupled axes: **wire format** (`anthropic` | `openai` | `responses`) × **auth style** (`x_api_key` | `bearer`); a `provider_specs` table is the single source of truth (~16 providers, incl. Codex via ChatGPT login on the Responses API and Claude Code subscription OAuth as provider slots).
- Normalized internal `ToolCall {id, name, input}`; all provider differences isolated to `buildBody` + the `step*` parsers. SSE streaming on all formats, but events are **reassembled into the exact non-streaming response shape** — history echo, compaction, and save/resume stay transport-agnostic.
- Compaction threshold is **per-provider-per-model**: `compactAt()` = 80% of that model's window.
- Soft-retry idiom for provider quirks (forced `tool_choice` conflicts with thinking; `max_tokens` vs `max_completion_tokens`; `stream_options` rejection): detect the rejection, flip a flag, retry once.
- Sessions store history in the provider's **native wire shape**; switching providers clears history — the deliberate opposite of pi's provider-neutral format (buys zero translation cost, loses portability).

### 5.3 Permission gate — the "structural invariant" school

- One shared `Approvals` (mutex + approved keys + yolo flag). Root: unapproved state-changing call prompts (yes-once / always / no); **a denial becomes the tool result** so the model adapts.
- **Metacharacter rule**: commands containing `;|&><$`, backticks, or control whitespace never prefix-match an approval — chaining (incl. `sh -c "ls\nrm"`) can smuggle a second command. Approving interpreters (`python3`, `node`, …) prints an arbitrary-code-execution warning. `find` is excluded from the read-only seed list because `-exec`/`-delete` make it an exec tool.
- **Path confinement** (`confinedPath`): file tools reject absolute paths and any `..` — confined to the cwd subtree, enforced for root *and* subagents, and **not bypassed by `/yolo`**: "a structural invariant, not a trust toggle."
- **bash cwd-lock**: a seed/approved command auto-runs only if every path argument stays inside cwd (`cat /etc/passwd` prompts at root, denied in subagents).
- Subagents gated structurally: bash allowlist-only, writes path-confined, MCP not exposed at all.

### 5.4 KV-cache discipline — "Manus lessons", matching Earendil rule-for-rule

- Frozen system prompt (no per-request timestamps), strictly append-only history, **comptime tool-list rendering** → byte-stable tool order.
- Explicit `cache_control` breakpoint only where the API needs it (direct Anthropic); automatic prefix caching trusted elsewhere.
- `recordUsage` normalizes the cache-read field across three provider spellings (`cache_read_input_tokens` / `prompt_cache_hit_tokens` / `prompt_tokens_details.cached_tokens`) into the trace.
- Project instructions (first of `AGENTS.md`/`HARNESS.md`/`CLAUDE.md`) built into two arena-owned strings **once at startup** — byte-stable, zero per-request cost.
- MCP schema sanitization (`oneOf` → `anyOf` for OpenAI's validator) runs **once at discovery**, so the rendered tool JSON stays cache-stable.

### 5.5 Hooks — choke point + fail-open

- `.hasp/settings.json` hooks: `pre_tool` (**exit 2 blocks the call**, and the hook's stderr is returned to the model as the error result — it learns *why*), `post_tool` (sequential — a formatter finishes before the next tool runs), `turn_end`.
- Both tool hooks live in `execTool`, the single function every external tool call flows through — root, subagents, and MCP alike. "A subagent's bash call hits the same gate."
- **Fail-open by construction**: hook timeout or spawn failure = allow. "Hooks are user automation, not the security model — the permission gate stays the enforcement layer. Anything that must fail closed belongs in the gate, not a hook."
- No hot reload: the hook list is part of the session's trust surface, same reasoning as the startup-frozen prompt.

### 5.6 Context: handles + paging (directly relevant to your goal)

- Large tool outputs become a **4 KB handle** whatever their size; the model pages with `read_tool_result`. Context philosophy in one line: "reuse stable setup, run small programs over working data, carry useful results forward."
- This is the complementary answer to the dense-payload problem (§8 adjacent, colbymchenry): never let fat payloads *into* context — keep them addressable outside it.

### 5.7 Observability & self-debugging

- Per-run JSONL trace (API latency, bytes, context tokens, error flag per call; tool timings) through one mutex, flushed per line — always valid mid-session. **The system prompt points the agent at its own trace file**, so the agent self-debugs with `read_file`.
- A second BehaviorTrace stream with bounded, allowlisted, content-free uploads (explicitly no prompt text, outputs, paths, or hidden reasoning in projections).

### 5.8 The rest (noted, adopt selectively)

- **Strict mode** — "every message is a tool": forced `tool_choice` + `ask_user`/`attempt_completion` meta tools (the Cline/Roo pattern) makes both human↔agent directions flow through the tool channel. `ultracode` codeword: pure prompt-augmentation steering toward workflow decomposition.
- **DGM local learning engine**: content-addressed store, typed mutation/evaluation protocols, sign tests + Bonferroni + holdout gates, append-only activation transactions — self-improvement with unusual statistical rigor (Prime's `/refine`, but with teeth).
- **Eval rig, three layers**: live gated PRs (n=3, pass ≥2); in-house deterministic suites with *hidden checks injected after the harness exits*; FrontierHarness/Terminal-Bench in Docker. "Honest list$": cost recomputed from tokens at published list rates because "a flat-rate subscription prints $0.0000 and that is a plan, not a price."
- Engineering discipline: 600-line file ceiling, test-count-never-shrinks, SDK drift checks, PTY probes, ADRs. Their live board: graff 12/12 at $21.48 vs Pi 12/12 at $18.47, grok-build 11/12 — close enough to pi on cost that the footprint argument (12 MB vs 155 MB–1.0 GB RSS) does the talking.

### 5.9 Steering & retained subagents (`docs/subagent-steering-design.md` — design, not yet built)

- The merge of "addressable children" and "resumable workers" into one mechanism: **identity** is harness-assigned `sa-<ordinal>-<hash>` (stable across runs); model-chosen names are aliases; sender identity is never model-supplied.
- **Retention is opt-in per spawn** (`retained: true`): resolves the disposable-worker vs steerable-worker collision — "retention is a choice with a price, made at spawn time when the parent knows whether a follow-up is likely." Only retained workers persist transcripts (via the append-only session-transcript mechanism).
- **One delivery primitive with a `trigger` flag** (beats Codex's two-tool `send_message`/`followup_task` split): running child → queued into next turn (trigger irrelevant); idle retained → `trigger:true` rehydrates from transcript + registry and runs a turn; completed unretained → actionable error.
- **Queued-and-never-awaited is the hard invariant**: mutual sends between two busy agents must not deadlock (composes with their admission-counter design that deliberately avoids holding a slot across a child's lifetime).
- Containment: family-scoped addressing only; retained children still can't spawn or steer (no cycles); per-message size limits + per-(sender,recipient) rate buckets; resumed workers draw from the same budget ledger as the original spawn.
- Honest self-correction in the doc: their own issue #392 wrongly assumed worker transcripts already persist — only final-report markdown cards do. History persistence is the first slice, not plumbing.

### 5.10 Hyperagents: DGM/MAP-Elites on harness primitives (`docs/hyperagents.md`)

- **Per-child `system_prompt` overrides** make swarms prompt-heterogeneous — and are KV-cache-cheap (each subagent is a fresh context); mutating the *root* prompt mid-loop is the expensive operation (full-prefix invalidation).
- Trajectory log = DGM archive tree: one JSONL node per agent run (`turn`/`subagent` kinds, parent edges, `prompt_sha` fingerprints); prompt text captured once per session so the archive is self-contained and replayable; scores written back keyed by sha via the SDK.
- The evolution loop itself lives in SDK code, not the harness — the harness only supplies primitives (editable prompts, spawn-with-variant, score channel, archive).
- **Signed fitness channel** (Step 0): HMAC-SHA256 over a canonical tuple (prompt_sha, parent_sha, score, run_id, judge_id, artifact_sha, eval_set_hash, niche, provider_class) — because a forged score row appended via `write_file`/`echo >>` manufactures fitness *and* lineage; the key lives outside cwd (confinement guards escape, but the archive lives inside); verification fail-closed when a key is set. Step 1: grounded replay judge with deterministic out-of-process assertions instead of self-report grading. The most rigorous self-improvement integrity design in any harness we studied.
- MAP-Elites: niches = named personas as markdown files in `.hasp/agents/<niche>.md` (frontmatter = execution policy, body = the genome); two-tier promotion — prompts auto, code-bearing memory designs gated.

### 5.11 Memory as a first-class layer (`docs/memory-architecture.md`)

- **Two memories, never conflated**: task/experience memory (episodes, `.hasp/memory/`) vs evolutionary memory (genomes + fitness, trajectories). 
- The structural invariant: **retrieved memory enters as append-only content (a tool result), never by editing the cached prefix**; model-*pulled* recall (`recall` tool) over framework-pushed injection; pushed injection allowed only for the cheapest scratchpad design, only after the cached prefix.
- `MemoryDesign` interface (retrieve/update/eval vtable), fingerprinted (`memory_sha`) as a genome slot; built-ins: null / scratchpad / episodic-JSONL / structured / engram (semantic, via MCP) / **summary-tree — compaction output *is* memory**: each compaction summary becomes a retrievable node, so compaction stops being a lossy reset and becomes a hierarchical episodic store. Compaction and `/save` become memory writes.
- Scopes: session / project (default — survives save/resume, path-confined, local) / user / fleet. Fleet-transmitted artifacts are only Designs + signed scores, never stored task content.

---

## 6. Aider (secondary) — cache-friendly history ordering

- `--cache-prompts` orders chat history to maximize the cacheable prefix: system prompt → read-only files (`--read`) → repo map → editable chat files.
- `--cache-keepalive-pings N`: background pings every 5 min for N×5 min after each message to defeat the TTL.
- Lesson: the *ordering* of static-vs-volatile context is a cache decision — put stable stuff first, volatile stuff last.

---

## 7. Adjacent mechanisms worth knowing

- **colbymchenry/codegraph** (unrelated same-name repo, kept for one lesson): pre-indexed code-graph + auto-sync watcher as an MCP server; their own honest caveat — dense verbatim payloads leave **~80% more residual context occupancy** at session end than grep-and-read churn (67k vs 18k tokens on VS Code). Retrieval efficiency and resident footprint are different axes; fat payloads need handles or eviction. CodeGraff's `read_tool_result` handles are the complementary fix.

- **oMLX** (HN): SSD-persisted KV blocks + prefix sharing with copy-on-write for local models — makes agent prefix-churn survivable locally. Relevant only if we target local inference.
- **thaw** (HN): snapshot a *live* inference session (weights refs, KV, scheduler, prefix-hash table) and hydrate N diverging children — "git branch for a running model." Far-end optimization; notes where the industry is heading.
- **pi's subagents package** (observed first-hand in this environment): scripted workflows, managed git worktrees per child (`worktree:true` requires clean source), acceptance/evidence policies, `bg_wait` wake semantics, supervisor channel for child→parent requests. Confirms the pattern: subagent infra is *policy + isolation + wake-ups*, not model magic.
