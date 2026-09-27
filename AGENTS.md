# harness

A personal agent harness. Research and architecture live in `research/`; the active implementation plan is `docs/plan-p0.md`.

## Commands

- `cargo test --workspace` — all crates
- `cargo run -p harness-cli` — TUI (needs a key: env `ANTHROPIC_API_KEY`, `harness login`, or first-run prompt)
- `cargo run -p harness-cli -- --plain` — line-REPL fallback
- `cargo run -p harness-cli -- -p "do something" --yolo` — one-shot
- `harness app` — the desktop app (sidebar, conversation, changes pane) over the daemon
- `harness review [new|edit|send|approve --land]` — review agent work in $EDITOR
- `harness providers` · `harness --provider kimi` — coding plans on our engine
- `harness agents` · `harness --agent claude|codex|gemini|opencode` — drive a vendor CLI on the user's own plan
- `harness login` · `harness doctor` · `harness update --models` (fetches models.dev)

## Invariants (do not break these in contributions)

- The append-only session JSONL is canonical state; never mutate written entries — branch, compact, or append instead.
- Structural safety is never bypassable by approvals: path confinement (relative-only, no `..`, symlink checks) and the bash metacharacter rule apply in every mode, including yolo.
- The system prompt stays byte-stable within a session: no timestamps, run ids, or other dynamic content beyond workspace root + project instructions.
- Tool list is name-sorted and schemas are stable — tool order is part of the cache contract.
