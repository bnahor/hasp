//! Across real process boundaries: the hasp daemon (mock model), the web API
//! with its two token scopes, the MCP server binary, the CLI's config checks.

mod common;

use common::{FakeTelegram, MockBackend, serve};
use hasp_gateway::adapters::telegram::Telegram;
use hasp_gateway::backend::{HaspDaemon, session_dir};
use hasp_gateway::gateway::{Config, Gateway};
use hasp_gateway::leader::Solo;
use hasp_gateway::people::OWNER;
use hasp_gateway::store::Store;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn hasp_bin() -> PathBuf {
    // target/debug/hasp, built by the workspace (cargo build -p hasp-cli)
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/hasp");
    assert!(p.exists(), "build hasp first: cargo build -p hasp-cli");
    p
}

async fn spawn_daemon(home: &Path) -> tokio::process::Child {
    let sock = home.join("d.sock");
    let _ = std::fs::remove_file(&sock);
    let child = tokio::process::Command::new(hasp_bin())
        .args(["--serve", "--provider", "mock"])
        .env("HOME", home)
        .env("HASP_SOCKET", &sock)
        .env("HASP_PROVIDER", "mock")
        .env("HASP_JOURNAL_FSYNC", "0")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    for _ in 0..400 {
        if sock.exists() {
            return child;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("daemon didn't start");
}

async fn shutdown(home: &Path, mut child: tokio::process::Child) {
    if let Ok(mut c) = hasp_wire::WireClient::connect(&home.join("d.sock")).await {
        let _ = c.shutdown().await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
}

async fn pump(gw: &Arc<Gateway>) {
    for _ in 0..3 {
        gw.poll_adapter("telegram").await.unwrap();
        gw.route_once().await.unwrap();
        for h in gw.run_turns().unwrap() {
            h.await.unwrap();
        }
        gw.deliver_once().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn real_hasp_daemon_keeps_the_conversation_across_a_daemon_restart() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let tg = Arc::new(FakeTelegram { token: "T".into(), ..Default::default() });
    let url = serve(tg.handler()).await;
    let store = Store::open(&dir.path().join("gw.db")).unwrap();
    store.ensure_owner("me").unwrap();
    let root = dir.path().join("sessions");
    let mut gw = Gateway::new(store, Arc::new(HaspDaemon { socket: home.join("d.sock"), root: root.clone() }), Arc::new(Solo), Config {
        approval_ttl_ms: 60_000, hasp_home: home.join(".hasp"), send_to_others: vec![], notify_per_hour: 10,
    });
    gw.add_adapter(Arc::new(Telegram::new(&url, "T", 0).with_per_chat_interval_ms(0)));
    let gw = Arc::new(gw);
    let code = gw.store.issue_link_code(OWNER, 60_000, "cli").unwrap();
    tg.push_text(1, 7, 7, "Me", &format!("/link {code}"), None);

    let daemon = spawn_daemon(&home).await;
    tg.push_text(2, 7, 7, "Me", "first message", None);
    pump(&gw).await;
    assert!(tg.texts().iter().any(|t| t == "echo: first message"), "{:?}", tg.texts());

    // the daemon goes away mid-conversation: the turn fails visibly, nothing hangs
    shutdown(&home, daemon).await;
    tg.push_text(3, 7, 7, "Me", "while it's down", None);
    pump(&gw).await;
    assert!(tg.texts().iter().any(|t| t.starts_with("That failed:")), "{:?}", tg.texts());

    // back up: same session (its journal holds the whole conversation)
    let daemon = spawn_daemon(&home).await;
    tg.push_text(4, 7, 7, "Me", "second message", None);
    pump(&gw).await;
    assert!(tg.texts().iter().any(|t| t == "echo: second message"));
    let ws = session_dir(&root, "personal:owner");
    let key_dirs: Vec<PathBuf> = std::fs::read_dir(home.join(".hasp/sessions")).unwrap().flatten().map(|e| e.path()).collect();
    let journals: Vec<PathBuf> = key_dirs.iter().flat_map(|d| std::fs::read_dir(d).unwrap().flatten().map(|e| e.path())).collect();
    assert_eq!(journals.len(), 1, "one session for the personal conversation: {journals:?}");
    let body = std::fs::read_to_string(&journals[0]).unwrap();
    assert!(body.contains("first message") && body.contains("second message"));
    assert!(body.contains(&ws.file_name().unwrap().to_string_lossy().to_string()), "journal belongs to the session's workspace");
    shutdown(&home, daemon).await;
}

// ---------------------------------------------------------------- web API + MCP

async fn http(method: &str, url: &str, token: Option<&str>, body: Option<Value>, extra: &[(&str, &str)]) -> (u16, Value) {
    let c = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let mut r = c.request(method.parse().unwrap(), url);
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    for (k, v) in extra {
        r = r.header(*k, *v);
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    let resp = r.send().await.unwrap();
    let st = resp.status().as_u16();
    (st, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn web_and_agent_tokens_are_separate_scopes() {
    let dir = tempfile::tempdir().unwrap();
    let tg = Arc::new(FakeTelegram { token: "T".into(), ..Default::default() });
    let url = serve(tg.handler()).await;
    let store = Store::open(&dir.path().join("gw.db")).unwrap();
    store.ensure_owner("me").unwrap();
    let root = dir.path().join("sessions");
    store.set_meta("workspace_root", &root.to_string_lossy()).unwrap();
    let mut gw = Gateway::new(store.clone(), Arc::new(MockBackend::default()), Arc::new(Solo), Config {
        approval_ttl_ms: 60_000, hasp_home: dir.path().join("hasp"), send_to_others: vec!["telegram".into()], notify_per_hour: 10,
    });
    gw.add_adapter(Arc::new(Telegram::new(&url, "T", 0).with_per_chat_interval_ms(0)));
    gw.add_adapter(Arc::new(hasp_gateway::adapters::web::Web::new(store.clone())));
    let gw = Arc::new(gw);
    store.link_surface("web", hasp_gateway::adapters::web::WEB_SENDER, "test").unwrap();
    // the owner's chat home (linked Telegram)
    let code = store.issue_link_code(OWNER, 60_000, "cli").unwrap();
    tg.push_text(1, 7, 7, "Me", &format!("/link {code}"), None);
    pump(&gw).await;
    let owner = hasp_gateway::web::set_token(&gw, "owner").unwrap();
    let agent = hasp_gateway::web::set_token(&gw, "agent").unwrap();
    let port = hasp_gateway::web::serve(gw.clone(), 0).await.unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let sdir = session_dir(&root, "personal:owner");
    std::fs::create_dir_all(&sdir).unwrap();
    let sd = sdir.to_string_lossy().to_string();

    assert_eq!(http("GET", &format!("{base}/api/state"), None, None, &[]).await.0, 401);
    assert_eq!(http("GET", &format!("{base}/api/state"), Some(&agent), None, &[]).await.0, 401, "agent can't read owner state");
    assert_eq!(http("GET", &format!("{base}/api/state"), Some(&owner), None, &[]).await.0, 200);
    let login = http("GET", &format!("{base}/login?token={owner}"), None, None, &[]).await;
    assert_eq!(login.0, 302);
    // agent: draft + request_send from its own workspace only
    let (st, d) = http("POST", &format!("{base}/agent/draft_message"), Some(&agent), Some(json!({"adapter": "telegram", "recipient": "42", "text": "hi"})), &[("x-session-dir", &sd)]).await;
    assert_eq!(st, 200, "{d}");
    let outside = dir.path().join("elsewhere").to_string_lossy().to_string();
    assert_eq!(http("POST", &format!("{base}/agent/draft_message"), Some(&agent), Some(json!({})), &[("x-session-dir", &outside)]).await.0, 403);
    assert_eq!(http("POST", &format!("{base}/agent/notify_owner"), Some(&owner), Some(json!({"text": "x"})), &[("x-session-dir", &sd)]).await.0, 401, "owner token isn't an agent token either");
    let (_, r) = http("POST", &format!("{base}/agent/request_send"), Some(&agent), Some(json!({"draft_id": d["draft_id"]})), &[("x-session-dir", &sd)]).await;
    let id = r["approval"].as_str().unwrap().to_string();
    // the agent can't decide, even with the code
    assert_eq!(http("POST", &format!("{base}/api/approvals/{id}/decide"), Some(&agent), Some(json!({"decision": "approve", "nonce": "x"})), &[]).await.0, 401);
    // the owner page sees the card with its code and decides through the same authority
    let (_, state) = http("GET", &format!("{base}/api/state"), Some(&owner), None, &[]).await;
    let card = state["approvals"].as_array().unwrap().iter().find(|a| a["id"] == id.as_str()).unwrap().clone();
    assert!(card["card"].as_str().unwrap().contains("To: 42 on telegram"));
    // cross-site form posts are refused
    let c = reqwest::Client::new();
    let form = c.post(format!("{base}/api/approvals/{id}/decide")).bearer_auth(&owner).header("content-type", "application/x-www-form-urlencoded").body("decision=approve").send().await.unwrap();
    assert_eq!(form.status().as_u16(), 403);
    let (_, msg) = http("POST", &format!("{base}/api/approvals/{id}/decide"), Some(&owner), Some(json!({"decision": "approve", "nonce": card["nonce"]})), &[]).await;
    assert!(msg["message"].as_str().unwrap().starts_with("Approved"), "{msg}");
    pump(&gw).await;
    assert!(tg.sent.lock().unwrap().iter().any(|m| m["chat_id"] == "42" && m["text"] == "hi"));
    // web chat goes through the same pipeline into the personal conversation
    http("POST", &format!("{base}/api/chat"), Some(&owner), Some(json!({"text": "from the browser"})), &[]).await;
    gw.route_once().await.unwrap();
    assert!(gw.store.json_rows("SELECT 1 FROM turns WHERE session_key = 'personal:owner' AND prompt = 'from the browser'", &[]).unwrap().len() == 1);

    // the MCP server binary: sorted tools, calls land in the caller's session
    let tokfile = dir.path().join("agent.token");
    std::fs::write(&tokfile, &agent).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tokfile, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut mcp = tokio::process::Command::new(env!("CARGO_BIN_EXE_hasp-gateway"))
        .arg("mcp")
        .current_dir(&sdir)
        .env("HASP_GATEWAY_URL", &base)
        .env("HASP_GATEWAY_AGENT_TOKEN_FILE", &tokfile)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut stdin = mcp.stdin.take().unwrap();
    let mut out = tokio::io::BufReader::new(mcp.stdout.take().unwrap()).lines();
    for line in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "wait_for_user", "arguments": {"question": "Which date?"}}}),
    ] {
        stdin.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    }
    let _init = out.next_line().await.unwrap().unwrap();
    let list: Value = serde_json::from_str(&out.next_line().await.unwrap().unwrap()).unwrap();
    let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "tool list is name-sorted");
    assert!(!names.iter().any(|n| n.contains("approve")), "no approval tool for the agent");
    let call: Value = serde_json::from_str(&out.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(call["result"]["isError"], false, "{call}");
    let w = gw.store.open_waiting(Some("personal:owner")).unwrap();
    assert_eq!(w[0]["question"], "Which date?");
}

#[test]
fn cli_refuses_inline_credentials() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), r#"{"adapters": {"telegram": {"token": "123:ABC"}}}"#).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_hasp-gateway"))
        .arg("run")
        .env("HASP_GATEWAY_HOME", dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("token_file"), "{}", String::from_utf8_lossy(&out.stderr));
}
