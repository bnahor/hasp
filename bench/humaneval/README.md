# HumanEval through the harness

Reproduce the numbers in `docs/benchmarks.md`:

```sh
curl -sL -o /tmp/bench/humaneval.jsonl.gz \
  https://raw.githubusercontent.com/openai/human-eval/master/data/HumanEval.jsonl.gz
gunzip -f /tmp/bench/humaneval.jsonl.gz
mkdir -p /tmp/hehome/.harness && cp ~/.harness/credentials.json /tmp/hehome/.harness/
python3 bench/humaneval/runner.py 0 164 6    # ~6 min, ~$0.80 on DeepSeek
python3 bench/humaneval/eval.py              # pass@1, latency, cost
```

`runner.py` writes resumable results to `/tmp/he/results.jsonl` (one-shot per
problem, fresh workspace each so the system prompt is identical everywhere).
`eval.py` extracts the last fenced block and runs the problem's own canonical
tests in a sandboxed subprocess with a 10 s timeout.
