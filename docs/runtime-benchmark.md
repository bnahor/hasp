# Container runtimes for environments: runc vs gVisor vs Kata

2026-09-25. The question: which runtime should an environment use? Reproduce
with `bench/runtimes/` (a Lima VM, `setup.sh`, then `python3 bench.py`).

## Setup

- Apple M5 Pro host; Lima VM (Ubuntu 24.04, 8 vCPU, 16 GB) on Apple's
  Virtualization framework **with nested virtualization** (so KVM exists for
  Kata). Docker 29.8, gVisor release-20260921.0 (default systrap platform),
  Kata Containers 4.2.0 with Cloud Hypervisor.
- Every workload runs **through a harness environment** — a `bash` tool call
  to the guest agent, one fresh environment per session, exactly as an agent
  would — and also times itself inside the environment. Spec for every
  runtime: `image = "rtbench:1"`, `cpus = 2`, `memory = "4g"`.
- Workloads: pinned sqlglot v30.19.0 (1,277 tests), gcc on the SQLite
  amalgamation, 5,000 small files created/read/deleted, `git status` +
  `git grep` + `grep -rn`, and the harness's own `grep` tool. Tests and files
  run both on the image filesystem and on the mounted workspace.
- 3 repetitions per runtime, runtime order rotated each repetition. Medians;
  the spread between repetitions was under 2% for runc and gVisor.

## Results

| workload | runc | gVisor (runsc) | gVisor overhead |
|---|---|---|---|
| session start + teardown (no tools) | 0.32 s | 0.38 s | +0.06 s |
| pure-Python CPU loop | 331 ms | 343 ms | +4% |
| gcc, SQLite amalgamation | 4.42 s | 4.75 s | +8% |
| 1,277 tests, image filesystem | 36.6 s | 40.8 s | +11% |
| 1,277 tests, mounted workspace | 36.4 s | 39.8 s | +9% |
| 5,000 small files, image filesystem | 125 ms | 428 ms | 3.4× |
| 5,000 small files, mounted workspace | 98 ms | 562 ms | 5.7× |
| git status + git grep + grep -rn | 10 ms | 69 ms | 6.9× |
| harness `grep` tool (round trip) | 17 ms | 54 ms | 3.2× |
| **whole workload session** | **78.2 s** | **87.2 s** | **+12%** |
| memory per idle instance | ≈0 MB | 19 MB | |
| start 5 instances | 0.58 s | 0.71 s | |

Reading: gVisor's cost is concentrated where its design puts it — every
system call goes through its user-space kernel, so file- and process-heavy
operations are 3–7× slower — while compute-bound work is within 4–11% of
native. Because the file-heavy steps of a coding agent are milliseconds and
the expensive steps (test suites, compiles) are compute-bound, a whole agent
session costs **+12%**. Startup (+60 ms) and memory (+19 MB) barely move,
which is why a platform like Modal can pack huge numbers of gVisor sandboxes:
the case for gVisor is density and cheap isolation, not speed.

## Kata (microVM): not measurable on this machine

| | runc | Kata (Cloud Hypervisor), nested |
|---|---|---|
| pure-Python CPU loop | 331 ms | 11.7 s (35×) |
| gcc | 4.42 s | 458 s (~100×) |
| test suite | 36.6 s | killed at the 10-minute limit |
| start | 0.3 s | 10 s with 256 MB → 59 s with 4 GB |
| memory per idle instance | ≈0 MB | 186 MB |

A microVM does not make single-threaded computation 35× slower; on bare
metal Kata runs it at near-native speed. These numbers are the cost of a VM
inside a VM on Apple silicon, where every exit and every first touch of guest
memory (hence start time growing with memory size) is expensive. They say
nothing about Kata or Firecracker as technologies; measuring those needs a
bare-metal Linux host. The one structural number that does carry over is
memory: a microVM carries a guest kernel and agent, so its per-instance
overhead is a fixed reservation rather than gVisor's ~19 MB.

Other findings on the way:

- **Kata under Docker has no network.** Its containers have loopback only,
  even on the default bridge. Kata has supported containerd and CRI-O, not
  Docker, since 2.0; a microVM backend should talk to containerd directly.
- **Kata's Firecracker mode cannot run these environments on Docker's
  default storage.** Firecracker has no shared filesystem, so the container
  rootfs must be a block device (containerd's devmapper snapshotter), and a
  bind-mounted workspace is impossible — confirming that a Firecracker
  backend needs `workspace = "copy"` (git in, a branch out) first.
- **Kata 4.2 accepts only its shipped configuration paths in
  `KATA_CONF_FILE`;** a hypervisor is selected with the `ConfigPath` runtime
  option (see `setup.sh`).

## What this changed in the harness

- `runtime = "runsc" | "kata-…"` in an environment spec (`--runtime` for
  Docker).
- The guest-agent handshake allows 300 s: the first exec into a freshly
  booted microVM exceeded the 30 s MCP default.
- `harness env check` detects networking by default route
  (`/proc/net/route`): interface listings are wrong under gVisor (no
  `/sys/class/net`) and noisy under Docker Desktop (dormant tunnel devices).

## Recommendation

- **Trusted work on your own machine:** `runc` (the default). Fastest.
- **Untrusted code, or cheap strong isolation:** `runtime = "runsc"`: about
  +12% on a whole agent session, 19 MB per sandbox, Linux hosts only.
- **Hardware isolation or full-kernel features:** a microVM, on bare-metal
  Linux, through containerd rather than Docker, with `workspace = "copy"` —
  and measured there before choosing it.
