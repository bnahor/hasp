# Performance & Modern-Rust Analysis

Question on the table: *is DPDK applicable here? Anything else, more recent, Rust/performance-wise?*

Short answers: **DPDK — no** (wrong regime by 3–4 orders of magnitude). **io_uring runtimes — no for the core, with one carve-out.** The real levers are economic (cache), perceptual (streaming), and a handful of mechanical wins that a modern Rust stack gives you almost for free. Details below, ranked by actual impact on turn latency/cost.

---

## 1. Where the time actually goes (the frame that answers DPDK)

A typical turn, decomposed:

| Phase | Typical duration | Bottleneck |
|---|---|---|
| Model inference (prefill + decode, 1–N samples) | 5 s – 3 min | Provider GPUs — **not ours** |
| Tool execution (build/test/bash) | 0 – 10 min | User's build |
| Network (TLS request/SSE stream) | 50 – 400 ms RTT + streaming | WAN latency, provider |
| Local syscalls (log append, file read, spawn, IPC) | 1 – 50 ms total | **Irrelevant** |

Our controllable slice is the third row (~100 ms scale) and everything *perceptual* around it. DPDK's design point is **µs latency and millions of packets/sec on kernel-bypass NICs** — firewalls, load balancers, trading. We open **a handful of TLS connections per minute carrying KB/s of JSON/SSE**. Adopting DPDK means hugepages, NIC binding, privileged drivers, and losing the kernel TCP/TLS stack — to optimize the row that is already 2–3 orders of magnitude below the model call. Verdict: **not applicable — and the honest version of this rule is: no networking technology below ~10 ms of expected wins matters in this system.**

Steelman — where DPDK-class tech *would* enter the picture, none of them us (yet):
- A local inference gateway (oMLX-style SSD KV-cache server) — that's a separate system, and it's GPUs, not packets.
- A hosted multi-tenant fleet — a product decision far away.
- The eval rig at high parallelism — bottleneck is provider rate limits + TLS handshakes, solved by connection pooling (§3), not packet processing.

## 2. io_uring: the honest middle ground

Current state (2026): **monoio** (ByteDance; pure io_uring, thread-per-core, ~2× tokio at 4 cores, ~3× at 16, but *worse latency than tokio on 1 core / few connections*), **glommio** (Linux-only), **compio** (Iggy migrated to it: 5M msgs/s, fsync-per-message at ~9.5 ms p9999).

We are exactly the "few connections, latency-sensitive" profile — a daemon holding: 1–2 provider SSE streams per active session, a UDS listener, some subprocess pipes, a file watcher. That's **tens of fds, not hundreds of thousands**. Verdict: **tokio multi-thread for the core**. Carve-outs:
- If the workspace indexer (P5, codegraph-style) ever becomes a hot path (re-parsing thousands of files on big checkouts), run it as its own thread-per-core task — either a `rayon` pool or a dedicated `monoio`/`compio` thread — behind the same bus. Isolate it; don't let it dictate the runtime.
- File-watching and log writes are trivial volume; `notify` + buffered appends suffice (§5).

## 3. The levers that actually matter, ranked

### L0 — Token economics (minutes-scale; the real "performance")

- Cache-hit rate is worth **seconds-to-minutes per turn and most of the cost** (Earendil: a miss re-bills 100k+ tokens at full input price; Databricks: 2×+ cost-per-task across harnesses on the *same model*).
- Mechanisms already in `03-architecture.md`: stable prefix assembly, additive tools, cache-stats from day one, timed pruning when the TTL is dead anyway, keepalive pings (optional aider-style).
- Context discipline (pi): fewer tokens sent = faster prefill = cheaper turns. This beats every syscall-level optimization combined, by orders of magnitude.

### L1 — Provider request path (10–1000 ms; mechanical, one-time work)

