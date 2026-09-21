# P5 hardening — DeepSeek provider + the bugs real models exposed

Follow-up to `plan-p5.md`. Using a real provider (DeepSeek) for the first time
found five defects that the mock could not: the mock only ever emitted one tool
call per turn, never asked for approval, and never thought. 2026-09-20.

## DeepSeek support (`--provider deepseek`)

- `harness-provider/src/deepseek.rs`: `https://api.deepseek.com/anthropic` (their
  Anthropic-compatible endpoint), default model `deepseek-v4-pro`.
- `harness login deepseek` / `DEEPSEEK_API_KEY` / `deepseek_api_key` in
  `credentials.json`; `doctor` lists all three providers and where each key came
  from.
- Models **`deepseek-v4-pro`** and **`deepseek-flash`** (verified against
  `GET /models`, not docs): 1M context, 384K max output, thinking on by default,
  tool calls supported. Peak/off-peak prices are in the fallback catalog at the
  **peak** rates so the displayed cost is never below what was billed.
- Thinking blocks stream as `thinking_delta`/`signature_delta`; the SSE parser
  already mapped unknown block types to `BlockAcc::Other`, so they are ignored,
  and `ContentBlock::Thinking` was already dropped from replayed transcripts —
  which is exactly what both providers expect.

## Bugs found, in order of severity

1. **Any tool approval deadlocked the daemon** (daemon.rs `WireGate::ask`). The
   `Gate` trait is synchronous, so the gate blocked a runtime worker on a
   `std::sync::mpsc` recv while waiting for the client. The wake-up for the
   connection-loop task — which is what *sends* the permission request — landed
   in that same worker's non-stealable LIFO slot, so the request was never sent
   and the daemon stopped answering anything, including `shutdown`. Fixed with
   `tokio::task::block_in_place`, which hands the worker's queued tasks (LIFO
   slot included) to another worker. Same fix applied to `gate.rs::ask_via` for
   the embedded TUI. Covered by `approvals_round_trip_over_the_wire`, which was
   verified to fail before the fix (20s hang → panic) and pass after.
2. **Parallel tool calls broke the transcript** (anthropic.rs). Real models emit
   several `tool_use` blocks in one assistant turn; the engine journaled one
   `ToolResult` message each, but the API requires *all* results in the single
   message that follows, and rejected every later request with a 400. Fixed in
   the serializer: consecutive tool results merge into one user message. Unit
   tests pin the merged shape, the thinking-only case, and the single-result
   path. (Claude enforces the same rule — this was latent for Anthropic too.)
3. **The daemon ignored the selected provider's default model.** `open_session`
   had its own hardcoded `claude-sonnet-4-5` fallback while `provider_factory`
   had a per-provider one, so `HARNESS_PROVIDER=deepseek harness` ran a Claude
   model name against DeepSeek. Removed the duplication: one
   `harness_provider::default_model()` plus `anthropic_compatible()` used by both
   the CLI and the daemon, and the TUI now always passes its resolved provider
   **and** model to the auto-spawned daemon instead of only when `--model` was
   given.
4. **A run killed mid-tool bricked its session permanently.** The last assistant
   turn kept `tool_use` blocks with no results, so every subsequent request to
   that session failed with the same 400 as (2). `SessionManager::open` now
   closes dangling turns with an explicit interrupted `tool_result`, journaled
   like any other outcome (idempotent; counted in `repaired_tool_calls`, surfaced
   to the user as a note). Tests in `harness-core/tests/session_repair.rs`.
5. **The context window was hardcoded to 200k**, so DeepSeek's 1M context was
   under-reported 5× and compaction fired far too early. The window is now
   resolved per model from the catalog (`context_window_for`).

Also: `Anthropic::name()` returned `"anthropic"` for z.ai and DeepSeek endpoints,
and the three model-switch paths journaled a literal `"anthropic"` — provider
labels in transcripts, traces and the model-change journal now name the service
actually in use.

## Test infrastructure

`HARNESS_MOCK_SCRIPT` gives the echo provider a scripted mode
(`[{"tool": "write", "input": {...}}, {"text": "done"}]`), which is what lets
daemon/PTY tests exercise real tool calls and therefore real approvals.

## Verified against the live API

- Multi-tool turn: 3 samples, 2 tool calls, file written, `$0.003`,
  `cache R 2944` (cache breakpoints honored).
- Goal loop: model explored, said it was blocked; `post_turn_check` fired
  *"continuing toward goal (turn 1)"*; the next turn wrote the file, verified it
  with `bash`, and called `goal_complete` → status `(completed)`.
- TUI over the daemon: two approvals asked and answered (`[y]`), write + bash
  executed, file on disk, footer `deepseek-v4-pro … ctx 1k/1000k … $0.006`.

**State: 55 tests green, `cargo fmt --all --check` clean, zero build warnings,
release build ok, LSP clean on every changed file.**

## Still open

- `deepseek-reasoner`-style models reject `tool_choice`; not used here.
- Peak/off-peak pricing is displayed at peak (conservative, never under-reports).
- Compaction, worktrees and the agent bus have not been exercised against a real
  provider yet — only the single-session turn/goal paths have.
