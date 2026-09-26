"""This harness as a FrontierHarness Eval candidate, for Harbor (Terminal-Bench)
and Pier (DeepSWE).

Both runners share one environment API, so one implementation serves both;
`harbor_agent.py` and `pier_agent.py` bind it to each runner's BaseAgent.

Per task: upload the static binary built by `install.sh`, run it one-shot
in the task's working directory, then copy the session journal out as
`harness-session.jsonl` — every model call with its own usage, which the
eval's per-call accounting reads (first-call cache, turn count) — and fill
the runner's AgentContext with the totals.

The model route is the eval's (`moonshot/kimi-k3`, …); the key is whatever
the runtime holds under the provider's variable, which on Runta is a stub the
egress proxy replaces with the real key on the way out.
"""

from __future__ import annotations

import json
import os
import shlex
from pathlib import Path

BINARY = Path(os.environ.get("HARNESS_FH_BINARY", "/work/harness/target/fh/release/harness"))
VERSION_FILE = BINARY.parent / "harness.version"

# eval provider prefix -> (harness --provider, key variable, host)
PROVIDERS = {
    "moonshot": ("moonshot", "MOONSHOT_API_KEY", "api.moonshot.ai"),
}

# The runtime host's CA bundle includes Runta's egress CA; task containers
# get it too, so the agent's TLS to the provider validates.
HOST_CA = Path("/etc/ssl/certs/ca-certificates.crt")
REMOTE_CA = "/tmp/harness-ca-bundle.crt"
REMOTE_BIN = "/usr/local/bin/harness"


def route(model_name: str | None) -> tuple[str, str, str, str]:
    """(harness provider, model id, key variable, host) for an eval route."""
    if not model_name or "/" not in model_name:
        raise ValueError(f"expected a provider/model route, got {model_name!r}")
    prefix, model = model_name.split("/", 1)
    if prefix not in PROVIDERS:
        raise ValueError(f"no harness preset for provider {prefix!r}")
    provider, key_var, host = PROVIDERS[prefix]
    return provider, model, key_var, host


def version() -> str:
    try:
        return VERSION_FILE.read_text().strip()
    except OSError:
        return "unknown"


def usage_from_session(text: str) -> dict:
    """Totals and per-call detail from a session journal."""
    calls = []
    for line in text.splitlines():
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        m = e.get("message") or {}
        u = m.get("usage")
        if e.get("type") == "message" and m.get("role") == "assistant" and u:
            calls.append(u)
    fresh = sum(u.get("input", 0) for u in calls)
    read = sum(u.get("cache_read", 0) for u in calls)
    write = sum(u.get("cache_write", 0) for u in calls)
    out = sum(u.get("output", 0) for u in calls)
    return {
        "calls": len(calls),
        "input_total": fresh + read + write,
        "cache_read": read,
        "cache_write": write,
        "output": out,
    }


class HarnessAgentMixin:
    """Shared by the Harbor and Pier bindings; expects BaseAgent attributes
    (`model_name`, `logs_dir`)."""

    @staticmethod
    def name() -> str:
        return "harness"

    def version(self) -> str | None:
        return version()

    def _env(self, environment, extra: dict[str, str]) -> dict[str, str] | None:
        # Pier adds its egress-proxy settings only through this hook.
        hook = getattr(environment, "agent_process_env", None)
        return hook(extra) if hook else extra

    async def setup(self, environment) -> None:
        arch = (await environment.exec(command="uname -m", user="root")).stdout.strip()
        if arch != "x86_64":
            raise RuntimeError(f"harness build is x86_64 only; task is {arch}")
        await environment.upload_file(BINARY, REMOTE_BIN)
        await environment.exec(command=f"chmod 755 {REMOTE_BIN}", user="root")
        if HOST_CA.exists():
            await environment.upload_file(HOST_CA, REMOTE_CA)
            await environment.exec(command=f"chmod 644 {REMOTE_CA}", user="root")

    async def run(self, instruction: str, environment, context) -> None:
        provider, model, key_var, _ = route(self.model_name)
        env = {key_var: os.environ.get(key_var, "")}
        if HOST_CA.exists():
            env["SSL_CERT_FILE"] = REMOTE_CA
        logs = Path(self.logs_dir)
        logs.mkdir(parents=True, exist_ok=True)
        command = (
            f"{REMOTE_BIN} --provider {provider} --model {shlex.quote(model)} "
            f"--no-daemon --new --yolo -p {shlex.quote(instruction)}"
        )
        result = await environment.exec(command=command, env=self._env(environment, env))
        (logs / "harness.stdout").write_text(result.stdout or "")
        (logs / "harness.stderr").write_text(result.stderr or "")

        # The journal of the session this run created: the newest one.
        found = await environment.exec(
            command='ls -t "$HOME"/.harness/sessions/*/*.jsonl 2>/dev/null | head -1'
        )
        journal = (found.stdout or "").strip()
        if journal:
            target = logs / "harness-session.jsonl"
            await environment.download_file(journal, target)
            u = usage_from_session(target.read_text())
            context.n_input_tokens = u["input_total"]
            context.n_cache_tokens = u["cache_read"]
            context.n_output_tokens = u["output"]
        if result.return_code != 0:
            raise RuntimeError(
                f"harness exited {result.return_code}: {(result.stderr or '')[-2000:]}"
            )
