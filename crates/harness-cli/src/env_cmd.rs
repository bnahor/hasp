//! `harness env …`: list environments, check one end to end, and build the
//! guest agent that runs inside them.

use anyhow::{Context, Result, bail};
use harness_core::{ArtifactStore, Gate, GateDecision, ToolCtx};
use harness_env::{Environment, guest, spec};
use harness_workspace::Workspace;
use serde_json::json;

pub async fn run(argv: &[String]) -> Result<()> {
    let ws = Workspace::resolve(&std::env::current_dir()?)?;
    match argv.first().map(String::as_str) {
        None | Some("list") => list(&ws),
        Some("check") => {
            let name = argv.get(1).context("usage: harness env check <name>")?;
            check(&ws, name).await
        }
        Some("build-guest") => build_guest(&argv[1..]).await,
        Some("prune") => prune().await,
        Some(other) => bail!(
            "unknown `env` command {other:?} — harness env [list | check <name> | prune | build-guest [--arch amd64|arm64|all]]"
        ),
    }
}

fn list(ws: &Workspace) -> Result<()> {
    let envs = spec::list(ws.root());
    if envs.is_empty() {
        println!(
            "no environments — add one as .harness/environments/<name>.toml, e.g.\n\n  \
             image = \"python:3.12-slim\"\n  cpus = 2\n  memory = \"4G\"\n  network = \"none\""
        );
        return Ok(());
    }
    for (name, spec) in envs {
        match spec {
            Ok(s) => println!("{name:<16} {}", s.summary()),
            Err(e) => println!("{name:<16} INVALID: {e:#}"),
        }
    }
    for arch in ["amd64", "arm64"] {
        let state = match guest::binary_for(arch) {
            Ok(p) => p.display().to_string(),
            Err(_) => "not built (harness env build-guest)".into(),
        };
        println!("guest linux/{arch}: {state}");
    }
    Ok(())
}

/// Allows everything: `check` runs its own fixed probe commands.
struct AllowAll;
impl Gate for AllowAll {
    fn check_bash(&mut self, _: &str) -> GateDecision {
        GateDecision::Allow
    }
    fn check_write(&mut self, _: &str) -> GateDecision {
        GateDecision::Allow
    }
}

/// Start the environment and report what is actually enforced inside it,
/// through the same path a session uses (the guest agent), then tear it down.
async fn check(ws: &Workspace, name: &str) -> Result<()> {
    let spec = spec::load(ws.root(), name)?;
    println!("{name}: {}", spec.summary());
    let t = std::time::Instant::now();
    let env = Environment::start(spec, ws.root()).await?;
    println!(
        "  started {} in {:.1}s",
        env.id(),
        t.elapsed().as_secs_f64()
    );

    // Limits as the kernel sees them (cgroup v2), not as requested.
    let probe = "echo \"arch    $(uname -m)\"; \
         echo \"cpus    $(cat /sys/fs/cgroup/cpu.max 2>/dev/null || echo ?) (quota period)\"; \
         echo \"memory  $(cat /sys/fs/cgroup/memory.max 2>/dev/null || echo ?)\"; \
         echo \"pids    $(cat /sys/fs/cgroup/pids.max 2>/dev/null || echo ?)\"; \
         out=$(awk 'NR>1 && $2==\"00000000\" {printf \" default route via %s\", $1}' /proc/net/route 2>/dev/null); \
         echo \"network${out:- none (no route out)}\"";
    let (_, out) = env.exec(probe).await?;
    for line in out.lines() {
        println!("  {line}");
    }

    // A real tool call through the guest agent, as a session would make it.
    let tools = env.tools();
    let bash = tools
        .iter()
        .find(|t| t.spec().name == "bash")
        .context("the guest agent offers no bash tool")?;
    let artifacts = ArtifactStore::new(
        std::env::temp_dir().join(format!("harness-env-check-{}", std::process::id())),
        "check".into(),
    )?;
    let mut gate = AllowAll;
    let mut ctx = ToolCtx {
        workspace: ws,
        gate: &mut gate,
        artifacts: &artifacts,
        bus: None,
    };
    let out = bash
        .execute(json!({"command": "pwd && ls | head -5"}), &mut ctx)
        .await;
    println!(
        "  guest agent: {} tool(s); bash in the workspace → {}",
        tools.len(),
        if out.is_error { "FAILED" } else { "ok" }
    );
    for line in out.content.lines().take(7) {
        println!("    {line}");
    }
    drop(tools);
    drop(env);
    println!("  stopped");
    Ok(())
}

