# P10 — benchmarks

Status: COMPLETE (2026-09-20) — results in docs/benchmarks.md

Headline: harness overhead is 39 ms per one-shot turn (mock provider, network
excluded) and unmeasurable against provider variance; the full 164-problem
HumanEval suite through `hasp -p` on deepseek-v4-pro scored **95.7% pass@1**
for **$0.78**, p50 latency 6.1 s per problem.

## Two questions

1. **What does the harness itself cost?** Startup, one-shot turnaround, daemon
   round trips, journal load and replay, compaction, MCP bridge latency —
   measured with the mock provider so the network is out of the picture.
2. **How does the whole system score on a public benchmark?** HumanEval (164
   problems, OpenAI, MIT) run through `hasp -p` against DeepSeek, evaluated
   locally against the canonical tests — plus a direct-to-API baseline on a
   subset so the harness's marginal cost (latency and tokens) is visible.

## Why HumanEval and not SWE-bench

SWE-bench Verified needs Docker, hours of wall clock and a per-instance budget
in dollars; Terminal-Bench needs its own runner. HumanEval is a genuine
open-source standard, runs locally in minutes, and exercises the exact path a
one-shot user hits: prompt → provider → streamed reply → journal. Its limits are
recorded, not hidden: it measures single-turn code generation, not agentic tool
use.

## Method

- Micro: `target/release/hasp`, mock provider, 20-50 repetitions, medians.
  Machine and rustc recorded in the results file.
- HumanEval: fresh temp workspace per problem (no `AGENTS.md`, so the system
  prompt is identical everywhere), `--new`, one-shot, 6 workers, 150 s per
  problem; candidate extracted from the last fenced block; judged by running the
  problem's own `test` code against the candidate in a sandboxed subprocess with
  a 10 s timeout. Pass@1. Cost and per-problem latency from the harness's own
  turn stats.
- Baseline: same prompts, same model, direct API calls with no harness, 25
  problems — the delta is what the harness adds.

## What will not be claimed

No comparison to other agents' SWE-bench numbers, no inference about agentic
skill from a single-turn suite, and no benchmark run against a provider window
that is rate-limited or exhausted.
