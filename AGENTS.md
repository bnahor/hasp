# hasp

A personal agent hasp. Research and architecture live in `research/`; the active implementation plan is `docs/plan-p0.md`.

## Commands

- `cargo test --workspace` — all crates
- `cargo run -p hasp-cli` — TUI (needs a key: env `ANTHROPIC_API_KEY`, `hasp login`, or first-run prompt)
- `cargo run -p hasp-cli -- --plain` — line-REPL fallback
- `cargo run -p hasp-cli -- -p "do something" --yolo` — one-shot
- `hasp app` — the desktop app (sidebar, conversation, changes pane) over the daemon
- `hasp review [new|edit|send|approve --land]` — review agent work in $EDITOR
- `hasp providers` · `hasp --provider kimi` — coding plans on our engine
- `hasp agents` · `hasp --agent claude|codex|gemini|opencode` — drive a vendor CLI on the user's own plan
- `hasp login` · `hasp doctor` · `hasp update --models` (fetches models.dev)

## Invariants (do not break these in contributions)

- The append-only session JSONL is canonical state; never mutate written entries — branch, compact, or append instead.
- Structural safety is never bypassable by approvals: path confinement (relative-only, no `..`, symlink checks) and the bash metacharacter rule apply in every mode, including yolo.
- The system prompt stays byte-stable within a session: no timestamps, run ids, or other dynamic content beyond workspace root + project instructions.
- Tool list is name-sorted and schemas are stable — tool order is part of the cache contract.
