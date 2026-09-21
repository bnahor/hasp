# P11 — closing the audit gaps, in order

Status: items 1–2 COMPLETE (2026-09-20); 3 onward next

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

## 5–7 (this turn if budget allows, else next)

Web research tool behind the existing gates · code-graph index served over our
own MCP server · opencode UI portability assessment (short answer: their TUI is
Go/Bubble Tea — nothing is *directly* portable — but their **web UI over a
server** pattern maps cleanly onto our daemon+wire protocol, and their **ACP
server** would let editors drive us; both are protocol work on top of what we
already have, not UI rewrites).
