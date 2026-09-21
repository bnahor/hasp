# P1a Plan — Setup & TUI Parity

> **Status (2026-09-19): COMPLETE.** `cargo test --workspace` green (24 tests), fmt clean, doctor + `update --models` verified against the live models.dev API (14 models, 14 priced, windows from `limit.context` — schema verified against the real payload). TUI compiles and runs; the streaming/approval interaction is exercised manually (needs a key) rather than by automated tests.
>
> Note: ratatui/crossterm render code is untested by unit tests by design — PTY-level e2e arrives with P2 (the same milestone as Grok's pager-pty-harness pattern).

Closes the parity gap with the studied agents (pi, Claude Code, Grok Build, Prime, codegraff) on setup and terminal UX. Scope agreed 2026-09-19 after P0.

## Scope

**In:**
- Credentials: `harness login` (hidden-input prompt, stores to `~/.harness/credentials.json`, 0600); resolution order env `ANTHROPIC_API_KEY` → file → first-run prompt. Shape leaves room for OAuth tokens later.
- Model catalog: bundled fallback (ids + windows) + `harness update --models` fetching `https://models.dev/api.json` (codegraff's source) into `~/.harness/models.json`; `/model <id>` switching (appends `model_change` entries); footer shows context vs. the *actual* model window; cost line when the catalog carries prices.
- TUI (ratatui + crossterm, alternate screen): streaming scrollback, multi-line input editor (Ctrl+J newline), input history (↑/↓), message queue while a turn runs, status footer (model · ctx · cache R/W · cost · session), tool notes, in-UI permission prompts (y/a/n), scrolling (PgUp/PgDn), `/exit /new /name /stats /model /resume /clear /help`.
- `/resume`: session lister (id, name, created, entry count) + `/resume <n>` reopen.
- `harness doctor`: key present, git present, sessions/trace dirs writable, catalog source + model count.
- `--plain` retains the P0 line REPL for pipes/debugging.

**Out (tracked as remaining parity debt):**
- Subscription OAuth login (pi/Claude Code style) — needs PKCE + refresh; credentials file shape prepared.
- Markdown rendering of assistant text (raw text + wrap for now).
- Turn cancellation (Esc) — engine has no cancel API yet (P2 with the wire).
- curl \| sh installer and self-update (cargo install is the personal-tool answer for now).

## Verification

- `cargo test --workspace`: catalog parse (fallback + fetched-shape fixture), credentials roundtrip + perms, session lister, `model_change` entry roundtrip.
- Manual: `harness login`, `harness doctor`, TUI run incl. queued messages, `/model`, `/resume`, approval prompts, footer cache/cost.

## Notable design points

- Engine shared as `Arc<tokio::sync::Mutex<Engine>>` between UI thread and per-turn spawned tasks.
- Approvals cross threads via std mpsc (`Mutex<Sender>` inside the gate — `Gate: Send + Sync`); the gate blocks a runtime worker while awaiting the human, acceptable at this scale, noted in code.
- Slash-command state changes between turns only; during a turn, input queues (pi's message-queue behavior).
