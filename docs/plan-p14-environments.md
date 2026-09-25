# P14 — environments (steps 1 and 2 of 4)

Status: steps 1 and 2 landed 2026-09-25. 195 tests (was 184); fmt and clippy
clean. Verified live with DeepSeek V4.1 Flash and in a real Docker e2e.

## What exists

- **Guest agent (step 2).** `harness --mcp-serve`, now concurrent (each
  `tools/call` is a task; replies share one writer) and cancellable
  (`notifications/cancelled` aborts the call; dropping a tool's future kills
  its command's process group; a disconnected client aborts everything). The
  MCP client has per-client call timeouts and sends `notifications/cancelled`
  for any call it abandons (dropped future or timeout). `--guest-idle` is the
  container's main process, so no image needs `sleep` or a shell.
- **`harness-env` (step 1).**
  - `EnvSpec` (`.harness/environments/<name>.toml`, home fallback): image,
    platform, cpus, memory, pids, network none|full, env, setup; validated.
  - `Backend`/`Instance` traits. The only thing the rest of the harness needs
    from an instance is `guest_command()`: a command whose stdio reaches the
    guest agent.
  - Docker backend: one container per environment session, workspace (and a
    worktree's main `.git`) mounted at the same host path, guest at
    `/harness-guest` as `--init` entrypoint, labels
    `harness.env/workspace/pid`, host uid on Linux.
  - Proxy tools for `ENV_TOOLS`, same names/schemas, host gate first,
    same concurrency flags as the local tools.
  - Guest binary selection by ELF machine: `HARNESS_GUEST_BINARY`,
    `~/.harness/guest/`, or the running binary on a same-arch Linux host.
- **Wiring.** `--env` / `HARNESS_ENV` (embedded, `-p`, REPL, daemon via the
  session-open `environment` param); `spawn`/`subagent` `environment`, with
  inheritance from the parent; `agents` and spawn results name the instance;
  `harness env list|check|prune|build-guest`; `doctor` line.

## Evidence

- `env check` on python:3.12-slim with cpus 1.5 / 512m / pids 256 / no
  network: cgroup reads 150000/100000, 536870912, 256; no interface up.
- Live session (V4.1 Flash): the model's `write` appeared on the host,
  code ran inside (aarch64, Python 3.12.14), network unreachable, setup and
  `[env]` applied, 0 containers left. 3 samples, 4.9 s, $0.002.
- Live spawn: a mutation child ran in its own container (no `git` in the
  image) with its worktree mounted; the file landed in the worktree on the
  host; 0 containers left.
- `prune`: kept a live session's container, removed it after SIGKILL of the
  owner.
- Tests: `env_e2e` (limits, a scripted session inside, unknown env),
  `mcp_server_e2e` (concurrency, cancellation of an abandoned call), unit
  tests for spec, ELF detection, worktree `.git` resolution, liveness.

## Known limits

- Network is `none` or `full`; an egress allowlist needs a proxy or nftables
  and belongs with step 3.
- A child's environment lives as long as the child's engine (until the parent
  session ends), so a child can be messaged again without a restart.
- The workspace is bind-mounted (live, shared). Firecracker has no shared
  filesystem, so step 3 needs a copy-in/copy-out or block-device workspace.

## Next: step 3 (Firecracker) plugs in at `Backend`

Kernel + OCI→ext4 rootfs, the Firecracker API config for the spec
(`vcpu_count`, `mem_size_mib`, drives, tap), the jailer, tap/bridge networking
with an egress policy, vsock bridged to stdio for `guest_command()`,
snapshot/restore for a warm pool. Developed on this Mac inside a Lima VM with
nested virtualization (Firecracker needs Linux KVM).
