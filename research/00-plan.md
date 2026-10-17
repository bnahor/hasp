# Harness Research — Plan

## Goal

Design a personal agent harness that is:
1. **Robust to context** — sessions and project state persist; no re-loading context by hand; no "wrong repo / wrong worktree" failures (the Cursor failure mode).
2. **Lightweight** — pi-class minimalism: small system prompt, tiny core toolset, context discipline.
3. **Multi-agent capable** — first-class inter-agent communication, subagents, background work.
4. **Cache-aware** — prompt-cache discipline is an architectural concern, not an afterthought.

Language decision comes later; preliminary lean is Rust (validated below), with extension points kept protocol-based so language doesn't leak everywhere.

## Sources under study

| # | Source | Identity | Status |
|---|--------|----------|--------|
| 1 | "The harness with the KV-cache blog posts" | **Earendil (pi's author)** — earendil.com series: *Prompt Caching In Agents*, *How Compaction Works in Pi*, *The Session You Cannot Take With You*, *What is a Harness?*, *Pi, Minimal and Performant* | Read in full |
| 2 | Prime agent | `PrimeIntellect-ai/prime-agent` — hard fork of pi-mono; RLM harness (persistent Python kernel); daemon/worker/kernel architecture; direct agent-to-agent messaging | README + architecture/RLM/long-running docs read |
| 3 | Grok agent | **Grok Build** — `xai-org/grok-build`, xAI's open-source (Rust) Claude Code-class agent; ACP-first wire, leader mode, session actors | Third-party deep analysis (visionik) + ACP spec read (public tree) |
| 4 | Codex | `openai/codex` (codex-rs) — Rust workspace; submission/event queue; thread manager; rollout persistence | DeepWiki architecture overview + source references read |
| 5 | CodeGraff | `justrach/codegraff` — Zig 0.17-dev harness, one 3.7 MB binary; Agent-struct unification, one-level subagents, tool-result handles, workflows-as-data, structural permission gate, DGM learning loop, 3-layer eval rig | In-repo `architecture.md` + README read in full |
| 7 | Aider (secondary) | Cache-friendly history ordering, `--cache-prompts`, keepalive pings | Docs read |
| 8 | Adjacent | oMLX (SSD-persisted KV cache), thaw (fork live inference sessions), ACP spec, MCP, pi's own session format docs, colbymchenry/codegraph (dense-payload residency lesson only) | Skimmed for specific mechanisms |

## Research questions

1. What is the invariant core every harness shares (session model, turn loop, provider layer, tool registry)?
2. How does each source manage **context lifetime** — persistence, compaction, portability?
3. How does each source do **inter-agent comms** — topologies, delivery semantics, receipts, auditability?
4. What makes prompt-cache performance a *structural* property?
5. How do harnesses bind a session to a **workspace/worktree** and prevent cross-project contamination?
6. What's the right process topology (in-process vs daemon vs multi-client)?

## Outputs

- `01-source-notes.md` — per-source findings with receipts
- `02-patterns.md` — cross-cutting synthesis; answers to the questions above
- `03-architecture.md` — granular proposed architecture (components, data formats, interfaces, topology, build phases)
- `04-performance.md` — performance analysis: DPDK verdict, ranked levers, 2026-verified Rust crate map, kernel options

## Decisions log

- 2026-09: **Language: Rust** (over Zig — ecosystem breadth; see `04-performance.md`). **Daemon: default-on** (Prime-style; `--no-daemon` escape hatch). **Persistent kernel: deferred to investigation** (`04-performance.md` §6).

## Caveats

- Databricks/Shopify benchmark claims come from Earendil's own blog (interested party); CodeGraph numbers are its own README benchmarks (vendor). Directionally useful, treat magnitudes as soft.
- Grok Build details come from a third-party analysis of the public tree + the tree itself, not from running it.
- The Grok Build "silent repo upload" controversy is noted only as due diligence; irrelevant to architecture.
- CodeGraff performance figures (3.7 MB binary / 12 MB RSS / eval boards) are the project's own self-measurements; methodology receipts are published, but they remain vendor-run.
