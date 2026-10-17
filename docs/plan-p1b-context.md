# P1b Plan — Context Robustness (compactor, branch/fork, worktrees)

> **Status (2026-09-19): COMPLETE.** 29 tests green (`cargo test --workspace`), fmt clean, CLI builds. Compactor verified end-to-end against the mock provider (brief + tail + est shrink; auto-compact triggers over threshold); prune transform, fork/branch-points, worktree create/resolve/remove, and dirty-state counting unit-tested. TUI/plain commands ship together.

Implements `research/03-architecture.md` §12 **P1** remainder (context half). Scope agreed 2026-09-19.

## Scope

**In:**
- **Compactor** (`hasp-core/src/compaction.rs`): standalone summarization request (own system prompt, no tools, mute sink) producing the 9-section successor brief (Grok Build's template: intent / concepts / files / errors / problem-solving / **all user messages verbatim** / pending / current work / next step w/ verbatim quote); tail retained by token budget (~20k est); persisted as a `compaction` entry with `retained_tail` (the P0 checkpoint format). Triggers: manual `/compact` + auto after turns when est > 85% of the model's window; trace records the event; booked as reset, never miss.
- **Timed pruning** (Claude Code's time-based microcompact, request-level transform — the session log stays append-only): when the idle gap since the last assistant message exceeds 60 min (cache certainly dead), clear tool-result content older than the most recent 5 results of compactable tools (`read`/`write`/`edit`/`bash`) to `[Old tool result content cleared]` before the request — shrinking the inevitable rewrite, never forcing a miss that wouldn't have happened.
- **Branch/fork**: `/branch` lists branch points (user messages + compactions with ids); `/branch <id8>` moves the leaf (existing tree mechanics); `/fork` clones the session file (new id, `parent_session` pointer) for before-risky-work snapshots.
- **Session-level worktree mode**: `/worktree [name]` creates `.hasp/worktrees/<name>` on branch `harness/<name>` from HEAD (git worktree), rebinds engine workspace + starts a fresh session there; `/worktree-exit [remove]` returns to the original root (optionally removing the worktree).
- **World-state check on resume**: session header gains `dirty_state` (modified/untracked counts from `git status --porcelain`); `open_latest` warns on drift (branch/head already warned in P0).
- **Process-group kill**: bash children get their own process group (`process_group(0)`); timeout kills the whole group (`kill -TERM -<pid>`) before falling back to child kill.

**Out:** mid-turn overflow recovery (retry-after-compact), parallel tool fan-out (P3), subagent worktrees (P3), tiktoken (chars/4 + catalog windows suffice).

## Verification

- Unit: prune transform semantics (keep-recent, compactable set, non-compactable untouched); branch_points; clone/fork roundtrip; worktree create/resolve/remove in a temp repo; dirty_state counts.
- Integration (mock provider): compaction end-to-end (context becomes brief + tail, est shrinks); auto-compact triggers with a small window.
- `cargo test --workspace` green + fmt clean.

## Notes

- TUI commands that need the provider (`/compact`) run as spawned jobs (like turns) with `Done` events carrying a `DoneKind::{Turn,Compact}`; sync commands stay inline.
- Worktree switching swaps `engine.workspace` + `engine.session` under the engine lock; the TUI keeps a stack of `(Workspace, name)` to exit back.
