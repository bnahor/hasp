//! `harness web`: the browser front end against the real binary.

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;

/// Minimal HTTP over a socket: no client dependency, and the page's needs are
/// one GET, one POST and one stream.
async fn http(port: u16, request: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    s.write_all(request.as_bytes()).await.unwrap();
    s.flush().await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_string();
    (status, body)
}

fn get(port: u16, path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n\r\n")
}

fn post(port: u16, path: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn start_web() -> (tokio::process::Child, u16, String, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_harness"));
    cmd.arg("web")
        .arg("0")
        .env("HOME", home.path())
        .env("HARNESS_PROVIDER", "mock")
        .env("HARNESS_SOCKET", home.path().join("web.sock"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().expect("spawn harness web");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(60), lines.next_line())
        .await
        .expect("web server prints its URL")
        .unwrap()
        .expect("a URL line");
    // harness web → http://127.0.0.1:PORT/?token=TOKEN
    let rest = line
        .split("http://127.0.0.1:")
        .nth(1)
        .expect("URL in banner");
    let port: u16 = rest.split('/').next().unwrap().parse().expect("port");
    let token = rest
        .split("token=")
        .nth(1)
        .expect("token in banner")
        .trim()
        .to_string();
    (child, port, token, home)
}

#[tokio::test]
async fn serves_the_page_and_drives_a_turn() {
    let (mut child, port, token, _home) = start_web().await;

    // No token, no service — a local port is not an authorization.
    let (status, _) = http(port, &get(port, "/")).await;
    assert_eq!(status, 401);

    let (status, page) = http(port, &get(port, &format!("/?token={token}"))).await;
    assert_eq!(status, 200);
    assert!(page.contains("harness"), "the page is served");
    assert!(page.contains("composer"), "and it is the app, not a stub");

    // The session is already open on this directory: stats answer immediately.
    let (status, body) = http(
        port,
        &post(
            port,
            &format!("/rpc?token={token}"),
            &serde_json::json!({"method": "session/stats"}).to_string(),
        ),
    )
    .await;
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], true, "{body}");
    assert!(v["result"]["model"].as_str().is_some(), "{body}");

    // A prompt runs the real turn through the daemon.
    let (_, body) = http(
        port,
        &post(
            port,
            &format!("/rpc?token={token}"),
            &serde_json::json!({"method": "session/prompt", "params": {"prompt": "hi web"}})
                .to_string(),
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], true, "{body}");
    assert!(
        v["result"]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("echo: hi web"),
        "{body}"
    );

    // History replays for a freshly loaded page.
    let (_, body) = http(
        port,
        &post(
            port,
            &format!("/rpc?token={token}"),
            &serde_json::json!({"method": "session/history"}).to_string(),
        ),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        v["result"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"].as_str().unwrap_or_default().contains("hi web")),
        "{body}"
    );

    let _ = child.kill().await;
}

#[tokio::test]
async fn streams_assistant_deltas_over_sse() {
    let (mut child, port, token, _home) = start_web().await;

    // Open the event stream, then prompt on a second connection: the deltas
    // must arrive on the stream while the turn is still running.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(get(port, &format!("/events?token={token}")).as_bytes())
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let mut head = [0u8; 256];
    let n = stream.read(&mut head).await.unwrap();
    assert!(
        String::from_utf8_lossy(&head[..n]).contains("200 OK"),
        "event stream opened"
    );

    let prompt = serde_json::json!({"method": "session/prompt", "params": {"prompt": "stream me"}});
    let mut s2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s2.write_all(post(port, &format!("/rpc?token={token}"), &prompt.to_string()).as_bytes())
        .await
        .unwrap();
    s2.flush().await.unwrap();
    let mut sink = Vec::new();
    s2.read_to_end(&mut sink).await.unwrap();

    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        let mut buf = [0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(30), stream.read(&mut buf))
            .await
            .expect("stream stays alive")
            .unwrap();
        if n == 0 {
            break;
        }
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        // Our wire naming on this hop; the browser translates to ACP-style
        // update kinds client-side.
        if seen.contains("\"kind\":\"delta\"") {
            break;
        }
    }
    assert!(
        seen.contains("\"kind\":\"delta\""),
        "deltas reached the browser stream: {seen}"
    );

    let _ = child.kill().await;
}