- **rustls** (no OpenSSL link → static musl binary, fewer CVEs), **hyper 1.x + h2** with connection pooling and keep-alive; never a fresh TLS handshake per turn (a handshake is 1–2 RTTs ≈ 50–300 ms — the single biggest mechanical win).
- HTTP/2 multiplexing across parallel subagent streams on the same origin.
- **SSE parsing**: hand-rolled line teeing (codegraff's pattern: tee each line to the TUI *and* the reassembly buffer — deltas paint immediately, history stays transport-agnostic). `eventsource-stream` if we want off-the-shelf.
- DNS caching via **hickory-resolver** (avoid a lookup per request).
- Retry with jitter + soft-retry idiom for provider quirks (codegraff's flag-flip-once pattern); cancel propagates instantly (sampler aborts stream; tools signalled).

### L2 — Perceived latency (the "it feels fast" budget)

- Stream deltas **directly into the TUI** as they arrive; render loop never blocks on model or tools (Grok's sampler/session-actor separation — already our design).
- Token-count projection computed **incrementally** (cache per entry, `tiktoken-rs`), so the 85%-threshold check is O(new tokens), not O(session).
- Daemon default-on (decided) amortizes startup: config/model catalog/AGENTS.md chains load once per workspace, not per invocation — a bootstrap-router trick (cheap paths finish without loading main) becomes mostly unnecessary but still worth it for `-p` one-shots against the socket.

### L3 — IPC (sub-ms; right-size it)

- The bus + ACP wire: **UDS + length-prefixed JSON-RPC frames** (`jsonrpsee` for types/transport or hand-rolled; `interprocess` crate if we want named-pipe parity on Windows).
- Message volume is tens/sec. Mutex + condvar channels are fine. **Upgrade path only if profiling demands**: SPSC lock-free rings (`rtrb`/`heapless`) in shared memory for high-rate event streams (e.g., trace telemetry to a desktop client). Don't build this now.

### L4 — Session log I/O (ms; correctness-adjacent)

- Append-only writes with **group-commit fsync policy**: buffer per turn, fsync on turn boundary + on `spawn`/`message` entries (audit rule beats throughput); crash-tolerant tail like codegraff's trace ("consumers must tolerate a malformed final tail").
- Load path: `simd-json` for big session files (several× serde_json on MB-scale), `memmap2` for read, `zstd` for archived sessions. Only bother when sessions hit MBs — they will.

### L5 — Subprocess / PTY (ms – s)

- `portable-pty` + process groups (kill the group, not the child — the classic zombie-shell bug), output coalescing before appending to the log (handles §4.2), `futures-concurrency` for tool fan-out, `rayon` for CPU-parallel work (tree-sitter batches).

### L6 — Workspace ops (the one place real CPU work lives)

- **`gix`** (pure-Rust git, no libgit2 C dep — ADR-proven choice in 2025–26 projects, jujutsu-adjacent ecosystem) for worktree management, HEAD/branch reads, status — static-binary-friendly.
- `notify` + **debounce + coalesce** for the file watcher (debouncing beats faster syscalls; the codegraph lesson is never-stale via batching, not raw inotify throughput).
- Search: the ripgrep family — `ignore`, `grep-searcher`, `grep-regex`, plus `memchr`/`aho-corasick`/`bstr` (same author, best-in-class).
- Parsing (P5 substrate): `tree-sitter` + grammar crates in a `rayon` pool.

### L7 — TUI (perceived, again)

- `ratatui` + `crossterm`, delta-only rendering, `vte` for parsing terminal output we echo; PTY-level e2e tests (already P2) catch the regressions that matter (dropped input, stuck spinner, mis-rendered stream).

### L8 — Binary & footprint (one-time)

- rustls + musl static → single ~5–15 MB binary (codegraff's 3.7 MB shows the floor; Rust with tokio lands a bit heavier — irrelevant).
- Cargo profile: `opt-level = 3` release, `lto = "thin"`, `strip = true`, `panic = "abort"` for the daemon/TUI (careful: `abort` + catch_unwind don't mix — keep unwind if we want panic-isolation per worker; we do).
- Memory: per-session worker arenas; session trees as `Arc`-shared immutable entries (append-only makes this natural). Expect 10–50 MB RSS total vs codegraff's 12 MB — fine.

## 4. The modern-Rust crate map (2026-verified)

| Concern | Pick | Why this one |
|---|---|---|
| Async runtime | `tokio` (multi-thread) | Ecosystem default; io_uring runtimes regress at our scale (§2) |
| TLS / HTTP | `rustls` + `hyper` 1.x + `h2` | No OpenSSL link; pooling + multiplexing |
| Provider SSE | hand-rolled tee or `eventsource-stream` | Delta-paint + reassembly (codegraff pattern) |
| Wire protocol | `jsonrpsee` (or hand-rolled JSON-RPC 2.0) | ACP-shaped client wire + bus envelopes |
| IPC | UDS via tokio; `interprocess` if Windows parity | §L3 |
| PTY/subprocess | `portable-pty`, process groups | §L5 |
| Git/worktrees | `gix` | Pure Rust, static-friendly, ADR-proven vs git2 |
| File watching | `notify` + debounce layer | Coalescing > throughput |
| Search | `ignore`/`grep-*`/`memchr`/`bstr` | ripgrep's own building blocks |
| CPU fan-out | `rayon` | tree-sitter/index batches |
| JSON | `serde` + `serde_json`; `simd-json` on hot parse | Session load path |
| Log storage | append + group commit; `memmap2`; `zstd` archives | §L4 |
| Tokenizer | `tiktoken-rs` (incremental, cached per entry) | Threshold checks O(new) |
| Caches | `moka` (in-mem, TTL) | Model catalogs, resolved workspaces |
| TUI | `ratatui` + `crossterm`; `vte` | §L7 |
| Observability | `tracing` + `tracing-appender` → the per-run JSONL trace | Self-readable trace (codegraff) from day one |
| Extensions (later) | `wasmtime` component model + `wit-bindgen` | **Answers §9's "no dynamic TS extensions" gap**: sandboxed, polyglot, statically-linkable; component API stable, tooling (`wkg`) maturing |
| Substrate stores (P5) | `redb` (KV), `tantivy` (FTS), `petgraph` (graph) | Only when the index ships |
| Sandbox (v2) | `landlock` crate; `sandbox-exec` on macOS | Grok/codegraff pattern |
| Bench/profiling | `criterion` benches; `flamegraph`/`perf`; `heaptrack` | §5 discipline |

## 5. Discipline: no optimization without a trace

- The per-run JSONL trace (P0) records per-call latency, bytes, tokens, cache read/write — it *is* the profiler for everything that matters at the system level.
- Micro-benches (`criterion`) only for admitted hot paths: SSE parse, session load, token projection.
- `flamegraph` when a trace shows something weird; `heaptrack` before any memory work.
- Steal codegraff's tier-1 CI: fmt, file-size ceiling, test-count-never-shrinks, SDK drift, PTY probes. Their eval rig's "honest list$" (cost recomputed at list rates) is the performance dashboard that actually reflects user value.

## 6. Kernel investigation (parked decision → research note)

What "persistent kernel à la Prime" means when the host is Rust, in ascending order of ambition:
1. **Resident shell session** — one long-lived PTY per session worker (cwd/env persist across tool calls). Cheap, obviously useful, ~no design risk. Could land as early as P1.
2. **Resident script worker** — a child process (Python today, like Prime; or `rquickjs`/`deno_core` if we want in-process) over stdio with a **typed host-request bridge** (credentials, provider calls, transcript writes stay in the host). This is exactly our bus/`spawn` seam — the design already reserves the slot.
3. **WASM kernel** — the `wasmtime` component model as the resident programmatic surface: state persists in the instance, tools are host functions, sandboxing is real rather than advisory. Also the extension mechanism from §4. Investigate (2) and (3) together when compaction pain shows up.

Recommendation: build P0–P2 with none of this; revisit after the first long session where the compaction brief visibly loses working state. That's the signal Prime optimized for, and it's measurable.
