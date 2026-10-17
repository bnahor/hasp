# Desktop parity: Codex-style local workflows

Target selected by the user: Codex desktop app. This is a capability checklist,
not a claim of full parity or a visual clone.

## Reference and scope

OpenAI's public feature entry point was checked on September 28, 2026:
<https://developers.openai.com/codex/app/features>. It currently presents a
broader ChatGPT desktop feature index, including projects/chats, scheduled
tasks, notifications, files, plugins, browser/computer use and remote work.
Harness's immediate scope here is the local coding workflow. Hosted services,
account integrations and general-purpose desktop capabilities are not implied
by the checklist below.

## Capability map

| Workflow | Harness desktop status |
| --- | --- |
| Projects, session history, search, tabs | Existing |
| Concurrent work in separate worktrees | Existing; creation, resident sessions and switching |
| Diff review and comments sent to the agent | Existing |
| Commit / merge worktree changes | Existing; not a full Git branch/PR client |
| Model and reasoning controls, streaming, approvals | Existing |
| Browse workspace directories | Added: folders first, breadcrumb navigation, per-folder filtering, refresh |
| Read file contents alongside chat | Added: escaped text, line numbers, copy path, add reference to draft |
| Quick-open a workspace file | Added: Cmd/Ctrl+P, searchable file index, opens its folder and preview |
| Discover and use installed skills | Added: sidebar and command palette; choosing a skill adds an explicit skill-tool request to the draft |
| Create/manage scheduled tasks | Added: sidebar and `/schedule`; interval, one-shot and local-time cron; pause/resume/remove and next-run display |
| Interactive terminal | Not implemented; safety/dispatch prerequisite below |
| File editing and save conflict handling | Not implemented; browser is read-only |
| Repository-wide content search | Not implemented as a desktop panel; quick-open searches filenames |
| Desktop completion notifications / inbox | Not implemented |
| Hosted remote tasks, GitHub PR UI, plugins/browser/computer use | Not part of this implementation |

## User controls

- Files: toolbar folder button or Cmd/Ctrl+Shift+E.
- Quick-open: Cmd/Ctrl+P, `/files`, the Files pane's search button, or the
  command palette. Cmd/Ctrl+K remains the command/session/project palette.
- Skills: sidebar **Skills**, `/skills`, or command palette. Choosing a skill
  does not send a message or execute anything automatically.
- Scheduled tasks: sidebar **Scheduled**, `/schedule`, or command palette.
  The dialog identifies the current project/session, shows the next due time,
  and refuses changes if that session is no longer resident.

## Important limits

- Directory responses are limited to 5,000 entries. Quick-open uses the
  existing 6,000-file index, which skips VCS/build/cache directories. Direct
  folder browsing includes hidden/build files except `.git`; it skips links
  escaping the workspace and special files.
- Previews read at most 256 KiB plus one truncation-detection byte. Binary files
  get a notice, oversized text gets a truncation notice. Contents are rendered
  as text, never interpreted as HTML. Missing/inaccessible files surface errors.
- Browsing uses `Workspace::confine`: relative paths only, no parent traversal,
  no symlink escape. New endpoints use the existing server token guard.
- Skills discovery uses the engine's existing `.hasp/skills` and
  `.claude/skills` precedence. The catalog exposes names/descriptions only.
  Discovery can create the harness home directory via the existing helper;
  it does not write skill files. An engine snapshots available skills when
  built, so a newly installed skill may require a new session.
- Schedules belong to a session, not a global automation inbox. They use the
  existing journal-backed heartbeat scheduler and run only while the daemon
  is running. Cron follows the machine's local timezone. A running turn may
  receive scheduled work through its notice inbox; this is not a fresh isolated
  worktree per run. Approvals still apply and unattended requests are denied.
  Resuming an interval/one-shot task makes it immediately eligible; cron waits
  for its next matching time.
- File/project async responses are guarded so stale responses cannot overwrite
  a newly selected workspace's browser state.

## Terminal prerequisite

Do not wire an unrestricted shell to a new HTTP endpoint. Inspection found
that `GateCore::decide_bash` lets yolo return early and treats metacharacters as
an auto-approval restriction rather than a universal structural rejection.
Calling the tool registry directly would also skip the engine's pre/post-tool
hooks; setting a subprocess's working directory is not filesystem isolation.

A terminal/command pane needs a shared dispatch entry point preserving policy
hooks, explicit non-bypassable command constraints, and a bounded job-output
path before exposing execution. An interactive PTY is a separate capability;
a noninteractive command runner must not be labelled as PTY parity.

## Verification

- `cargo test --workspace` passed after the initial browser implementation.
- The web end-to-end test exercises real token-protected file routes, nested
  paths/spaces, text and binary previews, truncation, traversal/symlink rejection,
  and skill metadata discovery.
- Browser smoke verification uses an isolated temporary workspace and mock
  provider, not a user's active session or paid model.
