#!/bin/sh
# One trial of this harness, for run-trials.sh --cmd:
#   --cmd 'sh /work/harness/bench/frontierharness/run.sh {suite} {task} {model} {jobs}'
# Same runner flags as the skill's default templates; only the agent differs.
set -eu
suite=$1 task=$2 model=$3 jobs=$4
export PYTHONPATH="/work/harness/bench/frontierharness${PYTHONPATH:+:$PYTHONPATH}"
case "$suite" in
  terminal-bench)
    exec harbor run -d terminal-bench@2.0 -i "$task" -a harbor_agent:HarnessAgent \
      -m "$model" --jobs-dir "$jobs" --extra-docker-compose /work/runta-ca-overlay.yaml -r 2 -y ;;
  datacurve)
    exec pier run -p "/work/deep-swe/tasks/$task" --agent-import-path pier_agent:HarnessAgent \
      --model "$model" --jobs-dir "$jobs" ;;
  *) echo "unknown suite: $suite" >&2; exit 2 ;;
esac
