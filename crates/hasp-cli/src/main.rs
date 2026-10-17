use anyhow::{Context, Result, anyhow, bail};
use hasp_core::engine::{Engine, TurnSink};
use hasp_core::session::SessionManager;
use hasp_core::{Sink, Usage};
use hasp_provider::catalog;
use hasp_workspace::Workspace;
use serde_json::json;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use tui::UiEvent;

mod acp;
mod agent_cmd;
mod backend;
mod creds;
mod daemon;
mod env_cmd;
mod gate;
mod highlight;
mod markdown;
mod mcp_server;
mod recall_cmd;
mod residents;
mod review_cmd;
mod theme;
mod tui;
mod web;

use gate::{GateCore, PlainGate, TuiGate};

const DEFAULT_MODEL: &str = "claude-opus-5";

/// `--env <name>`: run this session's tools in a named environment.
static ENV_FLAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The environment this session's tools run in: `--env`, else `HASP_ENV`.
pub(crate) fn session_environment() -> Option<String> {
    ENV_FLAG
        .get()
        .cloned()
        .or_else(|| hasp_workspace::env_var("HASP_ENV").ok())
        .filter(|s| !s.is_empty())
}

struct PrintSink;

impl Sink for PrintSink {
    fn on_delta(&mut self, text: &str) {
        print!("{text}");
        let _ = std::io::stdout().flush();
    }
}

impl TurnSink for PrintSink {
    fn on_note(&mut self, note: &str) {
        eprintln!("\n{note}");
    }

    fn on_tool(&mut self, ev: &hasp_core::engine::ToolEvent) {
        let mark = if ev.is_error { "✗" } else { "✓" };
        let subject = if ev.args_preview.is_empty() {
            String::new()
        } else {
            format!(": {}", ev.args_preview)
        };
        eprintln!("\n{mark} {}{subject} ({} ms)", ev.name, ev.ms);
        for line in ev.content_preview.lines() {
            eprintln!("  {line}");
        }
    }
}

