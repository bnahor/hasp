# P11 — closing the audit gaps, in order

Status: COMPLETE (2026-09-20) — all eight items

Item 1 verified three ways: unit + fake-endpoint e2e tests (fragmented tool
deltas, usage, the 400-on-`stream_options` retry), live discovery against
DeepSeek's OpenAI endpoint (`harness models compat` → the two models it serves),
and a live turn through the real harness (`--provider compat --model
deepseek-v4-pro` → "OK", $0.002, usage reported).

The audit's prioritized gaps, executed top-down, with one amendment from the
user: **model extensibility must be automatic** — no new model or vendor should
require editing code.

## 1. Model extensibility (folded into the top of the list)

Three layers:

- **Ollama provider** — local models, and the model list is *discovered at
  runtime* from `/api/tags`, so `ollama pull anything` shows up with no code
  change. This is the proof of the principle.
- **Generic Anthropic-compatible provider** — `HARNESS_COMPAT_URL` (plus key)
  turns any new vendor into `--provider compat` without a recompile.
- **Catalog auto-refresh** — the models.dev catalog is fetched on demand when
  stale instead of only via `harness update --models`, and `--model` accepts any
  id the catalog lists for the active provider.

## 2. Self-update — done

`git init` + initial commit (427d49c), then `harness update`: rebuilds and
reinstalls from the checkout it runs in (walking up from the executable, so
`target/debug` and `target/release` both resolve), remembers that checkout in
`~/.harness/checkout`, and an installed binary with no checkout beside it
updates from the remembered path. `harness update --models` unchanged. Verified
live in all three positions: dev binary, PATH binary with remembered checkout,
and a stray copy with neither (clean error naming the cargo command).

## 3. Session export/import

`harness export [session-id] > file` and `harness import file` over the
append-only JSONL that already *is* the format — no new serialization, just a
command surface (plus what opencode charges for and we get free: portability).

## 4. Themes / light mode

A palette struct behind the TUI's color choices, `HARNESS_THEME=light|dark` and
`/theme`, with a light palette that keeps the markdown/highlight hierarchy.

## 3–6 — done

- **export/import**: `harness export [id]` streams the journal, `harness import
  <file>` validates every line, files the session under the workspace its header
  names (path-derived key when that workspace only exists on another machine),
  and refuses to overwrite. Covered by a round-trip test across two HOME states.
- **themes**: one `Theme` struct feeds every content color (transcript kinds,
  markdown, syntax, diffs); `/theme light|dark` switches at runtime and
  `HARNESS_THEME` sets the default. Dark is byte-identical to the old look.
- **web_fetch**: read-only research behind structural egress rules — http(s)
  only, loopback/private/link-local/metadata names and IPs refused outright
  (verified live: example.com fetched in 57 ms, `169.254.169.254` refused in
  0 ms), body capped, big pages paged through the artifact store, HTML reduced
  to readable text. Hostname-rebinding is a recorded limit; `bash` remains the
  approval-gated egress path.
- **symbol_search**: on-demand definition scan (Rust/Python/JS/TS/Go) with no
  index to invalidate, ignore-aware, capped output. Live-verified through the
  harness (`estimate_text_tokens` → `lib.rs:127` in 5 ms) and served over
  `--mcp-serve`, so external agents get it too — the audit's "code-graph as
  MCP" gap closed at scan fidelity, with a true graph still future work.

## 7–8 — done

- **ACP** (`harness --acp`): a translation layer over the daemon, so an editor
  session inherits resident sessions, MCP tools, journals and gates. Streaming
  updates, journal replay on `session/load`, and approvals as
  `session/request_permission`. Covered by `acp_e2e`: a real turn through a real
  editor-side client, a second turn on the same session, and an approval the
  editor refuses (asserting the command did not run).
- **Web UI** (`harness web [port]`): one embedded page, hand-rolled HTTP over a
  daemon connection, SSE for streamed updates, token-guarded, localhost only.
  Covered by `web_e2e`: 401 without the token, page served with it, a real prompt
  through `/rpc`, history replay, and deltas arriving on the event stream while a
  turn runs.


Web research tool behind the existing gates · code-graph index served over our
own MCP server · opencode UI portability assessment (short answer: their TUI is
Go/Bubble Tea — nothing is *directly* portable — but their **web UI over a
server** pattern maps cleanly onto our daemon+wire protocol, and their **ACP
server** would let editors drive us; both are protocol work on top of what we
already have, not UI rewrites).
