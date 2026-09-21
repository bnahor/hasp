# P0 Implementation Plan — Walking Skeleton

> **Status (2026-09-19): COMPLETE.** All milestones M0.1–M0.6 done; `cargo test --workspace` green (22 tests), `cargo fmt --check` clean, CLI smoke-tested (workspace resolve → session create → header format verified; live model path compiled, pending `ANTHROPIC_API_KEY`).
>
> Deviations from the original scope, both simplifications:
> - **4 tools, not 5** — `read_result` was dropped: artifacts live under `<root>/.harness/artifacts/…` so the confined `read` tool already pages them via `offset`/`limit`. Same capability, pi-parity tool count.
> - `Tool::execute` takes `&mut ToolCtx` (tools mutate gate state through it); engine turn tests live in `tests/engine_turn.rs` because the core⇄provider/tools dev-dependency cycle would otherwise build two copies of `harness-core` (`cfg(test)` lib + dep), splitting the `Tool`/`Provider` traits.

Implements `research/03-architecture.md` §12 **P0** with the 2026-09 decisions applied (Rust; daemon/ACP deferred to P2 unchanged; compaction format *reserved* but not automated until P1).

## Scope

**In (P0):**
- Workspace identity resolution (git root/worktree/branch/HEAD; non-repo fallback) + structural path confinement (relative-only, no `..`, symlink-escape check — not bypassable by any mode)
- Session model: append-only JSONL tree (`id`/`parentId`), entry types incl. `compaction` (with `retained_tail` — reserved for P1), context rebuild walk, per-workspace session storage under `~/.harness/sessions/`
- Turn engine: sample → tools → sample loop, tool-result entries, per-sample usage accounting, 85%-context warning (manual `/compact` lands in P1)
- Providers: Anthropic Messages API over rustls with SSE streaming, `cache_control` breakpoints (system + tools), cache read/write usage captured; deterministic `MockProvider` for tests
- Tools (4, name-sorted, stable schemas): `read`, `write`, `edit` (unique-match rule), `bash` (approval gate: seed allowlist + metacharacter rule + cwd-lock); fat tool outputs become out-of-line artifacts paged by `read`
- Fat outputs → out-of-line artifacts (`.harness/artifacts/<run>/`) with a 4 KB inline handle
- Per-run JSONL trace (`.harness/traces/<run>.jsonl`): every API call (latency, tokens, cache R/W) and tool execution
- CLI: `harness` REPL + `harness -p "..."` one-shot; slash commands `/exit /new /name /stats`
- Verification: `cargo test` across all crates (session tree, compaction context rebuild, confinement, gate rules, SSE parsing, request shape, engine turn with mock provider)

**Out (deferred):** daemon + ACP wire (P2), auto-compaction (P1), branch/fork UI, subagents/bus/worktrees (P3), MCP, goals/schedules (P4), ratatui TUI (P1 — P0 ships a line REPL as the first client), cost tables (model catalog P1), OS sandbox (v2).

## Layout

```
Cargo.toml                # workspace
crates/harness-workspace/ # identity, confinement, storage paths
crates/harness-core/      # types + traits (Provider/Tool/Gate), session, context, engine, trace
crates/harness-provider/  # anthropic (rustls, SSE), mock
crates/harness-tools/     # read/write/edit/bash/read_result
crates/harness-cli/       # binary: REPL + one-shot
docs/plan-p0.md           # this file
```

Dependency direction: `provider`/`tools` depend on `core` (traits + types); `core` depends on `workspace`; `cli` depends on all. No cycles: engine consumes `Box<dyn Tool>` / `Box<dyn Provider>` defined in `core`.

## Conventions

- State on disk: sessions in `~/.harness/` (user-global); traces + artifacts in `<root>/.harness/` (workspace-local, gitignore-friendly).
- Structural safety is not a trust toggle: confinement and the metacharacter rule apply regardless of approvals.
- Approval semantics: deny becomes the tool result (the model adapts); "always" approvals are session-scoped, never persisted in P0.
- Token counting is a chars/4 estimate in P0; incremental tokenizer (tiktoken-rs) lands with the model catalog in P1.
- Bash timeout default 120 s; child kill on timeout (process-group kill is a known P1 gap).
- Tool execution is sequential in P0 (parallel fan-out arrives with the bus in P3).

## Milestones

- **M0.1** workspace + session crates green (`cargo test`) — identity, confinement, tree walk, compaction-context rebuild
- **M0.2** provider crate green — SSE parser + request shape tests; mock provider
- **M0.3** tools crate green — confinement, unique-match edit, gate rules (scripted gate)
- **M0.4** engine: full two-sample turn against mock, entries + trace asserted
- **M0.5** CLI REPL compiles and runs end-to-end against mock (`HARNESS_PROVIDER=mock`); live Anthropic path compiled but requiring `ANTHROPIC_API_KEY` (absent on this machine today — export it to smoke-test)
- **M0.6** formatting + final `cargo test` pass; summary to user

## Deferred-risk register (things P0 knowingly simplifies)

| Simplification | Revisit at |
|---|---|
| chars/4 token estimate | P1 model catalog |
| Sequential tool exec | P3 bus |
| Child kill, not group kill | P1 |
| Single provider, hardcoded 200k window | P1 catalog |
| No dynamic system content except workspace root + AGENTS.md (byte-stable per session) | keep |
