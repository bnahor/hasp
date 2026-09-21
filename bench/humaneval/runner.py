"""Run HumanEval problems through the harness, one-shot per problem.

Usage: python3 bench/humaneval/runner.py <first> <last> [workers]
Dataset: /tmp/bench/humaneval.jsonl (see README.md). Results append resumably
to /tmp/he/results.jsonl; one unparsable stats line or IO hiccup is recorded as
a failure for that problem instead of aborting the whole batch.
"""

import json
import os
import re
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

H = "target/release/harness"
DATASET = "/tmp/bench/humaneval.jsonl"
OUT = "/tmp/he/results.jsonl"
ENV = dict(os.environ, HOME="/tmp/hehome")


def build_prompt(p):
    return (
        "Complete this Python function. Reply with a single ```python code block "
        f"containing the complete definition of `{p['entry_point']}` (keep the docstring). "
        "No tests, no example calls, no explanation.\n\n" + p["prompt"]
    )


def parse_stats(stderr):
    """Tokens/cost from the harness's turn line; absent if the line changed shape."""
    try:
        m = re.search(r"in (\d+) \(cache R (\d+)/W (\d+)\) out (\d+).*?\$([\d.]+)", stderr)
        if m:
            return {
                "in": int(m.group(1)),
                "cache_r": int(m.group(2)),
                "out": int(m.group(4)),
                "cost": float(m.group(5)),
            }
    except (ValueError, AttributeError):
        pass
    return {"in": None, "cache_r": None, "out": None, "cost": None}


def run_one(p):
    tid = p["task_id"].replace("/", "_")
    wd = f"/tmp/he/w/{tid}"
    t0 = time.perf_counter()
    try:
        os.makedirs(wd, exist_ok=True)
        r = subprocess.run(
            [H, "-p", build_prompt(p), "--provider", "deepseek", "--new"],
            cwd=wd,
            env=ENV,
            capture_output=True,
            timeout=150,
        )
    except subprocess.TimeoutExpired:
        return {"task_id": tid, "ok": False, "why": "timeout", "ms": 150000}
    except OSError as e:
        return {"task_id": tid, "ok": False, "why": f"io: {e}", "ms": 0}
    # Recording one problem must not abort the batch; the raw timing is still
    # reported even when the stats line could not be parsed.
    try:
        ms = int((time.perf_counter() - t0) * 1000)
        if r.returncode != 0 or not r.stdout:
            return {
                "task_id": tid,
                "ok": False,
                "why": "harness",
                "err": r.stderr.decode(errors="replace")[-300:],
                "ms": ms,
            }
        return {
            "task_id": tid,
            "ok": True,
            "text": r.stdout.decode(errors="replace"),
            "ms": ms,
            **parse_stats(r.stderr.decode(errors="replace")),
        }
    except (OSError, ValueError) as e:
        return {"task_id": tid, "ok": False, "why": f"record: {e}", "ms": 0}


def load_done():
    done = set()
    if not os.path.exists(OUT):
        return done
    try:
        with open(OUT) as f:
            for line in f:
                try:
                    done.add(json.loads(line)["task_id"])
                except (ValueError, KeyError):
                    continue  # a torn final line is not worth losing a batch over
    except OSError as e:
        # An unreadable results file must stop the run: proceeding would re-run
        # and re-pay every problem already completed.
        sys.exit(f"results unreadable at {OUT} ({e}); inspect or remove it first")
    return done


def main():
    try:
        lo, hi = int(sys.argv[1]), int(sys.argv[2])
        workers = int(sys.argv[3]) if len(sys.argv) > 3 else 6
    except (IndexError, ValueError):
        sys.exit("usage: runner.py <first> <last> [workers]")
    try:
        with open(DATASET) as f:
            problems = [json.loads(line) for line in f]
    except OSError as e:
        sys.exit(f"dataset unreadable at {DATASET} (see README.md): {e}")
    except ValueError as e:
        sys.exit(f"dataset is not clean JSONL ({e}); re-download it")

    done = load_done()
    todo = [p for p in problems[lo:hi] if p["task_id"].replace("/", "_") not in done]
    print(f"{len(todo)} to run ({len(done)} already done)", flush=True)
    try:
        os.makedirs("/tmp/he/w", exist_ok=True)
        with open(OUT, "a") as out, ThreadPoolExecutor(max_workers=workers) as ex:
            for res in ex.map(run_one, todo):
                out.write(json.dumps(res) + "\n")
                out.flush()
                print(
                    f"  {res['task_id']}: "
                    f"{'ok' if res['ok'] else 'FAIL:' + res.get('why', '')} {res['ms']}ms",
                    flush=True,
                )
    except OSError as e:
        sys.exit(f"cannot write results to {OUT} ({e}); nothing was recorded for this batch")
    print("batch complete", flush=True)


if __name__ == "__main__":
    main()
