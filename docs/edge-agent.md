# Edge agent: safety, restarts, recall, edge execution

Hasp runs as the session/execution layer on two home nodes (an M1 Mac and a
Windows/WSL PC). Threadline owns shared task intent and ownership, Dagu owns
runs and concurrency (through the homelab-mesh hub), and the mesh owns device
identity. This page covers what hasp itself guarantees.

## Bash gate (no prompt only for proven read-only)
`hasp_tools::read_only_invocation` parses the command (quotes included) and
accepts a short list of read-only tools with their argument restrictions (see
README "Sandbox"). Everything else asks, or is refused when nobody can answer
(`--mcp-serve`, a detached daemon). Owner-vouched build/test commands go in
`~/.hasp/policy.json` `bash.allow`. Structural rules (no metacharacters, no
paths outside the workspace) and the OS sandbox apply in every mode; under
`HASP_SANDBOX=read-only` the workspace is denied explicitly, so a workspace
under a temp directory is still read-only.

MCP tools are gated too (`check_tool`): `auto` mode runs read-only tools by
name and asks for the rest; `~/.hasp/policy.json` `tools.allow|ask|deny`.

## Journal durability
The session JSONL is canonical. An append is part of history only after the
whole line is written and synced (`HASP_JOURNAL_FSYNC=0` skips the fsync).
If an append fails the session *faults*: nothing more is appended, the
half-written line is cut off, and the engine stops before the next model
request or tool run (`session journal is not durable`). Reopen to continue.

Opening a journal: a torn final record (no newline, not valid JSON) is moved
to `<file>.torn-<offset>` and the file truncated to the last complete entry;
a complete entry missing only its newline is kept; anything unparsable in the
middle is an error, never skipped or rewritten.

## Daemon restarts (resident sessions)
`hasp resident enable` (in a workspace) records that session in
`~/.hasp/residents.json`; the daemon restores those sessions when it starts,
with no client attached. Rules:
- the recorded session file is reopened, not "the latest";
- a turn interrupted by the crash is not resumed: its open tool calls are
  closed as interrupted, and nothing runs until a prompt or a schedule;
- interval and cron schedules restart from now; missed runs are skipped;
- one-shots are journaled `fired` before they run (never fired twice); one
  whose time passed during the downtime is journaled `expired`, not run late;
- new schedule ids continue the journal's numbering.

The system prompt stays byte-stable per session: project instructions and
memory are snapshotted on first use; `reload_instructions` adopts edits.

## Recall, memory, workflows (crate `hasp-recall`)
- `recall` tool / `hasp recall`: SQLite FTS5 over every journal on this node,
  incremental by byte offset, rebuildable (`hasp recall rebuild`). Node-local;
  copy journals between machines, never the index.
- `memory` tool: the agent can list and *propose*; only the owner confirms,
  edits or forgets (`hasp memory …`, the gateway's owner commands, the web
  view). Third-party text can't become a rule and is shown as "reported".
  Same-key conflicts need an explicit `supersedes`. Forget removes the text
  from the memory file (journals it came from are untouched).
- `workflow_propose` tool: versioned drafts in `~/.hasp/workflows/`; the owner
  activates a reviewed version by hash (`hasp workflow activate NAME V
  --sha256 H`), which installs it as an ordinary skill. No separate executor.

## Edge execution (crate `hasp-edge`)
- `hasp-edge worker`: claims Threadline tasks labelled `edge` whose
  `needs:<cap>` labels this node satisfies (`node:<name>` pins), persists the
  lease, and submits a Dagu run through the hub (`/api/edge/submit`, node
  credential, submission id = (task, fencing token)). Renews while it waits.
- `hasp-edge exec` (the Dagu job): re-checks the lease *with the
  coordinator* before anything else; a stale job exits 3 without running
  hasp. Runs `hasp -p` in a per-task workspace, watches for cancellation,
  publishes the session as an artifact (`hasp-session://<node>/<id>`) and
  completes with `{node, session_id, dag_run_id, fencing_token}`.
- Limits: no exactly-once. An effect hasp already started is not undone when
  a lease is lost or a task cancelled; fencing makes the late completion
  fail. Two-node demo: homelab-mesh `edge/demo.py`.