/// Build the static Linux guest agent from the checkout, in a Rust container,
/// into `~/.harness/guest/`. Native-architecture builds take about a minute;
/// the other architecture runs emulated and takes longer.
async fn build_guest(args: &[String]) -> Result<()> {
    let native = {
        let out = tokio::process::Command::new("docker")
            .args(["info", "--format", "{{.Architecture}}"])
            .output()
            .await
            .context("running docker")?;
        if !out.status.success() {
            bail!("Docker is not running");
        }
        guest::normalize_arch(String::from_utf8_lossy(&out.stdout).trim())
            .context("unsupported Docker architecture")?
    };
    let arches: Vec<&str> = match args {
        [] => vec![native],
        [flag, a] if flag == "--arch" && a == "all" => vec!["amd64", "arm64"],
        [flag, a] if flag == "--arch" => {
            vec![guest::normalize_arch(a).with_context(|| format!("unknown architecture {a}"))?]
        }
        _ => bail!("usage: harness env build-guest [--arch amd64|arm64|all]"),
    };
    let repo = crate::find_checkout()?;
    let out_dir = guest::guest_dir()?;
    std::fs::create_dir_all(&out_dir)?;
    for arch in arches {
        let target = match arch {
            "amd64" => "x86_64-unknown-linux-musl",
            _ => "aarch64-unknown-linux-musl",
        };
        println!(
            "building the guest agent for linux/{arch} from {}…",
            repo.display()
        );
        let t = std::time::Instant::now();
        let script = format!(
            "set -e; apt-get update -qq >/dev/null; apt-get install -y -qq musl-tools >/dev/null; \
             rustup target add {target} >/dev/null 2>&1; mkdir -p /src; cd /repo; \
             tar --exclude=./target --exclude=./bench -cf - . | tar -xf - -C /src; cd /src; \
             cargo build --release --target {target} -p harness-cli 2>&1 | tail -2; \
             cp /target/{target}/release/harness /out/harness-linux-{arch}"
        );
        let status = tokio::process::Command::new("docker")
            .args([
                "run",
                "--rm",
                "--platform",
                &format!("linux/{arch}"),
                "-v",
                &format!("{}:/repo:ro", repo.display()),
                "-v",
                &format!("harness-guest-cargo-{arch}:/usr/local/cargo/registry"),
                "-v",
                &format!("harness-guest-target-{arch}:/target"),
                "-v",
                &format!("{}:/out", out_dir.display()),
                "-e",
                "CARGO_TARGET_DIR=/target",
                "rust:latest",
                "bash",
                "-c",
                &script,
            ])
            .status()
            .await
            .context("running docker")?;
        if !status.success() {
            bail!("guest build for linux/{arch} failed");
        }
        let path = guest::guest_path(arch)?;
        if guest::elf_arch(&path) != Some(arch) {
            bail!(
                "the build did not produce a linux/{arch} binary at {}",
                path.display()
            );
        }
        println!(
            "  → {} ({:.1} MB, {:.0}s)",
            path.display(),
            std::fs::metadata(&path)?.len() as f64 / 1e6,
            t.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

/// Remove environment containers whose owning harness process is gone — a
/// session that was killed never ran its teardown. Live sessions' (and the
/// daemon's) containers are left alone.
async fn prune() -> Result<()> {
    let out = tokio::process::Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=harness.env",
            "--format",
            "{{.ID}} {{.Label \"harness.pid\"}} {{.Names}}",
        ])
        .output()
        .await
        .context("running docker")?;
    if !out.status.success() {
        bail!("Docker is not running");
    }
    let (mut removed, mut kept) = (0, 0);
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split_whitespace();
        let (Some(id), pid, name) = (parts.next(), parts.next(), parts.next().unwrap_or("?"))
        else {
            continue;
        };
        let owner_alive = pid
            .and_then(|p| p.parse::<u32>().ok())
            .is_some_and(harness_core::process::alive);
        if owner_alive {
            kept += 1;
            continue;
        }
        let _ = tokio::process::Command::new("docker")
            .args(["rm", "-f", id])
            .output()
            .await;
        println!("removed {name} (owner {} gone)", pid.unwrap_or("?"));
        removed += 1;
    }
    println!("{removed} removed, {kept} in use");
    Ok(())
}
