# P6 Plan — TUI parity

The TUI is the weakest surface: it prints raw text into a flat list. 2026-09-20.

## What is missing today (measured, with a scripted-model screenshot)

1. Markdown is dumped verbatim — `# headings`, `**bold**`, ` ```rust ` fences and
   backticks all show as source.
2. **Tool output is invisible.** The engine streams assistant text and one-line
   notes (`▶ bash …`, `✓ bash (22 ms)`); the tool's actual result never reaches
   the UI, so you cannot see what a command printed or what a write changed.
3. No diffs for `write`/`edit`.
4. Input is append-only: no cursor, no left/right/home/end/delete, no command
   completion. Editing a long prompt is impossible.
5. Footer has no context-pressure signal (colour by fill), no busy spinner.
6. Long sessions rebuild every line into a fresh `Text` each frame.
7. Glyph-heavy output looks wrong on terminals whose font lacks `▪ ▶ ✓ ─`.

## Scope, in order

- **Markdown renderer** (`src/markdown.rs`): headings, bold/italic, inline code,
  fenced code blocks with a language label, bullets, numbered lists, quotes,
  rules, links. Wrapping left to ratatui. Unit-tested.
- **Tool events end to end**: `ToolEvent` in core, emitted per tool with args and
  a result preview; `TurnSink::on_tool` with a default impl so existing sinks are
  unchanged; structured `UiEvent::Tool` in both backends (embedded + wire, via a
  structured `session/update` payload). UI renders a compact block: header with
  the command/path, status + duration, diff for writes/edits, output preview.
- **Input editing**: cursor position with arrows/home/end/word/delete, history,
  and a slash-command completion popup.
- **Footer**: context bar coloured by fill, busy spinner, queue depth, cost.
- **`--ascii`**: swap glyphs for ASCII for terminals that render `▪ ▶ ✓ ─` badly.

Out of scope here (tracked): mouse support, `/tree` picker, mid-turn
detach/reattach, syntax highlighting inside code blocks, tables, markdown
*while streaming* beyond the tail window.

## Verification

- Unit tests for the markdown and diff renderers.
- PTY screenshots before/after using a scripted mock turn (deterministic, free).
- Existing PTY/daemon e2e suites stay green.

## Parity checklist (pi / opencode / Claude Code as the bar)

Done in this pass:

- [x] markdown rendering: headings, bold/italic, inline code, fenced blocks with
      a language label, bullets, numbered lists, quotes, rules, links
- [x] tool calls are visible at all: header with the command/path, status,
      duration, output preview, "N more chars" truncation note
- [x] diff-aware output styling (any `+`/`-`/`@@` lines render green/red/cyan)
- [x] styled-line cache + windowed viewport (a frame no longer re-parses the
      transcript or clones all of it)
- [x] streaming message rendered as markdown each frame (fences snap in)
- [x] footer: context-pressure bar (green→yellow→red), busy spinner, queue
      depth, cache R/W, cost, scroll indicator
- [x] `--ascii` / `HARNESS_ASCII` for terminals that misrender `▪ ▶ ✓ ─`

Next, roughly in value order:

- [x] input editor: cursor position, arrows/home/end/word/delete, ctrl+a/e/u/k/w
- [x] slash-command completion menu (tab completes, ↑/↓ navigate; `/help` shares the table)
- [x] `@file` completion (workspace file list fetched from the backend,
      filtered by the token under the caret, tab inserts)
- [x] syntax highlighting inside code blocks (dependency-free: keywords, strings,
      numbers, comments; block comments span lines)
- [x] real diffs from `write`/`edit` (new `harness-tools::diff`, returned in tool output)
- [x] collapsible tool and reasoning blocks (ctrl+o toggles the newest; collapsed
      bodies show 6 lines plus `… N more line(s)`)
- [x] reasoning streamed as its own event (dim italic, then kept in the
      transcript behind a `thinking · N lines` marker); collapse/expand is still open
- [x] mouse wheel scrolling (scrollback search still open)
- [x] pickers for `/model` and `/resume` (menu lists the catalog / sessions, tab
      inserts the argument)
- [x] `/help` overlay (any key closes)
- [x] git branch + repo name on the status line
- [x] bracketed paste (a paste never submits); ctrl+r history search still open
- [x] external editor handoff (ctrl+g, `$VISUAL`/`$EDITOR`, prompt preserved on failure)
- [x] CJK/wide-char width correctness for the caret (display cells, not chars)
- [ ] theme file + light theme
- [ ] mid-turn detach/reattach
- [x] terminal bell when a turn ran ≥30s (you looked away)

Additional coverage added with the checklist: tool-preview capping tests and the
markdown renderer's unit tests.

## Status after the second TUI pass (2026-09-20)

Done: markdown, syntax highlighting, tool output + diff-aware colouring, **real diffs from
`write`/`edit`**, cached/windowed viewport, footer context bar + spinner,
`--ascii`, input editor + command completion, mouse wheel scrolling, bracketed
paste, `/help` overlay, repo/branch status line, reasoning streamed as its own
event, collapsible tool/reasoning blocks, `TestBackend` render tests.

Open in this file: scrollback search, fuzzy `/resume` and `/model` pickers, ctrl+r
history search, external editor handoff, themes/light mode, CJK width
correctness, mid-turn detach/reattach.
