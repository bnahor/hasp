use anyhow::{Context, Result, anyhow, bail};
use harness_core::engine::{Engine, TurnSink};
use harness_core::session::SessionManager;
use harness_core::{Sink, Usage};
use harness_provider::catalog;
use harness_tools::builtins;
use harness_workspace::Workspace;
use serde_json::json;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use tui::UiEvent;

mod backend;
mod creds;
mod daemon;
mod gate;
mod highlight;
mod markdown;
mod mcp_server;
mod tui;

use gate::{GateCore, PlainGate, TuiGate};

const DEFAULT_MODEL: &str = "claude-sonnet-4-5";
const MAX_OUTPUT_TOKENS: u32 = 16384;

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

    fn on_tool(&mut self, ev: &harness_core::engine::ToolEvent) {
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
            println!("harness {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("login") => {
            let provider = argv.get(1).map(String::as_str).unwrap_or("anthropic");
            return creds::login_interactive(provider);
        }
        Some("doctor") => return doctor().await,
        Some("models") => {
            let provider = argv
                .get(1)
                .cloned()
                .or_else(|| std::env::var("HARNESS_PROVIDER").ok())
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
                    "usage: harness update (rebuild from the checkout) · harness update --models (catalog)"
                );
            }
            return self_update().await;
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
                        println!("{:<22} {:<26} {}", "worktree", "branch", "state");
                        for name in names {
                            println!(
                                "{:<22} {:<26} {}",
                                name,
                                format!("harness/{name}"),
                                if ws.worktree_merged(&name) {
                                    "merged — nothing to lose"
                                } else {
                                    "unmerged — holds work"
                                }
                            );
                        }
                    }
                    println!("\nharness worktrees prune <name> [--force]");
                }
                Some("prune") => {
                    let force = args.iter().any(|a| *a == "--force");
                    let Some(name) = args.iter().skip(1).find(|a| !a.starts_with("--")) else {
                        bail!("usage: harness worktrees prune <name> [--force]");
                    };
                    if !ws.worktree_merged(name) && !force {
                        bail!(
                            "worktree `{name}` holds unmerged work — inspect it first, or pass \
                             --force to discard the checkout"
                        );
                    }
                    // The branch is deliberately left behind, so even a forced
                    // prune keeps the child's commits recoverable.
                    ws.remove_worktree(name)?;
                    println!(
                        "removed worktree {name}; branch harness/{name} kept (drop it with \
                         `git branch -D harness/{name}` when you are sure)"
                    );
                }
                Some(other) => bail!("unknown worktrees subcommand: {other}"),
            }
            return Ok(());
        }
        Some("shutdown") => {
            let sock = daemon::socket_path()?;
            let mut client = harness_wire::WireClient::connect(&sock)
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
    let mut use_daemon = false;
    let mut no_daemon = false;
    let mut ascii = std::env::var("HARNESS_ASCII").is_ok();
    let mut provider_flag: Option<String> = None;
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
            "--daemon" => use_daemon = true,
            "--provider" => provider_flag = Some(it.next().context("--provider needs a value")?),
            // embedded mode escape hatch (default is daemon-backed)
            "--no-daemon" => no_daemon = true,
            "--ascii" => ascii = true,
            other => bail!(
                "unknown flag: {other} (expected -p, --model, --new, --yolo, --plain, --serve, --daemon, --provider, --no-daemon, --ascii, --mcp-serve)"
            ),
        }
    }

    if serve {
        return daemon::serve(daemon::socket_path()?).await;
    }
    if mcp_serve {
        return mcp_server::serve().await;
    }

    let cwd = std::env::current_dir()?;
    if let (Some(p), true) = (&prompt, use_daemon) {
        let sock = daemon::socket_path()?;
        let mut client = harness_wire::WireClient::connect(&sock)
            .await
            .context("daemon not running — start it with `harness --serve` (or drop --daemon)")?;
        client.initialize().await?;
        let s = client
            .request(
                harness_wire::METHOD_SESSION_CONTINUE,
                serde_json::json!({"cwd": cwd}),
            )
            .await?;
        let r = client
            .request(
                harness_wire::METHOD_SESSION_PROMPT,
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
        std::env::var("HARNESS_PROVIDER").unwrap_or_else(|_| "anthropic".into())
    });
    let mock = provider_name == "mock";
    // provider-appropriate default model when neither flag nor env set one;
    // dynamic providers have no static default, so ask the endpoint
    if !model_flag_given && std::env::var("ANTHROPIC_MODEL").is_err() {
        model = if harness_provider::is_dynamic(&provider_name) {
            dynamic_default_model(&provider_name).await?
        } else {
            harness_provider::default_model(&provider_name).to_string()
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
                        "no API key found for {provider_name} (env or `harness login {provider_name}`)"
                    );
                    creds::login_interactive(&provider_name)?;
                    Some(creds::resolve_key(&provider_name).context("still no key after login")?)
                } else {
                    bail!(
                        "no {provider_name} API key: run `harness login {provider_name}` (or --provider mock)"
                    );
                }
            }
        }
    };
    // Children each need their own provider instance; the root takes one
    // from the same factory so model selection stays consistent.
    let provider_factory: harness_bus::ProviderFactory = {
        let key = api_key.clone();
        let model_f = model.clone();
        let name = provider_name.clone();
        Arc::new(move || {
            if name == "mock" {
                return Ok(Box::new(harness_provider::echo::EchoProvider::from_env())
                    as Box<dyn harness_core::Provider>);
            }
            match harness_provider::build(&name, key.clone().unwrap_or_default(), model_f.clone()) {
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
        harness_wire::METHOD_SESSION_NEW
    } else {
        harness_wire::METHOD_SESSION_CONTINUE
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
        // (build_provider reads HARNESS_PROVIDER / ANTHROPIC_MODEL)
        let mut daemon_env: Vec<(String, String)> = vec![
            ("HARNESS_PROVIDER".into(), provider_name.clone()),
            ("ANTHROPIC_MODEL".into(), model.clone()),
        ];
        // The daemon owns the gate, so it has to know about --yolo; otherwise
        // approvals keep prompting in daemon mode no matter what the client said.
        if yolo {
            daemon_env.push(("HARNESS_YOLO".into(), "1".into()));
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

#[allow(clippy::too_many_arguments)]
async fn embedded_engine_with_bus(
    ws: harness_workspace::Workspace,
    session: SessionManager,
    provider_factory: &harness_bus::ProviderFactory,
    model: String,
    yolo: bool,
    interactive: bool,
    prompt_tx: Option<std::sync::mpsc::Sender<gate::PromptRequest>>,
    fanout: Option<harness_bus::NoticeFanout>,
) -> Result<Engine> {
    // Parent gate: the TUI approval channel when present, plain stdin gate
    // otherwise. Child gate factory: approvals route to the same channel;
    // without one (non-interactive), children run yolo — structural
    // confinement still applies (approvals are a trust toggle, confinement
    // is not).
    let (parent_gate, child_gate_factory): (Box<dyn harness_core::Gate>, harness_bus::GateFactory) =
        match prompt_tx {
            Some(tx) => {
                let child_tx = std::sync::Mutex::new(tx.clone());
                (
                    Box::new(TuiGate::new(GateCore::new(yolo, true), tx)),
                    Arc::new(move || {
                        Box::new(TuiGate::new(
                            GateCore::new(yolo, true),
                            child_tx.lock().expect("gate channel").clone(),
                        )) as Box<dyn harness_core::Gate>
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
                    }) as Box<dyn harness_core::Gate>
                }),
            ),
        };
    // MCP servers contribute tools alongside the built-ins; the engine keeps
    // the combined list name-sorted (cache contract).
    let mut tools = builtins();
    tools.extend(harness_mcp::tools_for(&ws).await);
    let mut engine = Engine::new(
        ws.clone(),
        session,
        provider_factory().map_err(|e| anyhow::anyhow!("{e}"))?,
        tools,
        parent_gate,
        model.clone(),
        MAX_OUTPUT_TOKENS,
    )?;
    let bus = Arc::new(harness_bus::LocalBus::new(
        ws,
        model,
        MAX_OUTPUT_TOKENS,
        provider_factory.clone(),
        child_gate_factory,
        engine.notice_inbox(),
        fanout,
    ));
    engine.set_bus(bus);
    Ok(engine)
}

#[allow(clippy::too_many_arguments)]
async fn embedded_backend(
    ws: Workspace,
    session: SessionManager,
    provider_factory: &harness_bus::ProviderFactory,
    model: String,
    yolo: bool,
    prompt_tx: std::sync::mpsc::Sender<gate::PromptRequest>,
    event_tx: std::sync::mpsc::Sender<tui::UiEvent>,
) -> Result<Box<dyn backend::Backend>> {
    let fanout: harness_bus::NoticeFanout = {
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
            "/compact" => {
                let mut eng = engine.lock().await;
                let model = eng.model.clone();
                let max_tokens = eng.max_tokens;
                let e: &mut Engine = &mut *eng;
                let est = e.context_est_tokens();
                match harness_core::compaction::compact(
                    e.provider.as_ref(),
                    &model,
                    &mut e.session,
                    max_tokens,
                    est,
                )
                .await
                {
                    Ok(cs) => eprintln!(
                        "▪ compacted ~{}k tokens → brief + {} retained message(s)",
                        cs.tokens_before / 1000,
                        cs.retained_messages
                    ),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
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
                    name = format!("wt-{}", harness_core::session::now_ms() % 100_000);
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
                                "▪ worktree .harness/worktrees/{name} on harness/{name} (/worktree-exit [remove])"
                            );
                        }
                        Err(e) => eprintln!("error: {e}"),
                    },
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            cmd if cmd == "/worktree-exit" || cmd.starts_with("/worktree-exit ") => {
                if let Some((old_ws, name)) = worktree.take() {
                    if cmd.contains("remove") || cmd.contains(" rm") {
                        if let Err(e) = old_ws.remove_worktree(&name) {
                            eprintln!("worktree remove failed: {e} (kept)");
                        }
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
                    harness_provider::is_dynamic(eng.provider.name())
                };
                if known || dynamic {
                    let mut eng = engine.lock().await;
                    eng.model = id.to_string();
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
/// OpenAI-compatible vendor via `HARNESS_COMPAT_URL`.
fn dynamic_endpoint(provider: &str) -> Result<(String, String)> {
    match provider {
        "ollama" => Ok((
            std::env::var("HARNESS_OLLAMA_URL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "http://127.0.0.1:11434/v1".into()),
            "ollama".into(),
        )),
        _ => Ok((
            std::env::var("HARNESS_COMPAT_URL").context(
                "set HARNESS_COMPAT_URL to an OpenAI-compatible /v1 base (or pass --model)",
            )?,
            std::env::var("HARNESS_COMPAT_KEY").unwrap_or_default(),
        )),
    }
}

/// First model a dynamic endpoint serves — so `harness --provider ollama`
/// works with no `--model` at all.
async fn dynamic_default_model(provider: &str) -> Result<String> {
    let (base, key) = dynamic_endpoint(provider)?;
    let models = harness_provider::openai_compat::list_models(&base, &key)
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
        if parent.join("crates/harness-cli/Cargo.toml").exists() {
            return Some(parent.to_path_buf());
        }
        dir = parent;
    }
    None
}

/// Reinstall from the checkout this binary was built from. Not a network
/// updater — there is no release channel yet — so "update" means "rebuild
/// what is on disk", which is exactly right for a checkout-based install.
async fn self_update() -> Result<()> {
    let exe = std::env::current_exe()?;
    // An installed binary is not next to any checkout, but the first
    // successful update records where one lives, so later updates work from
    // anywhere.
    let remembered = std::fs::read_to_string(harness_workspace::harness_home()?.join("checkout"))
        .ok()
        .filter(|p| {
            std::path::Path::new(p.trim())
                .join("crates/harness-cli/Cargo.toml")
                .exists()
        });
    let repo = checkout_root(&exe)
        .or_else(|| remembered.map(|p| p.trim().to_string().into()))
        .with_context(|| {
            format!(
                "no checkout found — run once from the repo, or: cargo install --path <repo>/crates/harness-cli"
            )
        })?;
    println!(
        "harness {} — rebuilding from {}",
        env!("CARGO_PKG_VERSION"),
        repo.display()
    );
    let status = tokio::process::Command::new("cargo")
        .args(["install", "--path", "crates/harness-cli"])
        .current_dir(&repo)
        .status()
        .await
        .map_err(|e| anyhow!("running cargo: {e} — is it on PATH?"))?;
    if !status.success() {
        bail!("cargo install failed: {status}");
    }
    let out = std::process::Command::new(&exe).arg("--version").output()?;
    std::fs::write(
        harness_workspace::harness_home()?.join("checkout"),
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
    if harness_provider::is_dynamic(provider) {
        let (base, key) = dynamic_endpoint(provider)?;
        let models = harness_provider::openai_compat::list_models(&base, &key)
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
        "models.dev catalog — {} model(s), {priced} priced · refresh with: harness update --models",
        catalog.len()
    );
    for m in &catalog {
        println!("  {:<36} {}k window", m.id, m.context_window / 1000);
    }
    Ok(())
}

/// First `harness` on `PATH`, if any — how doctor tells an installed binary
/// from a dev one.
fn which_harness() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("harness");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

async fn doctor() -> Result<()> {
    println!("harness doctor");
    let exe = std::env::current_exe()?;
    println!("  version:     {}", env!("CARGO_PKG_VERSION"));
    println!("  binary:      {}", exe.display());
    match which_harness() {
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
            format!("MISSING (run: harness login {label})")
        };
        println!("    {label:<10} {state}");
    }

    // Local and bring-your-own endpoints: reachable now, and serving what?
    let ollama_base = std::env::var("HARNESS_OLLAMA_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:11434/v1".into());
    match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        harness_provider::openai_compat::list_models(&ollama_base, "ollama"),
    )
    .await
    {
        Ok(Ok(models)) => println!("  ollama:       {ollama_base} — {} model(s)", models.len()),
        _ => println!("  ollama:       {ollama_base} — unreachable"),
    }
    match std::env::var("HARNESS_COMPAT_URL") {
        Ok(url) => println!("  compat:       {url}"),
        Err(_) => println!("  compat:       unset (HARNESS_COMPAT_URL)"),
    }

    // MCP: configured servers and the tools they actually contribute.
    let ws = Workspace::resolve(&std::env::current_dir()?)?;
    let servers = harness_mcp::load_config(&ws);
    if servers.is_empty() {
        println!("  mcp:         none configured (.harness/mcp.json)");
    } else {
        let tools = harness_mcp::tools_for(&ws).await;
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
        harness_workspace::sessions_root().map_err(|e| std::io::Error::other(e.to_string()))?;
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
        "bundled fallback (run: harness update --models)"
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
    let probe = dir.join(".harness-probe");
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
        Some(harness_core::engine::GOAL_DEFAULT_BUDGET_TOKENS),
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
