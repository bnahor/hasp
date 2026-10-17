# Benchmarks

Two kinds of measurement, both taken 2026-09-20 on an Apple M5 Pro (52 GB,
macOS 27.0, rustc 1.98.1, release build) unless noted. Reproduce with
`bench/humaneval/`.

## 1. The harness itself (mock provider — network excluded)

| measurement | result |
| --- | --- |
| `hasp --version` process startup | 3 ms median (n=20) |
| one-shot turn end to end, fresh session | **39 ms median** (n=20, min 36 / max 45) |
| same, continuing a 2.1 MB / 4,000-entry journal | **52 ms median** (+13 ms to load, validate and assemble) |
| daemon `session/stats` on that session | 3.1 ms median |
| daemon `session/history` (30-message replay) | 0.2 ms median |
| daemon `session/compact` | 6 ms of mechanics (the real cost is the model's summary tokens) |

Reading: everything the harness adds to a turn — process start, workspace
resolution, session creation, journal write, context assembly, streaming — fits
in ~40 ms. A real turn is dominated entirely by the provider (p50 6.1 s below),
so harness overhead is ~0.6% of a typical turn and invisible end to end.

While building the large journal, malformed entries (bad `stop_reason`, missing
`ts`) were rejected with precise errors rather than panicking — a robustness
datum, not just a benchmark one.

## 2. HumanEval through the harness

The full public suite (164 problems, OpenAI, MIT) run as one-shots:
`hasp -p` per problem, fresh temp workspace each (identical system prompt
everywhere), `deepseek-v4-pro` with its default thinking enabled, six workers,
150 s per problem. Candidates extracted from the last fenced block and judged
against each problem's own canonical tests in a sandboxed subprocess (10 s).

| metric | result |
| --- | --- |
| **pass@1** | **157 / 164 = 95.7%** (96.9% of the 162 answered) |
| not answered | 2 (provider timeouts at 150 s: #47, #132) |
| latency per problem | p50 6.1 s · p90 20.6 s · max 103.3 s |
| tokens per problem | ~1,994 in (incl. system prompt) · ~641 out |
| total cost | **$0.78** |

### Baseline: same prompts, direct API, no harness (first 25 problems)

22/25 = 88.0% pass, mean latency 12.3 s — slightly *slower* than the harness
runs' mean, i.e. the harness's marginal latency is below provider variance, in
line with the 39 ms mock measurement. The accuracy difference is not a
controlled comparison (n=25 vs 164, and the direct calls carry no system
prompt) and is reported as such, not claimed as an advantage.

### How 95.7% compares (looked up 2026-09-20, third-party boards)

HumanEval is largely saturated at the top and every leaderboard disagrees with
the others by a few points, so ranges are given, not rankings: the frontier
cluster sits at roughly **93–97.6%** (Claude Sonnet 4.5 reported anywhere from
91% to 97.6% depending on the board; Claude Opus 4.6 ~96.3%; GPT-5/5.5 ~96%;
Gemini 3.5 Flash ~94.6%), while the **median across 208 scored models is ~76%**.
One leaderboard reports **DeepSeek V4 Pro Max at 93.4%** (single source;
DeepSeek's own V4 report publishes HumanEval only for *base* models, 76.8 for
V4-Pro-Base, which is a different evaluation than an API chat model).

So: our 95.7% (n=164, binomial 95% CI ≈ ±3.1 → **92.6–98.8%**) sits inside the
frontier cluster, at or above the one DeepSeek V4 figure found — consistent,
not a victory lap. HumanEval is also widely considered contaminated (in most
frontier models' training data) and barely differentiates top models any more.

The honest attribution: **the score is the model's, not the harness's** — that
is the desired property of a thin harness (39 ms of overhead, baseline parity in
latency). Where harnesses actually differentiate from each other is agentic
suites like SWE-bench Verified or Terminal-Bench, which this project has not
run.

One comparison we *can* make from our own token counts: at Sonnet 4.5's listed
pricing ($3/$15 per M) the same 164 problems would cost ≈ **$2.56 in+out**,
versus the **$0.78** this run cost on DeepSeek — about 3.3×.

### What this does and does not measure

It measures the exact path a one-shot user hits — prompt, provider request,
streamed reply, journal — on a public suite. It does not measure agentic tool
use, long sessions, or compaction quality. SWE-bench Verified needs Docker and
hours per instance and was deliberately out of scope; these numbers say nothing
about SWE-bench.
