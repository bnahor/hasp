# Functionality audit: harness vs pi, opencode, goose, graff (codegraff)

2026-09-20. Sources: `--help` output on this machine for pi and opencode (marked
*local*), official documentation found via search for goose (Block / Agentic AI
Foundation) and graff (justrach/codegraff) (marked *docs*), and this repo's own
code and tests for us. Claims sourced only from marketing copy are marked; where
I could not verify, it says `?`. "us" = this harness, everything below verified
by tests or direct measurement unless noted.

## Matrix

Legend: ✓ has it · ✗ does not · partial = has some of it.

| capability | us | pi | opencode | goose | graff |
| --- | --- | --- | --- | --- | --- |
| read/write/edit/bash tools, diffs, previews | ✓ | ✓ local | ✓ local | ✓ docs | ✓ docs |
| approval gates + structural rules (confinement, no bypass) | ✓ | ✓ local | ✓ docs | ✓ docs | ? |
| OS-level sandboxing (fs/net isolation) | ✗ | ✗ | ✗ | ? | ? |
| append-only journal, resume, fork | ✓ + branch | ✓ local | ✓ local + export/import | ✓ docs | ✓ docs |
| compaction + **measured** token accounting | ✓ | partial (estimates) | ✓ local (`stats`) | ? | ✓ docs (outcomes) |
| daemon by default, sessions outlive the client | ✓ | ✗ | ✓ local (`serve`/`attach`) | ✓ docs (`goosed`) | ✓ docs (`serve`) |
| **detached schedules: interval, one-shot, cron** | ✓ | ✗ | ✗ | ✗ (recipes are manual runs) | ? |
| goals / autonomous mode with exit gates | ✓ | ✗ | ✗ | ✗ | ✓ docs (runs until done) |
| multi-agent: child bus, worktrees, sync subagent | ✓ | partial (delegation) | ✓ local (`agent`) | ✓ docs | ✓ docs (workers) |
| worktree isolation + safe sweeps | ✓ | ✗ | ✗ | ✗ | ✗ |
| MCP client | ✓ | ✗ local (no MCP in help) | ✓ local | ✓ docs (MCP-first) | ✓ docs |
| MCP **server** mode | ✓ | ✗ | ✗ (serve is its own API, not MCP) | ✗ (goosed is REST) | ✗ |
| provider breadth (incl. local/Ollama) | partial: 4 + any OpenAI-compat endpoint, no code change | ✓ local (multi) | ✓ local (`providers`) | ✓ docs (Ollama) | ✓ docs |
| model catalog + live cost accounting | ✓ | ✓ local (catalogs) | ✓ local (`stats`) | ? | ✓ docs |
| TUI | ✓ (light/dark themes) | ✓ local (very polished) | ✓ local | ✗ CLI+desktop docs | ? |
| plain REPL / one-shot / `-p` | ✓ | ✓ local | ✓ local (`run`) | ✓ docs | ✓ docs |
| web / desktop UI | ✓ (browser client) | ✗ | ✓ local (`web`) | ✓ docs (desktop) | ✗ |
| ACP (Agent Client Protocol) server | ✓ | ✗ | ✓ local (`acp`) | ✗ | ✗ |
| extensions / plugins / skills | ✗ | ✓ local (extensions, themes, skills) | ✓ local (`plugin`) | ✓ docs (extensions= MCP, recipes) | ✓ docs (SDKs) |
| shareable workflow packages | ✗ (goals are per-session) | ✗ | ✗ | ✓ docs (recipes: YAML) | ✓ docs (workflows) |
| code-graph / code-intelligence index | partial: on-demand symbol scan over MCP | ✗ | ✓ docs (IDE-grade index claim) | ✗ | ✓ docs (CodeDB) |
| `@file` completion, syntax highlighting | ✓ | ✓ local | ✓ local | ? | ? |
| web research tools built in | ✓ (gated egress) | ✗ | ✗ | ? | ✓ docs |
| GitHub/PR integration | ✗ | ✗ | ✓ local (`github`, `pr`) | ✗ | ✗ |
| session export/import | ✓ | ✗ | ✓ local (`export`/`import`, db) | ? | ? |
| self-update | ✗ (reinstall; documented) | ✓ local (`update self`) | ✓ local (`upgrade`) | ✓ docs (desktop) | ✓ docs (install.sh) |
| install footprint / startup (measured here) | **6.5 MB · 3 ms · 16 MB RSS** | 434 MB pkg · 149 ms · 250 MB | 137 MB · 220 ms · 807 MB | n/a here | n/a here |

