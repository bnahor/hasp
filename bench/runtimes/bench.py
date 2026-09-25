"""Runtime benchmark: the same agent workload through harness environments
under runc (shared kernel), runsc (gVisor) and kata-clh (microVM).

Every workload runs as a `bash` tool call through the guest agent, exactly as
an agent session would; each command also times itself inside the
environment, so runtime cost and harness RPC overhead are separable.
"""
import json, os, re, statistics, subprocess, sys, time
from pathlib import Path

RUNTIMES = sys.argv[1].split(",") if len(sys.argv) > 1 else ["runc", "runsc", "kata-clh"]
REPS = int(sys.argv[2]) if len(sys.argv) > 2 else 3
ROOT = Path.home() / "rtb"
OUT = ROOT / "results.jsonl"

def timed(label, cmd):
    # Timed inside the environment; output discarded so only the work counts.
    return (f"s=$(date +%s%N); ( {cmd} ) >/tmp/{label}.out 2>&1; rc=$?; e=$(date +%s%N); "
            f"echo \"T {label} $(( (e-s)/1000000 )) rc=$rc\"")

WORKLOADS = [
    ("cpu", "python3 -c 'n=0\nfor i in range(2,300000):\n  n+=all(i%d for d in range(2,int(i**.5)+1))\nprint(n)'"),
    ("compile", "gcc -O1 -c /opt/sqlite/sqlite3.c -o /tmp/s.o"),
    ("tests_rootfs", "cd /opt/sqlglot && python -m pytest -q -p no:cacheprovider tests"),
    ("tests_workspace", "python -m pytest -q -p no:cacheprovider tests"),
    ("files_rootfs", "mkdir -p /tmp/f && cd /tmp/f && for i in $(seq 1 5000); do echo $i > f$i; done && cat f* >/dev/null && cd / && rm -rf /tmp/f"),
    ("files_workspace", "mkdir -p f && cd f && for i in $(seq 1 5000); do echo $i > f$i; done && cat f* >/dev/null && cd .. && rm -rf f"),
    ("git_workspace", "git status --porcelain >/dev/null && git grep -c 'def ' >/dev/null && grep -rn 'class ' --include=*.py . >/dev/null"),
]

def workspace(rt):
    ws = ROOT / f"ws-{rt}"
    if not ws.exists():
        cid = subprocess.run(["docker", "create", "rtbench:1"], capture_output=True, text=True).stdout.strip()
        subprocess.run(["docker", "cp", f"{cid}:/opt/sqlglot", str(ws)], check=True)
        subprocess.run(["docker", "rm", cid], capture_output=True)
    envdir = ws / ".harness" / "environments"
    envdir.mkdir(parents=True, exist_ok=True)
    spec = 'image = "rtbench:1"\ncpus = 2\nmemory = "4g"\n'
    if rt != "runc":
        spec += f'runtime = "{rt}"\n'
    (envdir / "b.toml").write_text(spec)
    return ws

def session(rt, script):
    ws = workspace(rt)
    t0 = time.time()
    p = subprocess.run(["harness", "--provider", "mock", "--env", "b", "-p", "go", "--yolo"],
                       cwd=ws, capture_output=True, text=True,
                       env=dict(os.environ, HARNESS_MOCK_SCRIPT=json.dumps(script)))
    return time.time() - t0, p.stdout + p.stderr

def run_rep(rt, rep):
    # Startup/teardown: a session with no tool calls.
    wall, out = session(rt, [{"text": "hi"}])
    assert "▪ environment b:" in out, out[-2000:]
    rec = {"runtime": rt, "rep": rep, "session_no_tools_s": round(wall, 2)}
    # Workloads: one session, one bash call each.
    script = [{"tool": "bash", "input": {"command": timed(n, c), "timeout_ms": 600000}} for n, c in WORKLOADS]
    script.append({"tool": "grep", "input": {"pattern": "def parse", "output_mode": "count"}})
    script.append({"text": "done"})
    wall, out = session(rt, script)
    rec["session_workloads_s"] = round(wall, 2)
    for label, ms, rc in re.findall(r"T (\w+) (\d+) rc=(\d+)", out):
        rec[label] = int(ms); rec[label + "_rc"] = int(rc)
    tools = re.findall(r"^✓ (\w+): .*\((\d+) ms\)$", out, re.M)
    rec["tool_ms"] = [int(ms) for _, ms in tools]
    grep_ms = [int(ms) for name, ms in tools if name == "grep"]
    rec["grep_tool"] = grep_ms[0] if grep_ms else None
    rec["failures"] = [l for l in re.findall(r"^✗ .*$", out, re.M)]
    return rec

def memory_density(rt, n=5):
    """Memory the VM loses per idle environment-sized container."""
    def avail():
        return int(re.search(r"MemAvailable:\s+(\d+)", open("/proc/meminfo").read()).group(1)) // 1024
    subprocess.run("sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null", shell=True)
    before = avail()
    flag = [] if rt == "runc" else ["--runtime", rt]
    ids = []
    t0 = time.time()
    for _ in range(n):
        r = subprocess.run(["docker", "run", "-d", *flag, "--cpus", "1", "--memory", "512m",
                            "rtbench:1", "sleep", "infinity"], capture_output=True, text=True)
        if r.returncode == 0:
            ids.append(r.stdout.strip())
    start_s = time.time() - t0
    time.sleep(15)
    after = avail()
    subprocess.run(["docker", "rm", "-f", *ids], capture_output=True)
    return {"runtime": rt, "density_n": len(ids), "mem_per_instance_mb": round((before - after) / max(len(ids), 1), 1),
            "start_n_s": round(start_s, 2)}

if __name__ == "__main__":
    with OUT.open("a") as fh:
        for rep in range(REPS):
            order = RUNTIMES[rep % len(RUNTIMES):] + RUNTIMES[:rep % len(RUNTIMES)]
            for rt in order:
                rec = run_rep(rt, rep)
                fh.write(json.dumps(rec) + "\n"); fh.flush()
                print(json.dumps({k: v for k, v in rec.items() if k != "tool_ms"}), flush=True)
        for rt in RUNTIMES:
            rec = memory_density(rt)
            fh.write(json.dumps(rec) + "\n"); fh.flush()
            print(json.dumps(rec), flush=True)
