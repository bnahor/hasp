//! hasp-gateway run | mcp | link-code | owner-token | agent-token | status
//!
//! Home: $HASP_GATEWAY_HOME (default ~/.hasp-gateway): config.json, gw.db,
//! agent.token. Credentials live in their own 0600 files referenced by path
//! from config.json; none are inline, none are in any agent workspace, and
//! none reach a model. See docs/gateway.md for the config reference.

use anyhow::{Context, Result, bail};
use hasp_gateway::adapters::{Role, signal::Signal, telegram::Telegram, web::Web, whatsapp::WhatsApp};
use hasp_gateway::backend::HaspDaemon;
use hasp_gateway::gateway::{Config, Gateway};
use hasp_gateway::inbox::{Action, Rule};
use hasp_gateway::leader::{Leader, Solo, ThreadlineLeader};
use hasp_gateway::people::OWNER;
use hasp_gateway::store::Store;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn home() -> PathBuf {
    std::env::var("HASP_GATEWAY_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".hasp-gateway")
    })
}

fn secret(path: &str) -> Result<String> {
    let p = Path::new(path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(p).with_context(|| format!("credential file {path}"))?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!("{path} must be 0600 (it holds a credential)");
        }
    }
    Ok(std::fs::read_to_string(p)?.trim().to_string())
}

fn write_secret(path: &Path, value: &str) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, value)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Refuse inline secrets: they end up in backups, diffs and screenshots.
pub fn check_no_inline_secrets(v: &Value, path: &str) -> Result<()> {
    if let Value::Object(m) = v {
        for (k, x) in m {
            if (k == "token" || k == "password" || k.ends_with("_key")) && x.is_string() {
                bail!("config.json {path}.{k}: put credentials in a 0600 file and use {k}_file");
            }
            check_no_inline_secrets(x, &format!("{path}.{k}"))?;
        }
    }
    Ok(())
}