## What this says

**Where we genuinely lead (all verified by tests or measurement):**

1. **Detached execution.** Daemon-resident sessions with interval, one-shot and
   cron schedules that keep firing with no client attached, and turns that
   outlive their client. None of the four advertises cron-with-detachment;
   goose recipes are manually invoked workflows, not schedules.
2. **Scheduling semantics.** Claim-before-deliver, coalescing, wall-clock cron
   in local time, validation at set time, journaled intent.
3. **MCP in both directions** — we are a client *and* a server (`--mcp-serve`),
   which none of the four is (their servers are their own APIs).
4. **Economics of the harness itself**: one 6.5 MB static binary, 3 ms cold
   start, 16 MB peak RSS, 39 ms one-shot overhead, +13 ms to load a 4,000-entry
   journal. Measured head-to-head: 4× faster than pi and Claude Code on a full
   one-shot turn here.
5. **Worktree isolation with safe sweeps** for mutating child agents, branch
   preserved on prune.

**Where they lead (no hedging):**

1. **Extensibility** — pi's extensions/themes/skills, opencode plugins, goose
   MCP-extensions + recipes, graff's typed JSON protocol with TS/Python SDKs.
   We have none of this; our only extension surface is MCP itself.
2. **Provider breadth**, especially local models (Ollama). We ship three cloud
   providers plus a mock.
3. **UI surface**: opencode's web UI and ACP server; goose's desktop app; pi's
   TUI polish (themes, light mode — both still open items for us).
4. **Code intelligence**: graff's CodeDB and opencode's index; we have
   `@file`-completion and syntax highlighting only, with graph indexing still
   on the roadmap.
5. **Ecosystem integration**: opencode's GitHub/PR flow and session export
   import; everyone's self-update.
6. **Web research** (graff ships it built in; we have no web tool at all).
7. **Sandboxing** is absent everywhere as far as verified — nobody wins this
   row, including us; our structural rules are confinement + gates, not OS
   isolation.

## Prioritized gaps (what parity would cost us, in order)

1. ~~Local models via an Ollama provider~~ **landed 2026-09-20**, with the
   general form: an OpenAI-compatible provider (`--provider ollama|compat`)
   whose model list is discovered from the endpoint, so new models and new
   vendors need no code change (`docs/plan-p11-gaps.md`). Verified live against
   DeepSeek's OpenAI endpoint.
2. Self-update once the repo has a remote (`harness update` reinstalling).
3. Session export/import commands — trivial over our JSONL.
4. Themes/light mode (already planned Tier 1).
5. Web research tool (fetch + read) behind the same gates.
6. Code-graph index served over our MCP server (planned; CodeDB-like).
7. Extension surface beyond MCP — the genuinely large one.
8. ~~ACP server / web UI~~ **both landed 2026-09-20** (`docs/plan-p11-gaps.md`):
   ACP on stdio for editors and a localhost browser client, each a thin front end
   on the daemon rather than a second implementation.

## Update 2026-09-23 (docs/plan-p13-parity.md)

Rows that changed for us:

- **OS-level sandboxing**: ✗ → ✓ (`HARNESS_SANDBOX`: Seatbelt on macOS, bwrap on Linux).
- **extensions / plugins / skills**: ✗ → ✓ for prompt templates, skills (progressive
  disclosure) and pre/post/stop hooks, `.claude/`-compatible. Still no compiled
  plugin API; MCP remains the tool-extension surface.
- **shareable workflow packages**: ✗ → partial (templates + skills are files you can share).
- **code search**: native gitignore-aware `grep`/`glob`, parallel and prompt-free.
- **prompt caching**: the conversation tail is now cached (before: system + tools only).
