# P9 — install, any-directory robustness, TUI clean

Status: COMPLETE (2026-09-20)

Everything verified with the real binary: `--version`/`-V`; doctor's install
block (version, running binary, PATH membership, and the exact
`cargo install --path …` command when running from `target/`);
`cargo install --path crates/hasp-cli` → `~/.cargo/bin/harness` → `--version`
→ doctor `install: on PATH` → a one-shot from PATH in a fresh directory. The
directory matrix (spaces, unicode, symlink → resolved target, 15-deep nesting,
plain non-git) all pass embedded *and* through the daemon; a read-only directory
fails with a clean `Error:` and exit code 1. The reopen flow is pinned by
`tui_replays_history_when_reopening`: turn → exit → reopen → adopts the live
resident with history on screen.

Two real defects found and fixed along the way:

- `serve()` chmod'ed its socket's *parent* to 0700 — correct for directories it
  created, but `HASP_SOCKET=/tmp/x.sock` tried to chmod `/tmp` itself and
  died with a bare `EPERM`. It now restricts only directories it created.
- The daemon wrote a blank line after every message (`encode` already ends with
  `\n`, the writer added another). Our client skipped them; any other speaker
  of the protocol had to know to.

## What the user asked for

Spawn the harness in any directory on the computer; install path correct;
install/update "stupid easy, like pip"; the TUI clean.

## Findings from the survey

- **No `--version`.** A tool you can update needs to say what it is.
- **`cargo install --path .` cannot work** — the repo root is a virtual
  workspace, so the real command is `cargo install --path crates/hasp-cli`.
  Untested until now; the README has no install section at all.
- **`hasp update` only refreshes the model catalog.** Fine — self-update
  has nothing to pull from yet (no git remote, no releases) — but doctor should
  say how the binary got here and whether it is a dev build from `target/`.
- **The TUI opens to a blank screen.** `WireBackend::spawn` prints
  `▪ session {sid}` and nothing else: no prior turns, and now that sessions are
  daemon-resident, no sign that it *adopted a live session*. History lives in
  the journal; it just isn't shown.
- **`/heartbeat` in the command table predates cron.**

## Plan

1. `--version`/`-V` → `hasp <semver>` from `CARGO_PKG_VERSION`.
2. Doctor gains an install block: version, running binary path, whether that
   binary is reachable via `PATH`, and — when running from a `target/` dir —
   the exact `cargo install --path …` command to install properly.
3. Verify `cargo install --path crates/hasp-cli` end-to-end (build, install
   into `~/.cargo/bin`, `hasp --version` from PATH, a turn in a temp dir),
   then document it in the README, including updating
   (`git pull && cargo install --path crates/hasp-cli`) and the model
   catalog (`hasp update --models`).
4. Any-directory matrix, mock provider: a plain non-git dir, a directory with
   spaces, unicode, a symlink, a deep path, `$HOME` itself, and a read-only
   directory (which must fail with a clean error, not a panic). One-shot `-p`
   and a daemon attach each.
5. History replay: new wire method `session/history` (last N user/assistant
   texts, plus the total), handled by the daemon and by the embedded backend's
   `call`; the TUI renders it on open through a new `UiEvent::History`, with
   an explicit "attached to a live session" note when it adopted one.