fn cost_note(catalog: &[catalog::ModelInfo], model: &str, usage: &Usage) -> String {
    match catalog::find(catalog, model).and_then(|m| catalog::cost_of(m, usage)) {
        Some(c) => format!(" · ${c:.3}"),
        None => String::new(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("hasp {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        // An environment's main process: keeps its container alive without
        // relying on anything (a `sleep`, a shell) being in the image.
        Some("--guest-idle") => loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        },
        Some("login") => {
            let provider = argv.get(1).map(String::as_str).unwrap_or("anthropic");
            return creds::login_interactive(provider);
        }
        Some("doctor") => return doctor().await,
        Some("agents") => {
            agent_cmd::list();
            return Ok(());
        }
        Some("providers") => {
            println!(
                "{:<12} {:<6} {:<22} endpoint",
                "provider", "key", "default model"
            );
            let anthropic_key = creds::resolve_key("anthropic").is_some();
            println!(
                "{:<12} {:<6} {:<22} https://api.anthropic.com",
                "anthropic",
                if anthropic_key { "yes" } else { "-" },
                hasp_provider::default_model("anthropic")
            );
            for p in hasp_provider::plans::all() {
                println!(
                    "{:<12} {:<6} {:<22} {}   ({})",
                    p.id,
                    if creds::resolve_key(&p.id).is_some() {
                        "yes"
                    } else {
                        "-"
                    },
                    p.default_model,
                    p.base_url,
                    p.label
                );
            }
            println!(
                "\nuse: hasp login <provider>, then hasp --provider <provider> [--model <id>]\n\
                 add your own: ~/.hasp/providers.json\n\
                 subscriptions (Claude, ChatGPT, Gemini, …) through their own CLI: hasp agents"
            );
            return Ok(());
        }
        Some("env") => return env_cmd::run(&argv[1..]).await,
        Some("review") => return review_cmd::run(&argv[1..]).await,
        Some("trust") => {
            let ws = Workspace::resolve(&std::env::current_dir()?)?;
            let root = ws.root();
            if hasp_core::hooks::trust(root)? {
                println!("trusted {} — its .hasp/hooks.json will run", root.display());
            } else {
                println!("{} is already trusted", root.display());
            }
            return Ok(());
        }
        Some("sessions") => return sessions_cmd(&argv[1..]).await,
        Some("app") => return web::app().await,
        Some("web") => {
            let port: u16 = match argv.get(1) {
                Some(p) => p.parse().context("port must be a number")?,
                None => 0,
            };
            return web::serve(port).await;
        }
        Some("models") => {
            let provider = argv
                .get(1)
                .cloned()
                .or_else(|| hasp_workspace::env_var("HASP_PROVIDER").ok())
                .unwrap_or_else(|| "anthropic".into());
            return list_models_cmd(&provider).await;
        }
        Some("update") => {
            if argv.get(1).map(String::as_str) == Some("--models") {
                let models = catalog::fetch_models_dev()
                    .await
                    .map_err(|e| anyhow::anyhow!("fetching models.dev catalog: {e}"))?;
                let priced = models.iter().filter(|m| m.priced()).count();
                let path = catalog::store(&models)?;
                println!(
                    "stored {} models ({priced} priced) → {}",
                    models.len(),
                    path.display()
                );
                return Ok(());
            }
            if argv.len() > 1 {
                bail!(
                    "usage: hasp update (rebuild from the checkout) · hasp update --models (catalog)"
                );
            }
            return self_update().await;
        }
        Some("export") => {
            // The journal already is the portable format; export is stdout plus
            // a choice of which session.
            let ws = Workspace::resolve(&std::env::current_dir()?)?;
            let mut list = SessionManager::list_for(&ws)?;
            list.sort_by(|a, b| b.path.cmp(&a.path));
            let target = match argv.get(1) {
                Some(id) => list
                    .iter()
                    .find(|s| s.id.starts_with(id.as_str()))
                    .with_context(|| format!("no session matching {id}"))?,
                None => list.first().context("no sessions in this workspace")?,
            };
            use std::io::Write;
            std::io::stdout().write_all(&std::fs::read(&target.path)?)?;
            return Ok(());
        }
        Some("import") => {
            let Some(src) = argv.get(1) else {
                bail!("usage: hasp import <file>");
            };
            let src = PathBuf::from(src);
            let content =
                std::fs::read(&src).with_context(|| format!("reading {}", src.display()))?;
            let mut lines = content.split(|b| *b == b'\n').filter(|l| !l.is_empty());
            let header: serde_json::Value =
                serde_json::from_slice(lines.next().context("empty import file")?)
                    .context("first line must be the session header")?;
            if header["type"] != "session" {
                bail!("first line is not a session header");
            }
            for (i, line) in lines.enumerate() {
                serde_json::from_slice::<serde_json::Value>(line)
                    .with_context(|| format!("entry {} is not JSON", i + 2))?;
            }
            // A session belongs to the workspace named in its header; placing
            // it under that workspace's key is what makes `continue` find it.
            let root = header["workspace"]["root"]
                .as_str()
                .context("header has no workspace root")?;
            let key = if std::path::Path::new(root).exists() {
                Workspace::resolve(std::path::Path::new(root))?.session_key()
            } else {
                // From another machine: keep the recorded key shape so it lands
                // where that workspace would live if it existed here.
                root.replace(['/', '\\'], "-")
                    .trim_start_matches('-')
                    .to_string()
            };
            let name = src
                .file_name()
                .context("import needs a file name")?
                .to_string_lossy()
                .to_string();
            if !name.ends_with(".jsonl") {
                bail!("import expects a .jsonl session file");
            }
            let dir = hasp_workspace::sessions_root()?.join(key);
            std::fs::create_dir_all(&dir)?;
            let dest = dir.join(&name);
            if dest.exists() {
                bail!("a session already exists at {}", dest.display());
            }
            std::fs::write(&dest, &content)?;
            // Full parse as the final gate — a file that loads is a session.
            if let Err(e) = SessionManager::open(&dest) {
                std::fs::remove_file(&dest)?;
                bail!("imported file does not load as a session: {e}");
            }
            println!("imported {} → {}", src.display(), dest.display());
            return Ok(());
        }
        Some("worktrees") => {
            let ws = Workspace::resolve(&std::env::current_dir()?)?;
            let args: Vec<&str> = argv.iter().skip(1).map(String::as_str).collect();
            match args.first().copied() {
                None => {
                    let names = ws.child_worktrees();
                    if names.is_empty() {
                        println!("no child worktrees");
                    } else {
                        println!("{:<22} {:<26} state", "worktree", "branch");
                        for name in names {
                            println!(
                                "{:<22} {:<26} {}",
                                name,
                                ws.worktree_branch(&name),
                                if ws.worktree_merged(&name) {
                                    "merged — nothing to lose"
                                } else {
                                    "unmerged — holds work"
                                }
                            );
                        }
                    }
                    println!("\nhasp worktrees prune <name> [--force]");
                }
                Some("prune") => {
                    let force = args.contains(&"--force");
                    let Some(name) = args.iter().skip(1).find(|a| !a.starts_with("--")) else {
                        bail!("usage: hasp worktrees prune <name> [--force]");
                    };
                    if !ws.worktree_merged(name) && !force {
                        bail!(
                            "worktree `{name}` holds unmerged work — inspect it first, or pass \
                             --force to discard the checkout"
                        );
                    }
                    // The branch is deliberately left behind, so even a forced
                    // prune keeps the child's commits recoverable.
                    let branch = ws.worktree_branch(name);
                    ws.remove_worktree(name)?;
                    println!(
                        "removed worktree {name}; branch {branch} kept (drop it with \
                         `git branch -D {branch}` when you are sure)"
                    );
                }
                Some(other) => bail!("unknown worktrees subcommand: {other}"),
            }
            return Ok(());
        }
        Some(cmd @ ("recall" | "memory" | "workflow")) => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            return match cmd {
                "recall" => recall_cmd::recall(&rest),
                "memory" => recall_cmd::memory(&rest),
                _ => recall_cmd::workflow(&rest),
            };
        }
        Some("resident") => {
            let rest: Vec<String> = std::env::args().skip(2).collect();
            return residents::command(&rest);
        }
        Some("shutdown") => {
            let sock = daemon::socket_path()?;
            let mut client = hasp_wire::WireClient::connect(&sock)
                .await
                .context("daemon not running")?;
            client.shutdown().await?;
            println!("daemon stopped");
            return Ok(());
        }
        _ => {}
    }

    let mut prompt: Option<String> = None;
    let mut model = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let mut new_session = false;
    let mut yolo = false;
    let mut plain = false;
    let mut serve = false;
    let mut mcp_serve = false;
    let mut acp_serve = false;
    let mut use_daemon = false;
    let mut no_daemon = false;
    let mut ascii = hasp_workspace::env_var("HASP_ASCII").is_ok();
    let mut provider_flag: Option<String> = None;
    let mut agent_flag: Option<String> = None;
    let mut model_flag_given = false;
    let mut it = argv.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-p" => prompt = Some(it.next().context("-p needs a prompt")?),
            "--model" => {
                model = it.next().context("--model needs a value")?;
                model_flag_given = true;
            }
            "--new" => new_session = true,
            "--yolo" => yolo = true,
            "--plain" => plain = true,
            "--serve" => serve = true,
            "--mcp-serve" => mcp_serve = true,
            "--acp" => acp_serve = true,
            "--daemon" => use_daemon = true,
            "--provider" => provider_flag = Some(it.next().context("--provider needs a value")?),
            "--agent" => agent_flag = Some(it.next().context("--agent needs a name")?),
            // embedded mode escape hatch (default is daemon-backed)
            "--no-daemon" => no_daemon = true,
            "--ascii" => ascii = true,
            "--env" => {
                let name = it.next().context("--env needs an environment name")?;
                let _ = ENV_FLAG.set(name);
            }
            other => bail!(
                "unknown flag: {other} (expected -p, --model, --new, --yolo, --plain, --serve, --daemon, --provider, --agent, --no-daemon, --ascii, --env, --mcp-serve)"
            ),
        }
    }

    if serve {
        return daemon::serve(daemon::socket_path()?).await;
    }
    if mcp_serve {
        return mcp_server::serve().await;
    }
    if acp_serve {
        // Editor-driven: ACP on stdio, our wire protocol to the daemon behind it.
        return acp::serve().await;
    }

    let cwd = std::env::current_dir()?;
    let agent_name = agent_flag.or_else(|| {
        hasp_workspace::env_var("HASP_AGENT")
            .ok()
            .filter(|a| !a.is_empty())
    });
    if let Some(name) = agent_name {
        return run_agent(
            &name,
            &cwd,
            prompt,
            model_flag_given.then_some(model),
            new_session,
            yolo,
            ascii,
        )
        .await;
    }
    if let (Some(p), true) = (&prompt, use_daemon) {
        let sock = daemon::socket_path()?;
        let mut client = hasp_wire::WireClient::connect(&sock)
            .await
            .context("daemon not running — start it with `hasp --serve` (or drop --daemon)")?;
        client.initialize().await?;
        let s = client
            .request(
                hasp_wire::METHOD_SESSION_CONTINUE,
                serde_json::json!({"cwd": cwd}),
            )
            .await?;
        let r = client
            .request(
                hasp_wire::METHOD_SESSION_PROMPT,
                serde_json::json!({"prompt": p}),
            )
            .await?;
        println!("{}", r["text"].as_str().unwrap_or_default());
        eprintln!(
            "\n─ {} sample(s) · {} ms · session {}",
            r["samples"],
            r["ms"],
            s["sessionId"].as_str().unwrap_or("-")
        );
        return Ok(());
    }
    let ws = Workspace::resolve(&cwd)?;
    eprintln!(
        "▪ workspace {} {}{}",
        ws.root().display(),
        ws.identity().branch.as_deref().unwrap_or("(no branch)"),
        ws.identity()
            .head
            .as_deref()
            .map(|h| format!("@{h}"))
            .unwrap_or_default(),
    );

    let session = if new_session {
        SessionManager::create(&ws)?
    } else {
        SessionManager::open_latest(&ws)?
            .unwrap_or_else(|| SessionManager::create(&ws).expect("session dir is writable"))
    };
    if session.repaired_tool_calls > 0 {
        eprintln!(
            "▪ last run was interrupted mid-tool: closed {} unfinished tool call(s)",
            session.repaired_tool_calls
        );
    }

    let provider_name = provider_flag.unwrap_or_else(|| {
        hasp_workspace::env_var("HASP_PROVIDER").unwrap_or_else(|_| "anthropic".into())
    });
    let mock = provider_name == "mock";
    // provider-appropriate default model when neither flag nor env set one;
    // dynamic providers have no static default, so ask the endpoint
    if !model_flag_given && std::env::var("ANTHROPIC_MODEL").is_err() {
        model = if hasp_provider::is_dynamic(&provider_name) {
            dynamic_default_model(&provider_name).await?
        } else {
            hasp_provider::default_model(&provider_name).to_string()
        };
    }
    let api_key: Option<String> = if mock {
        None
    } else {
        match creds::resolve_key(&provider_name) {
            Some(k) => Some(k),
            None => {
                if std::io::stdin().is_terminal() {
                    eprintln!(
                        "no API key found for {provider_name} (env or `hasp login {provider_name}`)"
                    );
                    creds::login_interactive(&provider_name)?;
                    Some(creds::resolve_key(&provider_name).context("still no key after login")?)
                } else {
                    bail!(
                        "no {provider_name} API key: run `hasp login {provider_name}` (or --provider mock)"
                    );
                }
            }
        }
    };
    // Children each need their own provider instance; the root takes one
    // from the same factory so model selection stays consistent.
    let provider_factory: hasp_bus::ProviderFactory = {
        let key = api_key.clone();
        let model_f = model.clone();
        let name = provider_name.clone();
        Arc::new(move || {
            if name == "mock" {
                return Ok(Box::new(hasp_provider::echo::EchoProvider::from_env())
                    as Box<dyn hasp_core::Provider>);
            }
            match hasp_provider::build(&name, key.clone().unwrap_or_default(), model_f.clone()) {
                Some(client) => Ok(client),
                None => Err(format!("unknown provider “{name}”")),
            }
        })
    };

    let catalog = catalog::load();

    // plain REPL and one-shot -p stay embedded
    if plain || prompt.is_some() {
        let engine = embedded_engine_with_bus(
            ws.clone(),
            session,
            &provider_factory,
            model.clone(),
            yolo,
            prompt.is_none(),
            None,
            None,
        )
        .await?;
        let engine = Arc::new(tokio::sync::Mutex::new(engine));
        if let Some(p) = prompt {
            let mut eng = engine.lock().await;
            let mut sink = PrintSink;
            let mut res = eng.run_turn(&p, &mut sink).await;
            while res.is_ok() {
                match eng.post_turn_check(&mut sink).await {
                    Some(cont) => res = eng.run_turn(&cont, &mut sink).await,
                    None => break,
                }
            }
            match res {
                Ok(stats) => eprintln!(
                    "\n─ {} sample(s), {} tool call(s) · in {} (cache R {}/W {}) out {}{} · {} ms",
                    stats.samples,
                    stats.tool_calls,
                    stats.usage.total_in(),
                    stats.usage.cache_read,
                    stats.usage.cache_write,
                    stats.usage.output,
                    cost_note(&catalog, &model, &eng.session.totals()),
                    stats.ms
                ),
                Err(e) => eprintln!("error: {e}"),
            }
            return Ok(());
        }
        return plain_repl(engine, catalog).await;
    }

    // interactive TUI: daemon-backed by default (auto-spawn), embedded with --no-daemon
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (prompt_tx, prompt_rx) = std::sync::mpsc::channel();
    let open_method = if new_session {
        hasp_wire::METHOD_SESSION_NEW
    } else {
        hasp_wire::METHOD_SESSION_CONTINUE
    };
    let mut backend: Box<dyn backend::Backend> = if no_daemon {
        embedded_backend(
            ws,
            session,
            &provider_factory,
            model,
            yolo,
            prompt_tx.clone(),
            event_tx.clone(),
        )
        .await?
    } else {
        let sock = daemon::socket_path()?;
        // forward provider/model selection to the auto-spawned daemon via env
        // (build_provider reads HASP_PROVIDER / ANTHROPIC_MODEL)
        let mut daemon_env: Vec<(String, String)> = vec![
            ("HASP_PROVIDER".into(), provider_name.clone()),
            ("ANTHROPIC_MODEL".into(), model.clone()),
        ];
        // The daemon owns the gate, so it has to know about --yolo; otherwise
        // approvals keep prompting in daemon mode no matter what the client said.
        if yolo {
            daemon_env.push(("HASP_YOLO".into(), "1".into()));
        }
        let wire = async {
            daemon::ensure_daemon(&sock, &daemon_env).await?;
            backend::WireBackend::spawn(
                sock,
                open_method,
                &cwd,
                event_tx.clone(),
                prompt_tx.clone(),
            )
            .await
        }
        .await;
        match wire {
            Ok(b) => Box::new(b),
            Err(e) => {
                eprintln!("▪ daemon unavailable ({e}) — running embedded");
                embedded_backend(
                    ws,
                    session,
                    &provider_factory,
                    model,
                    yolo,
                    prompt_tx,
                    event_tx.clone(),
                )
                .await?
            }
        }
    };
    // Both a fresh open and an adopted resident start with the tail of the
    // session on screen instead of a blank transcript. `call` blocks (so does
    // the whole TUI), hence `block_in_place` — the same reason `tui::run`
    // needs it below.
    let history = tokio::task::block_in_place(|| backend.call("session/history", json!({})));
    if let Ok(h) = history {
        let lines: Vec<(bool, String)> = h["messages"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| {
                        Some((
                            m["role"].as_str()? == "user",
                            m["text"].as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !lines.is_empty() {
            let _ = event_tx.send(UiEvent::Note(format!(
                "▪ showing last {} of {} message(s)",
                lines.len(),
                h["total"].as_u64().unwrap_or(0)
            )));
            let _ = event_tx.send(UiEvent::History(lines));
        }
    }
    let mut terminal = ratatui::init();
    let res = tokio::task::block_in_place(|| {
        tui::run(&mut terminal, backend, catalog, prompt_rx, event_rx, ascii)
    });
    ratatui::restore();
    res
}

/// `--agent NAME`: an external agent on the user's own plan, in the TUI or
/// one-shot with `-p`. It continues the agent's last session in this
/// workspace unless `--new`.
async fn run_agent(
    name: &str,
    cwd: &std::path::Path,
    prompt: Option<String>,
    model: Option<String>,
    new_session: bool,
    yolo: bool,
    ascii: bool,
) -> Result<()> {
    let spec = agent_cmd::spec(name)?;
    let ws = Workspace::resolve(cwd)?;
    let root = ws.root().to_path_buf();
    let resume = (!new_session)
        .then(|| agent_cmd::last_session(&root, name))
        .flatten();
    if let Some(p) = prompt {
        let opts = hasp_agents::Opts {
            cwd: root.clone(),
            model,
            yolo,
            resume,
            ask: agent_cmd::terminal_asker(),
        };
        return agent_cmd::one_shot(&spec, &root, opts, &p).await;
    }
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (prompt_tx, prompt_rx) = std::sync::mpsc::channel();
    let opts = hasp_agents::Opts {
        cwd: root.clone(),
        model,
        yolo,
        resume,
        ask: agent_cmd::tui_asker(prompt_tx),
    };
    eprintln!("▪ starting {name} in {}", root.display());
    let backend =
        agent_cmd::AgentBackend::start(spec, opts, root, ws.identity().branch.clone(), event_tx)
            .await?;
    let mut terminal = ratatui::init();
    let res = tokio::task::block_in_place(|| {
        tui::run(
            &mut terminal,
            Box::new(backend),
            Vec::new(),
            prompt_rx,
            event_rx,
            ascii,
        )
    });
    ratatui::restore();
    res
}

#[allow(clippy::too_many_arguments)]
async fn embedded_engine_with_bus(
    ws: hasp_workspace::Workspace,
    session: SessionManager,
    provider_factory: &hasp_bus::ProviderFactory,
    model: String,
    yolo: bool,
    interactive: bool,
    prompt_tx: Option<std::sync::mpsc::Sender<gate::PromptRequest>>,
    fanout: Option<hasp_bus::NoticeFanout>,
) -> Result<Engine> {
    // Parent gate: the TUI approval channel when present, plain stdin gate
    // otherwise. Child gate factory: approvals route to the same channel;
    // without one (non-interactive), children run yolo — structural
    // confinement still applies (approvals are a trust toggle, confinement
    // is not).
    let (parent_gate, child_gate_factory): (Box<dyn hasp_core::Gate>, hasp_bus::GateFactory) =
        match prompt_tx {
            Some(tx) => {
                let child_tx = std::sync::Mutex::new(tx.clone());
                (
                    Box::new(TuiGate::new(GateCore::new(yolo, true), tx)),
                    Arc::new(move || {
                        Box::new(TuiGate::new(
                            GateCore::new(yolo, true),
                            child_tx.lock().expect("gate channel").clone(),
                        )) as Box<dyn hasp_core::Gate>
                    }),
                )
            }
            None => (
                Box::new(PlainGate {
                    core: GateCore::new(yolo, interactive),
                }),
                Arc::new(|| {
                    Box::new(PlainGate {
                        core: GateCore::new(true, false),
                    }) as Box<dyn hasp_core::Gate>
                }),
            ),
        };
    // MCP servers contribute tools alongside the built-ins; the engine keeps
    // the combined list name-sorted (cache contract).
    let mut tools = hasp_tools::builtins_for(ws.root());
    tools.extend(hasp_mcp::tools_for(&ws).await);
    let env_name = session_environment();
    if let Some(name) = &env_name {
        eprintln!("▪ starting environment {name}…");
        let env = hasp_env::Environment::open(name, ws.root()).await?;
        eprintln!(
            "▪ environment {name}: {} ({})",
            env.spec.summary(),
            env.id()
        );
        tools = hasp_env::with_environment(tools, &env);
    }
    let mut engine = Engine::new(
        ws.clone(),
        session,
        provider_factory().map_err(|e| anyhow::anyhow!("{e}"))?,
        tools,
        parent_gate,
        model.clone(),
        hasp_provider::max_output_for(&model),
    )?;
    let bus = Arc::new(
        hasp_bus::LocalBus::new(
            ws,
            model.clone(),
            hasp_provider::max_output_for(&model),
            provider_factory.clone(),
            child_gate_factory,
            engine.notice_inbox(),
            fanout,
        )
        .with_environment(env_name),
    );
    engine.set_bus(bus);
    Ok(engine)
}

#[allow(clippy::too_many_arguments)]
async fn embedded_backend(
    ws: Workspace,
    session: SessionManager,
    provider_factory: &hasp_bus::ProviderFactory,
    model: String,
    yolo: bool,
    prompt_tx: std::sync::mpsc::Sender<gate::PromptRequest>,
    event_tx: std::sync::mpsc::Sender<tui::UiEvent>,
) -> Result<Box<dyn backend::Backend>> {
    let fanout: hasp_bus::NoticeFanout = {
        let tx = event_tx.clone();
        Arc::new(move |note: &str| {
            let _ = tx.send(tui::UiEvent::Note(note.to_string()));
        })
    };
    let engine = embedded_engine_with_bus(
        ws,
        session,
        provider_factory,
        model,
        yolo,
        true,
        Some(prompt_tx),
        Some(fanout),
    )
    .await?;
    let engine = Arc::new(tokio::sync::Mutex::new(engine));
    let canceller = engine.lock().await.canceller();
    Ok(Box::new(backend::EngineBackend::new(
        engine,
        tokio::runtime::Handle::current(),
        event_tx,
        canceller,
    )))
}

async fn plain_repl(
    engine: Arc<tokio::sync::Mutex<Engine>>,
    catalog: Vec<catalog::ModelInfo>,
) -> Result<()> {
    eprintln!(
        "plain REPL — /exit /new /name /stats /model [id] /compact /branch [id] /fork /worktree [name] /worktree-exit [rm] /goal [text|status|pause|resume|clear] /autonomous [on|off] /resume [n] /help"
    );
    let mut worktree: Option<(Workspace, String)> = None;
    let mut line = String::new();
    loop {
        line.clear();
        print!("▸ ");
        let _ = std::io::stdout().flush();
        if std::io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let input = line.trim();
        match input {
            "" => continue,
            "/exit" | "/quit" => break,
            "/new" => {
                let mut eng = engine.lock().await;
                eng.session = SessionManager::create(&eng.workspace)?;
                eprintln!("▪ new session {}", eng.session.header.id);
            }
            "/stats" => {
                let eng = engine.lock().await;
                let t = eng.session.totals();
                eprintln!(
                    "▪ in {} (cache R {}/W {}) out {}{} · {} entries · ctx ~{}/{}",
                    t.total_in(),
                    t.cache_read,
                    t.cache_write,
                    t.output,
                    cost_note(&catalog, &eng.model, &t),
                    eng.session.entries().len(),
                    eng.context_est_tokens(),
                    eng.provider.context_window(),
                );
            }
            "/help" => eprintln!(
                "▪ /exit /new /name <text> /stats /model [id] /compact /branch [id] /fork /worktree /worktree-exit [rm] /goal <text>|status|pause|resume|clear [--budget N|--unlimited] /autonomous on|off [--max-turns N] [--max-tokens N] [--gate \"cmd\"] /resume [n]"
            ),
            "/compact" => match hasp_core::engine::compact_shared(&*engine).await {
                Ok(cs) => eprintln!(
                    "▪ compacted ~{}k tokens → brief + {} retained message(s)",
                    cs.tokens_before / 1000,
                    cs.retained_messages
                ),
                Err(e) => eprintln!("error: {e}"),
            },
            cmd if cmd == "/goal" || cmd.starts_with("/goal ") => {
                let arg = cmd.trim_start_matches("/goal").trim();
                let mut eng = engine.lock().await;
                match arg {
                    "" | "status" => match eng.session.goal() {
                        Some(g) => eprintln!(
                            "▪ goal{}: {}{}",
                            if g.completed {
                                " (completed)"
                            } else if g.paused {
                                " (paused)"
                            } else {
                                ""
                            },
                            g.text,
                            g.budget_tokens
                                .map(|b| format!(" [budget {b} tokens]"))
                                .unwrap_or_else(|| " [unlimited]".into())
                        ),
                        None => eprintln!("▪ no goal set"),
                    },
                    "pause" => {
                        eng.set_goal("pause", "", None);
                        eprintln!("▪ goal paused");
                    }
                    "resume" => {
                        eng.set_goal("resume", "", None);
                        eprintln!("▪ goal resumed");
                    }
                    "clear" => {
                        eng.set_goal("clear", "", None);
                        eprintln!("▪ goal cleared");
                    }
                    text => {
                        let (text, budget) = parse_goal(text);
                        eng.set_goal("set", &text, budget);
                        eprintln!(
                            "▪ goal set: {text}{}",
                            budget
                                .map(|b| format!(" [budget {b} tokens]"))
                                .unwrap_or_else(|| " [unlimited]".into())
                        );
                    }
                }
            }
            cmd if cmd == "/autonomous" || cmd.starts_with("/autonomous ") => {
                let arg = cmd.trim_start_matches("/autonomous").trim();
                let mut eng = engine.lock().await;
                if arg.is_empty() || arg == "status" {
                    match eng.session.autonomous().filter(|a| a.on) {
                        Some(a) => eprintln!(
                            "▪ autonomous on{} {}",
                            a.max_turns
                                .map(|m| format!(" max-turns {m}"))
                                .unwrap_or_default(),
                            a.gate
                                .as_deref()
                                .map(|g| format!("gate: {g}"))
                                .unwrap_or_else(|| "(no gate)".into())
                        ),
                        None => eprintln!("▪ autonomous off"),
                    }
                } else {
                    let (on, max_turns, max_tokens, gate) = parse_auto_flags(arg);
                    eng.set_autonomous(on, max_turns, max_tokens, gate.as_deref());
                    eprintln!("▪ autonomous {}", if on { "on" } else { "off" });
                }
            }
            "/branch" => {
                let eng = engine.lock().await;
                for (i, (id, label)) in eng.session.branch_points().iter().enumerate() {
                    eprintln!("{:>3}. [{}] {}", i + 1, id, label);
                }
            }
            cmd if cmd.starts_with("/branch ") => {
                let target = cmd.trim_start_matches("/branch ").trim().to_string();
                let mut eng = engine.lock().await;
                if eng.session.branch(&target) {
                    eprintln!("▪ leaf moved to {target}");
                } else {
                    eprintln!("▪ no entry {target}");
                }
            }
            "/fork" => {
                let eng = engine.lock().await;
                match eng.session.clone_session(&eng.workspace) {
                    Ok(s) => eprintln!("▪ forked → session {}", s.header.id),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            cmd if cmd == "/worktree" || cmd.starts_with("/worktree ") => {
                let name_arg = cmd.trim_start_matches("/worktree").trim();
                let mut name: String = name_arg
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .collect();
                if name.is_empty() {
                    name = format!("wt-{}", hasp_core::session::now_ms() % 100_000);
                }
                let mut eng = engine.lock().await;
                match eng.workspace.create_worktree(&name) {
                    Ok(ws2) => match SessionManager::create(&ws2) {
                        Ok(s) => {
                            let old = eng.workspace.clone();
                            eng.workspace = ws2;
                            eng.session = s;
                            worktree = Some((old, name.clone()));
                            eprintln!(
                                "▪ worktree .hasp/worktrees/{name} on hasp/{name} (/worktree-exit [remove])"
                            );
                        }
                        Err(e) => eprintln!("error: {e}"),
                    },
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            cmd if cmd == "/worktree-exit" || cmd.starts_with("/worktree-exit ") => {
                if let Some((old_ws, name)) = worktree.take() {
                    if (cmd.contains("remove") || cmd.contains(" rm"))
                        && let Err(e) = old_ws.remove_worktree(&name)
                    {
                        eprintln!("worktree remove failed: {e} (kept)");
                    }
                    let mut eng = engine.lock().await;
                    eng.workspace = old_ws.clone();
                    eng.session = SessionManager::open_latest(&old_ws)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| {
                            SessionManager::create(&old_ws).expect("session dir writable")
                        });
                    eprintln!("▪ back at {}", old_ws.root().display());
                } else {
                    eprintln!("▪ not in a worktree");
                }
            }
            cmd if cmd.starts_with("/name ") => {
                let name = cmd.trim_start_matches("/name ");
                let mut eng = engine.lock().await;
                eng.session.append_session_info(name);
                eprintln!("▪ session named “{name}”");
            }
            "/model" => {
                for m in &catalog {
                    eprintln!("  {} ({}k window)", m.id, m.context_window / 1000);
                }
            }
            cmd if cmd.starts_with("/model ") => {
                let id = cmd.trim_start_matches("/model ").trim();
                // Dynamic providers serve models the static catalog has never
                // heard of; their ids come from the endpoint, not from us.
                let known = catalog::find(&catalog, id).is_some();
                let dynamic = {
                    let eng = engine.lock().await;
                    hasp_provider::is_dynamic(eng.provider.name())
                };
                if known || dynamic {
                    let mut eng = engine.lock().await;
                    eng.model = id.to_string();
                    eng.max_tokens = hasp_provider::max_output_for(id);
                    let provider = eng.provider.name();
                    eng.session.append_model_change(provider, id);
                    eprintln!("▪ model → {id}");
                } else {
                    eprintln!("▪ unknown model {id}");
                }
            }
            "/resume" => {
                let eng = engine.lock().await;
                for (i, s) in SessionManager::list_for(&eng.workspace)?.iter().enumerate() {
                    eprintln!(
                        "{:>2}. {}  {}  {} entries  [{}]",
                        i + 1,
                        s.id,
                        s.name.as_deref().unwrap_or("-"),
                        s.message_entries,
                        s.path.display()
                    );
                }
                eprintln!("▪ /resume <n> opens");
            }
            cmd if cmd.starts_with("/resume ") => {
                let n: usize = cmd
                    .trim_start_matches("/resume ")
                    .trim()
                    .parse()
                    .context("/resume <n>")?;
                let eng = engine.lock().await;
                let list = SessionManager::list_for(&eng.workspace)?;
                let path = list
                    .get(n.checked_sub(1).context("1-based index")?)
                    .context("no such session")?
                    .path
                    .clone();
                drop(eng);
                let mut eng = engine.lock().await;
                eng.session = SessionManager::open(&path)?;
                eprintln!("▪ resumed {}", eng.session.header.id);
            }
            _ => {
                let mut eng = engine.lock().await;
                let mut sink = PrintSink;
                let mut res = eng.run_turn(input, &mut sink).await;
                while res.is_ok() {
                    match eng.post_turn_check(&mut sink).await {
                        Some(cont) => res = eng.run_turn(&cont, &mut sink).await,
                        None => break,
                    }
                }
                match res {
                    Ok(stats) => eprintln!(
                        "\n─ {} sample(s), {} tool call(s) · in {} (cache R {}/W {}) out {}{} · {} ms",
                        stats.samples,
                        stats.tool_calls,
                        stats.usage.total_in(),
                        stats.usage.cache_read,
                        stats.usage.cache_write,
                        stats.usage.output,
                        cost_note(&catalog, &eng.model, &eng.session.totals()),
                        stats.ms
                    ),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }
    }
    Ok(())
}

/// Endpoint coordinates for a dynamic provider: Ollama on localhost, or any
/// OpenAI-compatible vendor via `HASP_COMPAT_URL`.
fn dynamic_endpoint(provider: &str) -> Result<(String, String)> {
    match provider {
        "ollama" => Ok((
            hasp_workspace::env_var("HASP_OLLAMA_URL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "http://127.0.0.1:11434/v1".into()),
            "ollama".into(),
        )),
        _ => Ok((
            hasp_workspace::env_var("HASP_COMPAT_URL").context(
                "set HASP_COMPAT_URL to an OpenAI-compatible /v1 base (or pass --model)",
            )?,
            hasp_workspace::env_var("HASP_COMPAT_KEY").unwrap_or_default(),
        )),
    }
}

/// First model a dynamic endpoint serves — so `harness --provider ollama`
/// works with no `--model` at all.
async fn dynamic_default_model(provider: &str) -> Result<String> {
    let (base, key) = dynamic_endpoint(provider)?;
    let models = hasp_provider::openai_compat::list_models(&base, &key)
        .await
        .map_err(|e| anyhow!("listing models at {base}: {e}"))?;
    models.first().cloned().context(format!(
        "{base} serves no models — pull one (ollama pull …) or pass --model"
    ))
}

/// The checkout a binary lives in, if any: walks up from the executable so
/// `target/release/harness`, `target/debug/harness` and nested profiles all
/// resolve to the repo root.
fn checkout_root(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dir = exe.parent()?;
    for _ in 0..5 {
        let Some(parent) = dir.parent() else { break };
        if parent.join("crates/hasp-cli/Cargo.toml").exists() {
            return Some(parent.to_path_buf());
        }
        dir = parent;
    }
    None
}

/// Reinstall from the checkout this binary was built from. Not a network
/// updater — there is no release channel yet — so "update" means "rebuild
/// what is on disk", which is exactly right for a checkout-based install.
/// The checkout this binary was built from, or the one a previous update
/// recorded: an installed binary is not next to any checkout, but the first
/// successful update remembers where one lives.
pub(crate) fn find_checkout() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let remembered = std::fs::read_to_string(hasp_workspace::hasp_home()?.join("checkout"))
        .ok()
        .filter(|p| {
            std::path::Path::new(p.trim())
                .join("crates/hasp-cli/Cargo.toml")
                .exists()
        });
    checkout_root(&exe)
        .or_else(|| remembered.map(|p| p.trim().to_string().into()))
        .with_context(|| {
            "no checkout found — run once from the repo, or: cargo install --path <repo>/crates/hasp-cli".to_string()
        })
}

async fn self_update() -> Result<()> {
    let exe = std::env::current_exe()?;
    let repo = find_checkout()?;
    println!(
        "hasp {} — rebuilding from {}",
        env!("CARGO_PKG_VERSION"),
        repo.display()
    );
    let status = tokio::process::Command::new("cargo")
        .args(["install", "--path", "crates/hasp-cli"])
        .current_dir(&repo)
        .status()
        .await
        .map_err(|e| anyhow!("running cargo: {e} — is it on PATH?"))?;
    if !status.success() {
        bail!("cargo install failed: {status}");
    }
    let out = std::process::Command::new(&exe).arg("--version").output()?;
    std::fs::write(
        hasp_workspace::hasp_home()?.join("checkout"),
        repo.display().to_string(),
    )?;
    println!(
        "→ {} (checkout remembered: {})",
        String::from_utf8_lossy(&out.stdout).trim(),
        repo.display()
    );
    Ok(())
}

/// `harness models [provider]`: what is usable right now — the fetched catalog
/// for fixed providers, the live endpoint list for dynamic ones.
async fn list_models_cmd(provider: &str) -> Result<()> {
    if hasp_provider::is_dynamic(provider) {
        let (base, key) = dynamic_endpoint(provider)?;
        let models = hasp_provider::openai_compat::list_models(&base, &key)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        match models.len() {
            0 => println!("{base} serves no models"),
            n => {
                println!("{base} — {n} model(s):",);
                for m in models {
                    println!("  {m}");
                }
            }
        }
        return Ok(());
    }
    let catalog = catalog::load();
    let priced = catalog.iter().filter(|m| m.priced()).count();
    println!(
        "models.dev catalog — {} model(s), {priced} priced · refresh with: hasp update --models",
        catalog.len()
    );
    for m in &catalog {
        println!("  {:<36} {}k window", m.id, m.context_window / 1000);
    }
    Ok(())
}

/// Live daemon residents by session id — best effort: no daemon means no
/// residents, and a daemon that does not answer promptly is not waited on.
async fn live_resident_ids() -> Vec<String> {
    let Ok(sock) = daemon::socket_path() else {
        return Vec::new();
    };
    if !sock.exists() {
        return Vec::new();
    }
    let Ok(Ok(mut client)) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        hasp_wire::WireClient::connect(&sock),
    )
    .await
    else {
        return Vec::new();
    };
    if client.initialize().await.is_err() {
        return Vec::new();
    }
    match client
        .request(hasp_wire::METHOD_SESSION_RESIDENTS, json!({}))
        .await
    {
        Ok(v) => v
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|r| r["sessionId"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn human_age(created_ms: i64) -> String {
    let mins = (hasp_core::session::now_ms() - created_ms).max(0) / 60_000;
    if mins < 60 {
        format!("{mins}m")
    } else if mins < 60 * 24 {
        format!("{}h", mins / 60)
    } else {
        format!("{}d", mins / (60 * 24))
    }
}

/// `--name value` or `--name=value`.
fn flag_arg(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
        if a == name {
            return it.next().cloned();
        }
    }
    None
}

/// `harness sessions [list | rm <id> | prune [--keep N] [--older-than 7d]]`
///
/// Journals are files nobody could clean up before this. Deletion refuses any
/// session a live daemon holds: unlinking a file an appending daemon has open
/// loses that data silently, which is exactly the failure this project refuses.
async fn sessions_cmd(args: &[String]) -> Result<()> {
    let ws = Workspace::resolve(&std::env::current_dir()?)?;
    let mut list = SessionManager::list_for(&ws)?;
    list.sort_by(|a, b| b.path.cmp(&a.path));
    let live = live_resident_ids().await;
    let held = |id: &str| live.iter().any(|l| l == id);
    match args.first().map(String::as_str) {
        None | Some("list") => {
            if list.is_empty() {
                println!("no sessions in this workspace");
                return Ok(());
            }
            for s in &list {
                let kb = std::fs::metadata(&s.path)
                    .map(|m| m.len() / 1024)
                    .unwrap_or(0);
                println!(
                    "{} {:<10} {:<18} {:>4} msgs {:>6} KB  {}",
                    if held(&s.id) { "*" } else { " " },
                    s.id,
                    s.name.clone().unwrap_or_else(|| "-".into()),
                    s.message_entries,
                    kb,
                    human_age(s.created_ms)
                );
            }
            println!(
                "\n* = held by a running daemon · sessions rm <id> · prune [--keep N] [--older-than 7d]"
            );
        }
        Some("rm") => {
            let Some(prefix) = args.get(1) else {
                bail!("usage: hasp sessions rm <id>");
            };
            let target = list
                .iter()
                .find(|s| s.id.starts_with(prefix.as_str()))
                .with_context(|| format!("no session matching {prefix}"))?;
            if held(&target.id) {
                bail!(
                    "session {} is open in a running daemon — `hasp shutdown` first",
                    target.id
                );
            }
            std::fs::remove_file(&target.path)?;
            println!("removed {} ({})", target.id, target.path.display());
        }
        Some("prune") => {
            let keep: usize = flag_arg(args, "--keep")
                .map(|v| v.parse())
                .transpose()
                .context("--keep needs a number")?
                .unwrap_or(10);
            let older_ms = match flag_arg(args, "--older-than") {
                Some(d) => {
                    Some(parse_duration_ms(&d).with_context(|| format!("bad duration {d}"))?)
                }
                None => None,
            };
            let now = hasp_core::session::now_ms();
            let mut removed = 0;
            for (i, s) in list.iter().enumerate() {
                let too_old = older_ms
                    .map(|d| s.created_ms < now - d as i64)
                    .unwrap_or(false);
                let beyond_keep = older_ms.is_none() && i >= keep;
                if !(too_old || beyond_keep) || held(&s.id) {
                    continue;
                }
                std::fs::remove_file(&s.path)?;
                println!("removed {} ({})", s.id, human_age(s.created_ms));
                removed += 1;
            }
            println!("{removed} removed, {} kept", list.len() - removed);
        }
        Some(other) => bail!("unknown sessions subcommand: {other}"),
    }
    Ok(())
}

/// First `harness` on `PATH`, if any — how doctor tells an installed binary
/// from a dev one.
fn which_hasp() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("hasp");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

async fn doctor() -> Result<()> {
    println!("hasp doctor");
    let exe = std::env::current_exe()?;
    println!("  version:     {}", env!("CARGO_PKG_VERSION"));
    println!("  binary:      {}", exe.display());
    match which_hasp() {
        Some(on_path) if on_path == exe => println!("  install:     on PATH"),
        Some(on_path) => println!(
            "  install:     {} on PATH (this run: {})",
            on_path.display(),
            exe.display()
        ),
        None => println!("  install:     NOT on PATH"),
    }
    // A dev build inside target/ can say exactly where to install from.
    if let Some(repo) = checkout_root(&exe) {
        println!(
            "               dev build — install with: cargo install --path {}",
            repo.display()
        );
    }
    if let Ok(ws) = Workspace::resolve(&std::env::current_dir()?) {
        let root = ws.root();
        let names = |v: Vec<String>| {
            if v.is_empty() {
                "none".to_string()
            } else {
                v.join(", ")
            }
        };
        let commands = hasp_core::extensions::commands(root)
            .into_iter()
            .map(|c| format!("/{}", c.name))
            .collect();
        let skills = hasp_core::extensions::skills(root)
            .into_iter()
            .map(|s| s.name)
            .collect();
        println!("  commands:    {}", names(commands));
        println!("  skills:      {}", names(skills));
        let hooks = hasp_core::hooks::Hooks::load(root);
        println!(
            "  hooks:       {}{}",
            if hooks.is_empty() { "none" } else { "loaded" },
            if hasp_core::hooks::is_trusted(root) {
                " (project trusted)"
            } else {
                ""
            }
        );
        for w in &hooks.warnings {
            println!("               {w}");
        }
    }
    if let Ok(ws) = Workspace::resolve(&std::env::current_dir()?) {
        let envs = hasp_env::spec::list(ws.root());
        let names: Vec<String> = envs
            .iter()
            .map(|(n, r)| {
                if r.is_ok() {
                    n.clone()
                } else {
                    format!("{n} (INVALID)")
                }
            })
            .collect();
        let guests: Vec<String> = ["amd64", "arm64"]
            .iter()
            .filter(|a| hasp_env::guest::binary_for(a).is_ok())
            .map(|a| format!("linux/{a}"))
            .collect();
        println!(
            "  environments: {} · guest agent: {}",
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            },
            if guests.is_empty() {
                "not built (hasp env build-guest)".into()
            } else {
                guests.join(", ")
            }
        );
    }
    match hasp_tools::sandbox::Mode::from_env() {
        Ok(m) => println!("  sandbox:     {} (HASP_SANDBOX)", m.label()),
        Err(e) => println!("  sandbox:     INVALID — {e}"),
    }
    let stored = creds::load();
    let credential_path = creds::path()?;
    println!("  keys:");
    for (label, env_var, in_file) in [
        (
            "anthropic",
            "ANTHROPIC_API_KEY",
            stored.anthropic_api_key.is_some(),
        ),
        ("zai", "ZAI_API_KEY", stored.zai_api_key.is_some()),
        (
            "deepseek",
            "DEEPSEEK_API_KEY",
            stored.deepseek_api_key.is_some(),
        ),
    ] {
        let from_env = std::env::var(env_var)
            .map(|k| !k.is_empty())
            .unwrap_or(false);
        let state = if from_env {
            format!("env {env_var}")
        } else if in_file {
            format!("file {}", credential_path.display())
        } else {
            format!("MISSING (run: hasp login {label})")
        };
        println!("    {label:<10} {state}");
    }

    // Local and bring-your-own endpoints: reachable now, and serving what?
    let ollama_base = hasp_workspace::env_var("HASP_OLLAMA_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:11434/v1".into());
    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        hasp_provider::openai_compat::list_models(&ollama_base, "ollama"),
    )
    .await
    {
        Ok(Ok(models)) => println!("  ollama:       {ollama_base} — {} model(s)", models.len()),
        _ => println!("  ollama:       {ollama_base} — unreachable"),
    }
    match hasp_workspace::env_var("HASP_COMPAT_URL") {
        Ok(url) => println!("  compat:       {url}"),
        Err(_) => println!("  compat:       unset (HASP_COMPAT_URL)"),
    }

    // MCP: configured servers and the tools they actually contribute.
    let ws = Workspace::resolve(&std::env::current_dir()?)?;
    let servers = hasp_mcp::load_config(&ws);
    if servers.is_empty() {
        println!("  mcp:         none configured (.hasp/mcp.json)");
    } else {
        let tools = hasp_mcp::tools_for(&ws).await;
        println!(
            "  mcp:         {} server(s) → {} tool(s)",
            servers.len(),
            tools.len()
        );
        for t in &tools {
            println!("               {}", t.spec().name);
        }
    }

    let git = std::process::Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "MISSING".into());
    println!("  git:         {git}");

    let sessions =
        hasp_workspace::sessions_root().map_err(|e| std::io::Error::other(e.to_string()))?;
    let writable = probe_dir(&sessions);
    println!(
        "  sessions:    {} {}",
        sessions.display(),
        if writable { "writable" } else { "NOT WRITABLE" }
    );

    let cwd = std::env::current_dir()?;
    match Workspace::resolve(&cwd) {
        Ok(ws) => {
            println!(
                "  workspace:   {} ({}{})",
                ws.root().display(),
                ws.identity().branch.as_deref().unwrap_or("no branch"),
                ws.identity()
                    .head
                    .as_deref()
                    .map(|h| format!("@{h}"))
                    .unwrap_or_default(),
            );
            let state = ws
                .state_dir()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            println!(
                "  state dir:   {} {}",
                state.display(),
                if probe_dir(&state) {
                    "writable"
                } else {
                    "NOT WRITABLE"
                }
            );
        }
        Err(e) => println!("  workspace:   error: {e}"),
    }

    let models = catalog::load();
    let priced = models.iter().filter(|m| m.priced()).count();
    let source = if catalog::path()?.exists() {
        "models.json"
    } else {
        "bundled fallback (run: hasp update --models)"
    };
    println!(
        "  catalog:     {} models ({priced} priced) — {source}",
        models.len()
    );
    let sock = daemon::socket_path()?;
    let daemon_line = if daemon::is_running(&sock).await {
        format!("running ({})", sock.display())
    } else {
        "not running".to_string()
    };
    println!("  daemon:      {daemon_line}");
    Ok(())
}

fn probe_dir(dir: &std::path::Path) -> bool {
    let probe = dir.join(".hasp-probe");
    std::fs::write(&probe, b"").is_ok() && std::fs::remove_file(&probe).is_ok()
}

/// `/goal <text> [--budget N | --unlimited]` → (text, budget). Goals default to
/// a finite budget: continuations resend the whole context, so an unbounded
/// loop is the expensive failure mode.
fn parse_goal(text: &str) -> (String, Option<u64>) {
    if let Some((head, _)) = text.rsplit_once("--unlimited") {
        return (head.trim().to_string(), None);
    }
    if let Some((head, tail)) = text.rsplit_once("--budget")
        && let Ok(n) = tail.trim().parse::<u64>()
    {
        return (head.trim().to_string(), Some(n));
    }
    (
        text.trim().to_string(),
        Some(hasp_core::engine::GOAL_DEFAULT_BUDGET_TOKENS),
    )
}

/// `/autonomous on|off [--max-turns N] [--max-tokens N] [--gate "cmd"]`
fn parse_auto_flags(arg: &str) -> (bool, Option<u64>, Option<u64>, Option<String>) {
    let mut words = arg.split_whitespace();
    let on = matches!(words.next(), Some("on" | "true" | "1"));
    let (mut max_turns, mut max_tokens) = (None, None);
    let words: Vec<&str> = arg.split_whitespace().collect();
    for pair in words.windows(2) {
        match pair[0] {
            "--max-turns" => max_turns = pair[1].parse().ok(),
            "--max-tokens" => max_tokens = pair[1].parse().ok(),
            _ => {}
        }
    }
    let gate = arg
        .split_once("--gate")
        .map(|(_, rest)| rest.trim().trim_matches('"').trim().to_string())
        .filter(|g| !g.is_empty());
    (on, max_turns, max_tokens, gate)
}

/// Durations as users write them: `30s`, `10m`, `1h`.
fn parse_duration_ms(s: &str) -> Option<u64> {
    let (digits, unit) = s.split_at(s.len().checked_sub(1)?);
    let n: u64 = digits.parse().ok()?;
    match unit {
        "s" => Some(n * 1_000),
        "m" => Some(n * 60_000),
        "h" => Some(n * 3_600_000),
        _ => None,
    }
}
