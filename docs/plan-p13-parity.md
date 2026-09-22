# P13 — agent-quality parity

Status: landed 2026-09-23. 167 tests (was 150); fmt and `clippy -D warnings`
clean. Verified live on DeepSeek V4 Pro (no Anthropic key on this machine; see
"Not verified" below).

## Found in review, fixed

| problem | effect before | fix |
| --- | --- | --- |
| no cache breakpoint on the conversation | every sample re-billed the whole history at full input price | breakpoints on the last message and `len-3` (previous tail, beats the 20-block lookback) — 4 total |
| thinking blocks dropped on replay | history edit every sample: cache invalidation, and a 400 on models enforcing preserved thinking (Opus 5.5, Fable 5.1) | signature + `redacted_thinking` captured and replayed to the producing model only |
| no thinking/effort for Claude | current Claude models ran at API defaults (Opus 5.5 defaults to `medium`) | adaptive thinking (summarized) + `output_config.effort`, `xhigh` default where supported, per-request from `req.model` |
| `/model` kept the old model's settings | Opus → Haiku mid-session would send adaptive thinking to Haiku (400); output ceiling stayed fixed | reasoning computed per request; `max_tokens` re-derived on every switch |
| 600 s *total* reqwest timeout | a long healthy stream (64k tokens) cut off mid-turn | connect + per-read timeouts; the stall guard handles silence |
| fixed 16k output ceiling | large `write` inputs truncated → tool ran on `{}` | per-model ceiling up to 64k; unparseable input is flagged, refused with a precise error, and the turn continues |
| `refusal` / context-exceeded stop reasons mapped to "stop" | silent empty turns | mapped, with a visible note |
| stale default model and catalog (200k windows) | compaction ~5× too eager on 1M-context models | `claude-opus-5` default; bundled current models, windows and prices, merged under the fetched catalog |
| `bash` read output with `read_to_string` | any non-UTF-8 byte → empty output | lossy bytes; head + tail; partial output on timeout; process-group kill on cancel |
| tools ran strictly serially, cancel waited for tools | parallel read batches serialized; `esc` could not stop a hung command | concurrency-safe batches via `join_all`; cancel interrupts the call in flight and answers every `tool_use` |

## Added

- `grep`, `glob` (ripgrep's `ignore`/`globset`, in process), `todo`, `skill`.
- Prompt templates, skills and hooks (`crates/harness-core/src/{extensions,hooks}.rs`),
  `.claude/` layout compatible; `harness trust` gates project hooks.
- `HARNESS_SANDBOX` (`crates/harness-tools/src/sandbox.rs`): Seatbelt / bwrap,
  refuses rather than degrade; the real Seatbelt test runs on macOS.
- System prompt: tool-use guidance (search tools, parallel calls, todo). Static
  text, so byte-stability holds.

## Live evidence (DeepSeek V4 Pro, release build)

- 3-sample search task: model batched `grep, grep, glob` into one sample;
  cache reads 0 → 2432 → 2944 tokens; signed thinking replayed without error.
- `-p "/fix calc.py"` with `HARNESS_SANDBOX=workspace-write`: template expanded,
  model loaded the matching skill and followed it, fixed the bug, verified by
  running Python in the sandbox. 4 samples, 9216/12465 input from cache, $0.006.

## Not verified here

- The first-party Anthropic request shape (adaptive thinking, `display`,
  `output_config.effort`) is unit-tested against the documented API but was
  not sent live — this machine has no Anthropic key.
- No agentic benchmark (SWE-bench Verified / Terminal-Bench) has been run; the
  harness-quality claims above are from targeted tests and two live runs.

## Still open

- Engine lock held across provider I/O (P12 tranche 2).
- Image input (user messages and `read` of images) — `AgentMessage::User` is
  text-only; needs a content-block user message.
- Background processes for `bash` (dev servers) with output paging.
- Project `.harness/mcp.json` starts servers without `harness trust` — the same
  code-execution concern hooks now guard against. Gating it changes existing
  behavior, so it is left for a deliberate decision.
- Strict ACP framing + TCP transport (P12 3b); `available_commands_update` could
  now advertise templates.
