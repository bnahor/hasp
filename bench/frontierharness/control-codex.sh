#!/bin/sh
# Matched control: Codex CLI 0.148.0 (the version pinned in the published
# baseline's metadata/harness-versions.json) on the same model route, runners,
# flags and egress policy as the candidate. Codex speaks the Responses API,
# which Moonshot serves at /v1; the key variable holds Runta's stub and the
# egress proxy supplies the real key.
#   --cmd 'sh /work/harness/bench/frontierharness/control-codex.sh {suite} {task} {model} {jobs}'
set -eu
suite=$1 task=$2 model=$3 jobs=$4
case "$model" in
  moonshot/*) base=https://api.moonshot.ai/v1 key=${MOONSHOT_API_KEY:-} ;;
  *) echo "control wired for moonshot only, got $model" >&2; exit 2 ;;
esac
set -- --ak version=0.148.0 --ae "OPENAI_BASE_URL=$base" --ae "OPENAI_API_KEY=$key"
case "$suite" in
  terminal-bench)
    exec harbor run -d terminal-bench@2.0 -i "$task" -a codex -m "$model" "$@" \
      --jobs-dir "$jobs" --extra-docker-compose /work/runta-ca-overlay.yaml -r 2 -y ;;
  datacurve)
    exec pier run -p "/work/deep-swe/tasks/$task" --agent codex --model "$model" "$@" \
      --jobs-dir "$jobs" ;;
  *) echo "unknown suite: $suite" >&2; exit 2 ;;
esac
