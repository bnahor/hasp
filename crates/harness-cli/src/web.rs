//! A browser front end for the daemon (`harness web [port]`).
//!
//! Deliberately thin: the daemon already owns the session, so this is one more
//! client of it — a small localhost HTTP server that serves one embedded page,
//! forwards its commands over the wire protocol, and streams the daemon's
//! `session/update` notifications back as SSE. Nothing is reimplemented; the
//! browser gets exactly what a TUI gets.
//!
//! Localhost only, and every route requires the token printed at startup, so a
//! stray page in another tab cannot drive the session (CSRF on a local port is
//! a real thing).

use anyhow::{Context, Result};
use harness_wire::Wire;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};

const PAGE: &str = include_str!("../assets/web.html");

/// A command for the session actor; the reply travels back on the oneshot.
struct Cmd {
    method: String,
    params: Value,
    reply: oneshot::Sender<Result<Value, String>>,
}

/// Owns the daemon connection: requests out, notifications broadcast to
/// whoever is watching `/events`.
async fn session_actor(
    cwd: std::path::PathBuf,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    events: broadcast::Sender<Value>,
) -> Result<()> {
    let sock = crate::daemon::socket_path()?;
    crate::daemon::ensure_daemon(&sock, &[]).await?;
    let stream = UnixStream::connect(&sock)
        .await
        .with_context(|| format!("connecting to daemon at {}", sock.display()))?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let mut next_id = 0u64;
    let mut pending: std::collections::HashMap<u64, oneshot::Sender<Result<Value, String>>> =
        std::collections::HashMap::new();

    async fn send(wr: &mut OwnedWriteHalf, msg: &Wire) -> Result<()> {
        wr.write_all(harness_wire::encode(msg).as_bytes()).await?;
        wr.flush().await?;
        Ok(())
    }

    // Handshake first: initialize and open this directory's session. Nothing
    // else can happen until both are answered, so their replies are read
    // inline rather than through the pending map.
    for (method, params) in [
        (harness_wire::METHOD_INITIALIZE, json!({"client": "web"})),
        (
            harness_wire::METHOD_SESSION_CONTINUE,
            json!({"cwd": cwd.to_string_lossy()}),
        ),
    ] {
        next_id += 1;
        let id = next_id;
        send(&mut wr, &Wire::request(id, method, params)).await?;
        let answered = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(Wire::Response {
                    id: rid,
                    result,
                    error,
                }) = harness_wire::decode(&line)
                    && rid == id
                {
                    return match (result, error) {
                        (Some(_), _) => Ok(()),
                        (None, Some(e)) => Err(e.message),
                        _ => Ok(()),
                    };
                }
            }
            Err("daemon closed before answering".to_string())
        })
        .await;
        match answered {
            Ok(Ok(())) => {}
            Ok(Err(e)) => anyhow::bail!("daemon refused {method}: {e}"),
            Err(_) => anyhow::bail!("daemon did not answer {method}"),
        }
    }

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                next_id += 1;
                let id = next_id;
                if send(&mut wr, &Wire::request(id, &cmd.method, cmd.params)).await.is_err() {
                    let _ = cmd.reply.send(Err("daemon connection lost".into()));
                    return Ok(());
                }
                pending.insert(id, cmd.reply);
            }
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()) };
                match harness_wire::decode(&line) {
                    Some(Wire::Response { id, result, error }) => {
                        if let Some(tx) = pending.remove(&id) {
                            let _ = tx.send(match (result, error) {
                                (Some(r), _) => Ok(r),
                                (None, Some(e)) => Err(e.message),
                                _ => Ok(Value::Null),
                            });
                        }
                    }
                    Some(Wire::Notification { method, params }) => {
                        let _ = events.send(json!({"method": method, "params": params}));
                    }
                    // A localhost page has no way to answer approvals; deny,
                    // exactly like a detached daemon does.
                    Some(Wire::Request { id, .. }) => {
                        let _ = send(&mut wr, &Wire::Response {
                            id,
                            result: Some(json!({"answer": "n"})),
                            error: None,
                        }).await;
                    }
                    None => {}
                }
            }
        }
    }
}

fn token() -> String {
    // Localhost CSRF guard, not a secret worth a CSPRNG.
    format!(
        "{:x}{:x}",
        std::process::id(),
        harness_core::session::now_ms()
    )
}

