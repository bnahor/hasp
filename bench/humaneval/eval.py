"""Judge runner output against HumanEval's canonical tests.

Usage: python3 bench/humaneval/eval.py [results.jsonl]
Extracts the last fenced block from each answer and runs the problem's own
`test` code against it in a sandboxed subprocess (10 s timeout). A candidate
that crashes, hangs or fails any assertion counts as a fail.
"""

import json
import os
import re
import subprocess
import sys

DATASET = "/tmp/bench/humaneval.jsonl"
DEFAULT_RESULTS = "/tmp/he/results.jsonl"


def extract(text):
    blocks = re.findall(r"```(?:python)?\s*\n(.*?)```", text, re.S)
    return blocks[-1] if blocks else text


def evaluate(res, problems):
    p = problems[res["task_id"]]
    prog = f"{extract(res['text'])}\n{p['test']}\ncheck({p['entry_point']})\nprint('PASSED')"
    wd = f"/tmp/he/eval/{res['task_id']}"
    try:
        os.makedirs(wd, exist_ok=True)
        r = subprocess.run(
            [sys.executable, "-c", prog], cwd=wd, capture_output=True, timeout=10
        )
    except subprocess.TimeoutExpired:
        return False, "eval timeout"
    except OSError as e:
        return False, f"io: {e}"
    if b"PASSED" in r.stdout and r.returncode == 0:
        return True, ""
    tail = (r.stderr or r.stdout).decode(errors="replace")[-200:]
    return False, tail


def load_jsonl(path, what):
    try:
        with open(path) as f:
            return [json.loads(line) for line in f]
    except OSError as e:
        sys.exit(f"{what} unreadable at {path}: {e}")
    except ValueError as e:
        sys.exit(f"{what} is not clean JSONL ({e})")


def main():
    problems = {
        p["task_id"].replace("/", "_"): p for p in load_jsonl(DATASET, "dataset")
    }
    results = load_jsonl(sys.argv[1] if len(sys.argv) > 1 else DEFAULT_RESULTS, "results")
    ok = fail = noans = 0
    fails = []
    lat = cost = 0.0
    for res in results:
        lat += res.get("ms") or 0
        cost += res.get("cost") or 0
        if not res.get("ok") or res["task_id"] not in problems:
            noans += 1
            continue
        passed, why = evaluate(res, problems)
        if passed:
            ok += 1
        else:
            fail += 1
            if len(fails) < 6:
                fails.append((res["task_id"], why.strip()[:120]))
    n = len(results)
    print(f"n={n}  answered={n - noans}  pass={ok}  fail={fail}  no-answer={noans}")
    if n:
        print(f"pass@1 = {ok / n:.1%}   (of answered: {ok / max(1, n - noans):.1%})")
    print(f"latency: mean {lat / max(1, n) / 1000:.1f}s   cost: ${cost:.2f}")
    for task, why in fails:
        print(f"  FAIL {task}: {why}")


if __name__ == "__main__":
    main()
