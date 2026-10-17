//! hasp-edge worker | exec | dispatch. Configuration by environment:
//!   HASP_EDGE_NODE           this node's name (pc, mac)
//!   HASP_EDGE_CAPS           comma list (gpu,linux | xcode,macos ...)
//!   THREADLINED_URL          local threadlined (default http://127.0.0.1:8788)
//!   THREADLINED_TOKEN_FILE   its daemon token file (<data>/<swarm>.daemon-token)
//!   MESH_URL / MESH_TOKEN    hub + this node's own node token (~/.config/mesh/config)
//!   HASP_EDGE_HASP           hasp binary (default: `hasp` on PATH)
//!   HASP_EDGE_WORK           where task workspaces live (default ~/edge-work)
//!   HASP_EDGE_HASP_ARGS      extra args for hasp (space separated, e.g. "--provider mock")

use anyhow::{Context, Result};
use hasp_edge::{exec, hub::Hub, threadline::{Grant, Threadline}, worker};
use std::path::PathBuf;

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

fn mesh_config() -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(t) = std::fs::read_to_string(PathBuf::from(home).join(".config/mesh/config")) {
            for l in t.lines() {
                if let Some((k, v)) = l.split_once('=') {
                    m.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
    }
    for (k, v) in std::env::vars().filter(|(k, _)| k.starts_with("MESH_")) {
        m.insert(k, v);
    }
    m
}

fn threadline() -> Result<Threadline> {
    let file = std::env::var("THREADLINED_TOKEN_FILE").context("THREADLINED_TOKEN_FILE (threadlined's <swarm>.daemon-token)")?;
    let token = std::fs::read_to_string(&file).with_context(|| format!("reading {file}"))?;
    Ok(Threadline::new(&env("THREADLINED_URL", "http://127.0.0.1:8788"), token.trim()))
}

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let node = env("HASP_EDGE_NODE", "");
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    match args.first().map(String::as_str) {
        Some("worker") => {
            let mesh = mesh_config();
            let hub = Hub::new(mesh.get("MESH_URL").context("MESH_URL")?, mesh.get("MESH_TOKEN").map(String::as_str).unwrap_or(""));
            let cfg = worker::Config {
                node: node.clone(),
                caps: env("HASP_EDGE_CAPS", "").split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
                ttl_s: env("HASP_EDGE_TTL", "60").parse()?,
                max_held: env("HASP_EDGE_MAX", "2").parse()?,
                state: home.join(".hasp/edge/held.json"),
            };
            let tl = threadline()?;
            let every = std::time::Duration::from_millis(env("HASP_EDGE_TICK_MS", "2000").parse()?);
            let once = args.iter().any(|a| a == "--once");
            loop {
                match worker::tick(&cfg, &tl, &hub).await {
                    Ok(log) => log.iter().for_each(|l| println!("[{node}] {l}")),
                    Err(e) => eprintln!("[{node}] tick failed: {e:#}"),
                }
                if once {
                    return Ok(());
                }
                tokio::time::sleep(every).await;
            }
        }
        Some("exec") => {
            let grant = Grant {
                task_id: arg(&args, "--task").context("--task")?,
                lease_id: arg(&args, "--lease").context("--lease")?,
                fencing_token: arg(&args, "--fencing").context("--fencing")?.parse()?,
                client_id: arg(&args, "--client").context("--client")?,
                expires_at: String::new(),
            };
            let a = exec::ExecArgs {
                node,
                grant,
                hasp: PathBuf::from(env("HASP_EDGE_HASP", "hasp")),
                work_root: std::env::var("HASP_EDGE_WORK").map(PathBuf::from).unwrap_or_else(|_| home.join("edge-work")),
                hasp_args: env("HASP_EDGE_HASP_ARGS", "").split_whitespace().map(String::from).collect(),
            };
            let o = exec::run(&threadline()?, &a).await?;
            println!("{o:?}");
            std::process::exit(exec::exit_code(&o));
        }
        Some("dispatch") => {
            // hasp-edge dispatch "title" "description" [needs:gpu node:pc ...]
            let tl = threadline()?;
            let client = tl.register_client("hasp-edge", "dispatch").await?;
            let mut labels: Vec<String> = args.iter().skip(3).cloned().collect();
            labels.push("edge".into());
            let id = tl.create_task(&client, args.get(1).context("title")?, args.get(2).map(String::as_str).unwrap_or(""), &labels).await?;
            println!("{id}");
        }
        _ => anyhow::bail!("usage: hasp-edge worker [--once] | exec --task T --lease L --fencing N --client C | dispatch TITLE DESC [labels…]"),
    }
    Ok(())
}