async fn read_request(
    stream: &mut TcpStream,
) -> Result<(String, String, Option<usize>, String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(4096);
    let mut byte = [0u8; 1];
    // Headers end at CRLFCRLF; read a byte at a time so no body is swallowed.
    while !buf.ends_with(b"\r\n\r\n") {
        if buf.len() > 64 * 1024 {
            anyhow::bail!("request head too large");
        }
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            anyhow::bail!("connection closed");
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut content_length = None;
    for line in lines {
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().ok();
        }
    }
    let mut body = vec![0u8; content_length.unwrap_or(0)];
    if !body.is_empty() {
        stream.read_exact(&mut body).await?;
    }
    Ok((method, target, content_length, head, body))
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
         cache-control: no-store\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// `harness web [port]` — serve the browser client until killed.
pub async fn serve(port: u16) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (events, _) = broadcast::channel::<Value>(256);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let actor_events = events.clone();
    tokio::spawn(async move {
        if let Err(e) = session_actor(cwd, cmd_rx, actor_events).await {
            eprintln!("session ended: {e}");
        }
    });

    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{port}"))?;
    let bound = listener.local_addr()?.port();
    let token = token();
    // Written once, errors ignored: whoever reads the banner may close the
    // pipe after the URL line (`harness web | head -1`, a test harness), and a
    // `println!` into a closed pipe panics — which took the server down with it.
    {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = write!(
            out,
            "harness web → http://127.0.0.1:{bound}/?token={token}\n  \
             localhost only · token required · one session (this directory)\n"
        );
        let _ = out.flush();
    }

    let token = Arc::new(token);
    loop {
        let (mut stream, _) = listener.accept().await?;
        let token = token.clone();
        let events = events.clone();
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(&mut stream, &token, &events, &cmd_tx).await {
                let _ = respond(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain",
                    &format!("{e}"),
                )
                .await;
            }
        });
    }
}

async fn handle(
    stream: &mut TcpStream,
    token: &str,
    events: &broadcast::Sender<Value>,
    cmd_tx: &mpsc::UnboundedSender<Cmd>,
) -> Result<()> {
    let (method, target, _, _, body) = read_request(stream).await?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let supplied = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
        .unwrap_or_default();
    if supplied != token {
        return respond(stream, "401 Unauthorized", "text/plain", "bad token").await;
    }
    match (method.as_str(), path.as_str()) {
        ("GET", "/") => respond(stream, "200 OK", "text/html; charset=utf-8", PAGE).await,
        ("GET", "/health") => respond(stream, "200 OK", "application/json", r#"{"ok":true}"#).await,
        ("POST", "/rpc") => {
            let req: Value = serde_json::from_slice(&body).context("body must be JSON")?;
            let method = req["method"]
                .as_str()
                .context("method required")?
                .to_string();
            let params = req.get("params").cloned().unwrap_or(json!({}));
            let (tx, rx) = oneshot::channel();
            cmd_tx
                .send(Cmd {
                    method: method.clone(),
                    params,
                    reply: tx,
                })
                .map_err(|_| anyhow::anyhow!("session is gone"))?;
            let out = match tokio::time::timeout(std::time::Duration::from_secs(600), rx).await {
                Ok(Ok(Ok(v))) => json!({"ok": true, "result": v}),
                Ok(Ok(Err(e))) => json!({"ok": false, "error": e}),
                Ok(Err(_)) => json!({"ok": false, "error": "session ended"}),
                Err(_) => json!({"ok": false, "error": "timed out"}),
            };
            respond(stream, "200 OK", "application/json", &out.to_string()).await
        }
        ("GET", "/events") => {
            let mut rx = events.subscribe();
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                        cache-control: no-store\r\nconnection: keep-alive\r\n\r\n";
            stream.write_all(head.as_bytes()).await?;
            stream.flush().await?;
            // Keep-alive comments so proxies and browsers hold the stream open.
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(20));
            loop {
                tokio::select! {
                    ev = rx.recv() => match ev {
                        Ok(v) => {
                            stream.write_all(format!("data: {v}\n\n").as_bytes()).await?;
                            stream.flush().await?;
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    },
                    _ = tick.tick() => {
                        stream.write_all(b": ping\n\n").await?;
                        stream.flush().await?;
                    }
                }
            }
            Ok(())
        }
        _ => respond(stream, "404 Not Found", "text/plain", "no such route").await,
    }
}