fn load_config() -> Result<Value> {
    let p = home().join("config.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?)?;
    check_no_inline_secrets(&v, "")?;
    Ok(v)
}

fn store() -> Result<Store> {
    let s = Store::open(&home().join("gw.db"))?;
    s.ensure_owner("owner")?;
    Ok(s)
}

fn offline_gateway(s: Store) -> Gateway {
    Gateway::new(s, Arc::new(HaspDaemon { socket: PathBuf::new(), root: PathBuf::new() }), Arc::new(Solo), Config {
        approval_ttl_ms: 0, hasp_home: PathBuf::new(), send_to_others: vec![], notify_per_hour: 0,
    })
}

fn make_leader(cfg: &Value) -> Result<Arc<dyn Leader>> {
    Ok(match cfg["leader"]["kind"].as_str() {
        Some("threadline") => {
            let l = &cfg["leader"];
            let tl = hasp_edge::threadline::Threadline::new(
                l["threadlined_url"].as_str().unwrap_or("http://127.0.0.1:8788"),
                &secret(l["token_file"].as_str().context("leader.token_file")?)?,
            );
            Arc::new(ThreadlineLeader::new(tl, l["ttl_s"].as_u64().unwrap_or(30), l["name"].as_str().unwrap_or("mac")))
        }
        _ => Arc::new(Solo),
    })
}

/// Consistent copy of the live database (SQLite `VACUUM INTO`), written next
/// to `dest` and renamed into place, so a standby never reads a half copy.
fn snapshot(s: &Store, dest: &Path) -> Result<()> {
    let tmp = dest.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    s.conn().execute("VACUUM INTO ?1", [tmp.to_string_lossy().to_string()])?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// A standby taking over: start from the leader's last snapshot when it is
/// newer than our own database. Anything the old leader did after that
/// snapshot is unknown here: platform cursors may replay a few events (most
/// are caught by event-id dedup if they were in the snapshot), and sends it
/// had in flight are its own (they show as uncertain only on its side).
fn restore(from: &Path) -> Result<bool> {
    let db = home().join("gw.db");
    let newer = match (std::fs::metadata(from).and_then(|m| m.modified()), std::fs::metadata(&db).and_then(|m| m.modified())) {
        (Ok(a), Ok(b)) => a > b,
        (Ok(_), Err(_)) => true,
        _ => false,
    };
    if !newer {
        return Ok(false);
    }
    std::fs::create_dir_all(home())?;
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(home().join(format!("gw.db{suffix}")));
    }
    let tmp = home().join("gw.db.restore");
    std::fs::copy(from, &tmp)?;
    std::fs::rename(&tmp, &db)?;
    Ok(true)
}

async fn build(cfg: &Value, leader: Arc<dyn Leader>) -> Result<Arc<Gateway>> {
    let s = store()?;
    let user_home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let root = cfg["hasp"]["workspace_root"].as_str().map(PathBuf::from).unwrap_or_else(|| home().join("sessions"));
    s.set_meta("workspace_root", &root.to_string_lossy())?;
    let backend = Arc::new(HaspDaemon {
        socket: cfg["hasp"]["socket"].as_str().map(PathBuf::from).unwrap_or_else(|| user_home.join(".hasp/daemon.sock")),
        root,
    });
    let mut gw = Gateway::new(s.clone(), backend, leader.clone(), Config {
        approval_ttl_ms: cfg["approval_ttl_min"].as_i64().unwrap_or(30) * 60_000,
        hasp_home: user_home.join(".hasp"),
        send_to_others: cfg["send_to_others"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
        notify_per_hour: cfg["notify_per_hour"].as_i64().unwrap_or(12),
    });
    let a = &cfg["adapters"];
    if let Some(t) = a.get("telegram") {
        let tg = Telegram::new(t["base_url"].as_str().unwrap_or("https://api.telegram.org"), &secret(t["token_file"].as_str().context("telegram.token_file")?)?, 25);
        tg.start().await?; // deleteWebhook: getUpdates and webhooks are exclusive
        gw.add_adapter(Arc::new(tg));
    }
    if let Some(sg) = a.get("signal") {
        let role = if sg["role"] == "inbox" { Role::Inbox } else { Role::Bot };
        gw.add_adapter(Arc::new(Signal::new(sg["url"].as_str().unwrap_or("http://127.0.0.1:8080"), sg["account_uuid"].as_str().context("signal.account_uuid")?, role, 20_000)));
    }
    if let Some(w) = a.get("whatsapp") {
        let jids = w["self_jids"].as_array().map(|j| j.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        gw.add_adapter(Arc::new(WhatsApp::new(w["url"].as_str().unwrap_or("http://127.0.0.1:8790"), &secret(w["token_file"].as_str().context("whatsapp.token_file")?)?, jids, 25)));
    }
    gw.add_adapter(Arc::new(Web::new(s.clone())));
    s.link_surface("web", hasp_gateway::adapters::web::WEB_SENDER, "owner-token")?;
    if let Some(src) = cfg.get("sources") {
        if let Some(t) = src.get("threadline") {
            let tl = hasp_edge::threadline::Threadline::new(
                t["threadlined_url"].as_str().unwrap_or("http://127.0.0.1:8788"),
                &secret(t["token_file"].as_str().context("sources.threadline.token_file")?)?,
            );
            gw.sources.push(Arc::new(hasp_gateway::sources::ThreadlineTasks { tl, label: t["label"].as_str().unwrap_or("edge").into() }));
        }
        if let Some(m) = src.get("mesh") {
            for source in ["calendar", "reminders"] {
                gw.sources.push(Arc::new(hasp_gateway::sources::MeshContext {
                    url: m["url"].as_str().context("sources.mesh.url")?.into(),
                    token: secret(m["token_file"].as_str().context("sources.mesh.token_file")?)?,
                    node: m["node"].as_str().unwrap_or("mac").into(),
                    source: source.into(),
                    http: reqwest::Client::new(),
                }));
            }
        }
    }
    // Defaults the owner can change: task results and blockers notify,
    // everything else lands in the digest.
    if s.rules().is_empty() {
        for (id, source, kind, action) in [
            ("task-done", "threadline", "task_completed", Action::Notify),
            ("task-blocked", "threadline", "task_blocked", Action::Notify),
            ("inbox-digest", "*", "message", Action::Digest),
        ] {
            s.add_rule(&Rule { id: id.into(), source: source.into(), kind: kind.into(), contains: None, action, quiet_exempt: false, priority: 1 })?;
        }
    }
    Ok(Arc::new(gw))
}

async fn run() -> Result<()> {
    let cfg = load_config()?;
    let leader = make_leader(&cfg)?;
    // Standby: hold no state until we lead (no second writer, no stale poller).
    loop {
        match leader.hold().await {
            Ok(Some(f)) => {
                eprintln!("hasp-gateway: leading (fencing {f})");
                break;
            }
            Ok(None) => {}
            Err(e) => eprintln!("hasp-gateway: lease check failed: {e:#}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    if let Some(from) = cfg["standby"]["restore_from"].as_str()
        && restore(Path::new(from))?
    {
        eprintln!("hasp-gateway: took over from the snapshot at {from}");
    }
    let gw = build(&cfg, leader.clone()).await?;
    gw.startup()?;
    let port = hasp_gateway::web::serve(gw.clone(), cfg["web_port"].as_u64().unwrap_or(8791) as u16).await?;
    eprintln!("hasp-gateway: web view on http://127.0.0.1:{port} (hasp-gateway owner-token, then /login?token=…)");
    let snap_path = cfg["snapshot"]["path"].as_str().map(PathBuf::from);
    let snap_every = cfg["snapshot"]["every_s"].as_i64().unwrap_or(60) * 1000;
    // Renew well inside the TTL (a third of it), or a live leader would lapse between renewals.
    let hold_every = (cfg["leader"]["ttl_s"].as_i64().unwrap_or(30) * 1000 / 3).clamp(500, 5_000);
    let (mut last_hold, mut last_events, mut last_snap) = (hasp_gateway::model::now_ms(), 0i64, 0i64);
    loop {
        let now = hasp_gateway::model::now_ms();
        if now - last_hold > hold_every {
            last_hold = now;
            if !matches!(leader.hold().await, Ok(Some(_))) {
                // Lost the lease: stop at once. The service manager restarts us as a standby.
                eprintln!("hasp-gateway: lost the lease; exiting to standby");
                if let Some(p) = &snap_path {
                    let _ = snapshot(&gw.store, p);
                }
                std::process::exit(3);
            }
        }
        let polls: Vec<_> = gw
            .adapters
            .keys()
            .filter(|k| *k != "web")
            .cloned()
            .map(|name| {
                let gw = gw.clone();
                tokio::spawn(async move {
                    if let Err(e) = gw.poll_adapter(&name).await {
                        eprintln!("poll {name}: {e:#}");
                    }
                })
            })
            .collect();
        for p in polls {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(40), p).await;
        }
        if let Err(e) = gw.route_once().await {
            eprintln!("route: {e:#}");
        }
        let _ = gw.run_turns(); // turns run in the background; delivery continues meanwhile
        if let Err(e) = gw.deliver_once().await {
            eprintln!("deliver: {e:#}");
        }
        let _ = gw.timers_once(now);
        if now - last_events > 60_000 {
            last_events = now;
            if let Err(e) = gw.events_once().await {
                eprintln!("events: {e:#}");
            }
        }
        if let Some(p) = &snap_path
            && now - last_snap > snap_every
        {
            last_snap = now;
            if let Err(e) = snapshot(&gw.store, p) {
                eprintln!("snapshot: {e:#}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(cfg["loop_ms"].as_u64().unwrap_or(500))).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => run().await,
        Some("mcp") => {
            let api = std::env::var("HASP_GATEWAY_URL").unwrap_or_else(|_| "http://127.0.0.1:8791".into());
            let file = std::env::var("HASP_GATEWAY_AGENT_TOKEN_FILE").unwrap_or_else(|_| home().join("agent.token").to_string_lossy().into());
            hasp_gateway::mcp::serve(&api, &secret(&file)?).await
        }
        Some("link-code") => {
            // The CLI on the gateway machine is an owner surface.
            let person = args.iter().position(|a| a == "--person").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| OWNER.into());
            let code = store()?.issue_link_code(&person, 15 * 60_000, "cli")?;
            println!("From the account to link (DM the bot / Note to Self / your WhatsApp self chat), send:\n\n  /link {code}\n\nValid 15 minutes, once.");
            Ok(())
        }
        Some("owner-token") => {
            let t = hasp_gateway::web::set_token(&offline_gateway(store()?), "owner")?;
            println!("Owner token (shown once; replaces the previous one): {t}\nOpen http://127.0.0.1:8791/login?token={t}");
            Ok(())
        }
        Some("agent-token") => {
            let t = hasp_gateway::web::set_token(&offline_gateway(store()?), "agent")?;
            let p = home().join("agent.token");
            write_secret(&p, &t)?;
            println!(
                "Agent token written to {} (0600). Add to ~/.hasp/mcp.json:\n{}",
                p.display(),
                serde_json::json!({"servers": {"gateway": {"command": std::env::current_exe()?.to_string_lossy(), "args": ["mcp"]}}})
            );
            Ok(())
        }
        Some("status") => {
            let s = store()?;
            for (what, q) in [
                ("outbox", "SELECT state, COUNT(*) AS n FROM outbox GROUP BY state"),
                ("turns", "SELECT state, COUNT(*) AS n FROM turns GROUP BY state"),
                ("approvals", "SELECT state, COUNT(*) AS n FROM approvals GROUP BY state"),
                ("waiting", "SELECT state, COUNT(*) AS n FROM waiting GROUP BY state"),
            ] {
                println!("{what}: {}", serde_json::Value::Array(s.json_rows(q, &[])?));
            }
            Ok(())
        }
        _ => {
            println!("hasp-gateway run | mcp | link-code [--person ID] | owner-token | agent-token | status");
            Ok(())
        }
    }
}
