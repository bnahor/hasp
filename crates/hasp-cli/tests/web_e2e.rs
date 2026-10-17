//! `harness web`: the browser front end against the real binary.

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;

/// Minimal HTTP over a socket: no client dependency, and the page's needs are
/// one GET, one POST and one stream.
async fn http(home: &std::path::Path, port: u16, request: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    s.write_all(request.as_bytes()).await.unwrap();
    s.flush().await.unwrap();
    let mut raw = Vec::new();
    if let Err(e) = s.read_to_end(&mut raw).await {
        // A reset here once failed CI with nothing else to go on: say which
        // request it was and what the server logged.
        let stderr = std::fs::read_to_string(home.join("web.stderr")).unwrap_or_default();
        panic!(
            "{e} on `{}`; server stderr:\n{stderr}",
            request.lines().next().unwrap_or_default()
        );
    }
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

fn get(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n\r\n")
}

fn post(path: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

/// A server whose working directory is a fresh git repo with one commit
/// (`project/`, inside the temp HOME). Returns the repo root too.
async fn start_web() -> (
    tokio::process::Child,
    u16,
    String,
    tempfile::TempDir,
    String,
) {
    start_web_with(&[]).await
}

async fn start_web_with(
    env: &[(&str, &str)],
) -> (
    tokio::process::Child,
    u16,
    String,
    tempfile::TempDir,
    String,
) {
    let home = tempfile::tempdir().unwrap();
    let repo = home.path().join("project");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "base",
        ],
    );
    let root = repo.canonicalize().unwrap().display().to_string();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hasp"));
    cmd.current_dir(&repo)
        .arg("web")
        .arg("0")
        .env("HOME", home.path())
        .env("HASP_PROVIDER", "mock")
        .env("HASP_SOCKET", home.path().join("web.sock"))
        .envs(env.iter().copied())
        .stdout(std::process::Stdio::piped())
        // Kept in a file so a failure can say what the server said.
        .stderr(std::fs::File::create(home.path().join("web.stderr")).unwrap());
    let mut child = cmd.spawn().expect("spawn harness web");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(60), lines.next_line())
        .await
        .expect("web server prints its URL")
        .unwrap()
        .expect("a URL line");
    // harness web → http://127.0.0.1:PORT/?token=TOKEN&root=ROOT
    let rest = line
        .split("http://127.0.0.1:")
        .nth(1)
        .expect("URL in banner");
    let port: u16 = rest.split('/').next().unwrap().parse().expect("port");
    let token = rest
        .split("token=")
        .nth(1)
        .expect("token in banner")
        .split('&')
        .next()
        .unwrap()
        .trim()
        .to_string();
    (child, port, token, home, root)
}

async fn rpc(
    home: &std::path::Path,
    port: u16,
    token: &str,
    root: &str,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let body = serde_json::json!({"root": root, "method": method, "params": params});
    let (status, body) = http(
        home,
        port,
        &post(&format!("/api/rpc?token={token}"), &body.to_string()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

async fn get_json(home: &std::path::Path, port: u16, path: &str) -> serde_json::Value {
    let (status, body) = http(home, port, &get(path)).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn serves_the_app_and_drives_a_turn() {
    let (mut child, port, token, home, root) = start_web().await;
    let h = home.path();

    // No token, no service — a local port is not an authorization.
    let (status, _) = http(h, port, &get("/")).await;
    assert_eq!(status, 401);

    let (status, page) = http(h, port, &get(&format!("/?token={token}"))).await;
    assert_eq!(status, 200);
    assert!(
        page.contains("id=\"composer\"") && page.contains("id=\"sidebar\""),
        "the app is served"
    );

    // A project opens on first use: stats answer.
    let v = rpc(
        h,
        port,
        &token,
        &root,
        "session/stats",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(v["ok"], true, "{v}");
    assert!(v["result"]["model"].as_str().is_some(), "{v}");

    // A prompt runs the real turn through the daemon.
    let v = rpc(
        h,
        port,
        &token,
        &root,
        "session/prompt",
        serde_json::json!({"prompt": "hi app"}),
    )
    .await;
    assert_eq!(v["ok"], true, "{v}");
    assert!(
        v["result"]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("echo: hi app"),
        "{v}"
    );

    // The sidebar lists it under its project, and its transcript replays.
    let v = get_json(
        h,
        port,
        &format!(
            "/api/projects?token={token}&root={}",
            root.replace(' ', "%20")
        ),
    )
    .await;
    let project = v["result"]["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["root"] == root.as_str())
        .unwrap_or_else(|| panic!("project listed: {v}"))
        .clone();
    let first = &project["sessions"][0];
    assert_eq!(first["title"], "hi app", "{project}");
    let path = first["path"].as_str().unwrap().to_string();
    let v = get_json(
        h,
        port,
        &format!(
            "/api/transcript?token={token}&path={}",
            path.replace(' ', "%20")
        ),
    )
    .await;
    let items = v["result"]["items"].as_array().unwrap();
    assert_eq!(items[0]["kind"], "user");
    assert!(items.iter().any(|i| i["kind"] == "assistant"), "{v}");

    // A new session in place, then back to the first one.
    let v = rpc(h, port, &token, &root, "session/new", serde_json::json!({})).await;
    assert_eq!(v["ok"], true, "{v}");
    let fresh = v["result"]["sessionId"].as_str().unwrap().to_string();
    assert_ne!(fresh, first["id"].as_str().unwrap());
    let v = rpc(
        h,
        port,
        &token,
        &root,
        "session/load",
        serde_json::json!({"path": path}),
    )
    .await;
    assert_eq!(v["ok"], true, "{v}");
    let v = rpc(
        h,
        port,
        &token,
        &root,
        "session/stats",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(v["result"]["sessionId"], first["id"], "{v}");

    // Changes: the working tree against HEAD, diffed.
    std::fs::write(std::path::Path::new(&root).join("a.txt"), "one\nTWO\n").unwrap();
    let v = get_json(
        h,
        port,
        &format!(
            "/api/changes?token={token}&root={}",
            root.replace(' ', "%20")
        ),
    )
    .await;
    let f = &v["result"]["files"][0];
    assert_eq!(
        (
            f["path"].as_str(),
            f["added"].as_u64(),
            f["removed"].as_u64()
        ),
        (Some("a.txt"), Some(1), Some(1)),
        "{v}"
    );
    assert!(
        f["hunks"][0]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l[0] == "add" && l[3] == "TWO"),
        "{v}"
    );

    let _ = child.kill().await;
}

#[tokio::test]
async fn browses_workspace_files_with_confined_bounded_previews() {
    let (mut child, port, token, home, root) = start_web().await;
    let h = home.path();
    let repo = std::path::Path::new(&root);
    std::fs::create_dir(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/hello world.txt"), "<script>hello</script>\n").unwrap();
    std::fs::write(repo.join("binary.bin"), [0, 255, 1]).unwrap();
    std::fs::write(repo.join("large.txt"), vec![b'x'; 300_000]).unwrap();
    std::os::unix::fs::symlink(h, repo.join("escape")).unwrap();
    let base = format!("token={token}&root={}", root.replace(' ', "%20"));
    let (status, _) = http(h, port, &get("/api/files")).await;
    assert_eq!(status, 401);
    let listing = get_json(h, port, &format!("/api/files?{base}")).await;
    let entries = listing["result"]["entries"].as_array().unwrap();
    assert_eq!(entries[0]["name"], "src");
    assert!(entries.iter().any(|e| e["name"] == "a.txt"));
    assert!(
        !entries
            .iter()
            .any(|e| e["name"] == ".git" || e["name"] == "escape")
    );
    let nested = get_json(h, port, &format!("/api/files?{base}&path=src")).await;
    assert_eq!(
        nested["result"]["entries"][0]["path"],
        "src/hello world.txt"
    );
    let text = get_json(
        h,
        port,
        &format!("/api/file?{base}&path=src/hello%20world.txt"),
    )
    .await;
    assert_eq!(text["result"]["text"], "<script>hello</script>\n");
    let binary = get_json(h, port, &format!("/api/file?{base}&path=binary.bin")).await;
    assert_eq!(binary["result"]["binary"], true);
    assert!(binary["result"]["text"].is_null());
    let large = get_json(h, port, &format!("/api/file?{base}&path=large.txt")).await;
    assert_eq!(large["result"]["truncated"], true);
    assert_eq!(large["result"]["text"].as_str().unwrap().len(), 256 * 1024);
    for path in [
        "../outside",
        "%2Fetc%2Fpasswd",
        "escape/web.stderr",
        "missing",
        "src",
    ] {
        let denied = get_json(h, port, &format!("/api/file?{base}&path={path}")).await;
        assert_eq!(denied["ok"], false, "{path}: {denied}");
    }
    let denied = get_json(h, port, &format!("/api/files?{base}&path=escape")).await;
    assert_eq!(denied["ok"], false);

    // Catalog uses the same discovery as the agent, without exposing skill bodies.
    let skill_dir = repo.join(".harness/skills/smoke");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: smoke\ndescription: Smoke skill\n---\nPrivate body marker\n",
    )
    .unwrap();
    let (status, _) = http(h, port, &get("/api/skills")).await;
    assert_eq!(status, 401);
    let skills = get_json(h, port, &format!("/api/skills?{base}")).await;
    assert_eq!(
        skills["result"]["skills"],
        serde_json::json!([{"name": "smoke", "description": "Smoke skill"}])
    );
    let denied = get_json(h, port, &format!("/api/skills?token={token}")).await;
    assert_eq!(denied["ok"], false);
    let _ = child.kill().await;
}

#[tokio::test]
async fn streams_assistant_deltas_over_sse() {
    let (mut child, port, token, _home, root) = start_web().await;

    // Open the event stream, then prompt on a second connection: the deltas
    // must arrive on the stream while the turn is still running.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(
            get(&format!(
                "/events?token={token}&root={}",
                root.replace(' ', "%20")
            ))
            .as_bytes(),
        )
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let mut head = [0u8; 256];
    let n = stream.read(&mut head).await.unwrap();
    assert!(
        String::from_utf8_lossy(&head[..n]).contains("200 OK"),
        "event stream opened"
    );

    let prompt = serde_json::json!({"root": root, "method": "session/prompt", "params": {"prompt": "stream me"}});
    let mut s2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s2.write_all(post(&format!("/api/rpc?token={token}"), &prompt.to_string()).as_bytes())
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

/// The old page could only deny approvals; the app asks and answers them.
#[tokio::test]
async fn approvals_are_asked_in_the_app_and_answered_back() {
    let script = serde_json::json!([
        {"tool": "bash", "input": {"command": "touch approved.txt"}},
        {"text": "done"}
    ])
    .to_string();
    let (mut child, port, token, home, root) =
        start_web_with(&[("HASP_MOCK_SCRIPT", &script)]).await;
    let enc = root.replace(' ', "%20");

    let mut events = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    events
        .write_all(get(&format!("/events?token={token}&root={enc}")).as_bytes())
        .await
        .unwrap();

    // The prompt blocks until the approval is answered, so it runs aside.
    let prompt = serde_json::json!({
        "root": root, "method": "session/prompt", "params": {"prompt": "make the file"}
    });
    let prompt_path = format!("/api/rpc?token={token}");
    let turn = tokio::spawn(async move {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(post(&prompt_path, &prompt.to_string()).as_bytes())
            .await
            .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out).to_string()
    });

    let mut seen = String::new();
    let id = loop {
        let mut buf = [0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(30), events.read(&mut buf))
            .await
            .expect("the question arrives")
            .unwrap();
        assert!(n > 0, "stream closed: {seen}");
        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        if let Some(line) = seen.lines().find(|l| l.contains("permission/request")) {
            let v: serde_json::Value =
                serde_json::from_str(line.trim_start_matches("data: ")).unwrap();
            let q = v["params"]["question"].as_str().unwrap_or_default();
            assert!(q.contains("touch approved.txt"), "{v}");
            break v["params"]["id"].as_u64().unwrap();
        }
    };
    let answer = serde_json::json!({"root": root, "id": id, "answer": "y"});
    let (status, body) = http(
        home.path(),
        port,
        &post(
            &format!("/api/permission?token={token}"),
            &answer.to_string(),
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let reply = tokio::time::timeout(Duration::from_secs(30), turn)
        .await
        .expect("the turn finishes once approved")
        .unwrap();
    assert!(reply.contains("\"ok\":true"), "{reply}");
    assert!(
        std::path::Path::new(&root).join("approved.txt").exists(),
        "the approved command ran"
    );
    let _ = child.kill().await;
}

/// Whoever reads the banner may close the pipe (`harness web | head -1`). A
/// `println!` into the closed pipe used to panic and take the server down,
/// which on Linux CI surfaced as a connection reset in the test above.
#[tokio::test]
async fn server_survives_its_stdout_being_closed() {
    let home = tempfile::tempdir().unwrap();
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_hasp"))
        .arg("web")
        .arg(port.to_string())
        .env("HOME", home.path())
        .env("HASP_PROVIDER", "mock")
        .env("HASP_SOCKET", home.path().join("web.sock"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::fs::File::create(home.path().join("web.stderr")).unwrap())
        .spawn()
        .expect("spawn harness web");
    drop(child.stdout.take()); // closed before the banner is written
    let mut status = 0;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            status = http(home.path(), port, &get("/")).await.0;
            break;
        }
    }
    assert_eq!(status, 401, "server is up and answering (no token → 401)");
    assert!(child.try_wait().unwrap().is_none(), "and still running");
    let _ = child.kill().await;
}

async fn post_json(
    home: &std::path::Path,
    port: u16,
    token: &str,
    path: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let (status, body) = http(
        home,
        port,
        &post(&format!("{path}?token={token}"), &body.to_string()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// A worktree is its own project: its own resident session (so it can run
/// beside the main checkout), its changes measured from where it branched,
/// and it lands back into the parent as a merge.
#[tokio::test]
async fn worktrees_run_beside_the_checkout_and_land_back() {
    let (mut child, port, token, home, root) = start_web().await;
    let h = home.path();
    let repo = std::path::Path::new(&root);
    for (k, v) in [("user.name", "t"), ("user.email", "t@t")] {
        git(repo, &["config", k, v]);
    }

    let v = post_json(
        h,
        port,
        &token,
        "/api/worktree",
        serde_json::json!({"root": root, "name": "side/../x y"}),
    )
    .await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["result"]["name"], "sidexy", "names are sanitized: {v}");
    let wt = v["result"]["root"].as_str().unwrap().to_string();

    // Its own resident: a turn there reports the worktree's root.
    let v = rpc(
        h,
        port,
        &token,
        &wt,
        "session/prompt",
        serde_json::json!({"prompt": "hi wt"}),
    )
    .await;
    assert_eq!(v["ok"], true, "{v}");
    let v = rpc(h, port, &token, &wt, "session/stats", serde_json::json!({})).await;
    assert_eq!(v["result"]["root"], wt.as_str(), "{v}");
    assert!(v["result"]["efforts"].is_array(), "{v}");

    // Work in the worktree, committed and not: both show against the fork point.
    let wtp = std::path::Path::new(&wt);
    std::fs::write(wtp.join("b.txt"), "side\n").unwrap();
    git(wtp, &["add", "b.txt"]);
    git(
        wtp,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "b",
        ],
    );
    std::fs::write(wtp.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let v = get_json(
        h,
        port,
        &format!("/api/changes?token={token}&root={}", wt.replace(' ', "%20")),
    )
    .await;
    let paths: Vec<&str> = v["result"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect();
    assert!(paths.contains(&"a.txt") && paths.contains(&"b.txt"), "{v}");
    assert_eq!(v["result"]["worktree"]["parent"], root.as_str(), "{v}");

    // Landing merges exactly that into the parent's branch.
    let v = post_json(
        h,
        port,
        &token,
        "/api/land",
        serde_json::json!({"root": wt, "message": "side work"}),
    )
    .await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(
        std::fs::read_to_string(repo.join("b.txt")).unwrap(),
        "side\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );

    // Nothing left to land is said plainly, not committed empty.
    let v = post_json(
        h,
        port,
        &token,
        "/api/land",
        serde_json::json!({"root": root, "message": "again"}),
    )
    .await;
    assert_eq!(v["ok"], false, "{v}");

    // Removal after landing needs no confirmation: nothing would be lost.
    let v = post_json(
        h,
        port,
        &token,
        "/api/worktree/remove",
        serde_json::json!({"root": wt}),
    )
    .await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["result"]["discarded"], false, "{v}");
    assert_eq!(v["result"]["merged"], true, "{v}");
    assert!(!wtp.exists());

    // Unlanded work is not removed without saying so.
    let v = post_json(
        h,
        port,
        &token,
        "/api/worktree",
        serde_json::json!({"root": root, "name": "scrap"}),
    )
    .await;
    let scrap = v["result"]["root"].as_str().unwrap().to_string();
    std::fs::write(std::path::Path::new(&scrap).join("c.txt"), "draft\n").unwrap();
    let v = post_json(
        h,
        port,
        &token,
        "/api/worktree/remove",
        serde_json::json!({"root": scrap}),
    )
    .await;
    assert_eq!(v["ok"], false, "{v}");
    let v = post_json(
        h,
        port,
        &token,
        "/api/worktree/remove",
        serde_json::json!({"root": scrap, "discard": true}),
    )
    .await;
    assert_eq!(v["result"]["discarded"], true, "{v}");

    // Effort: the mock provider takes none, and says so.
    let v = rpc(
        h,
        port,
        &token,
        &root,
        "effort/set",
        serde_json::json!({"effort": "high"}),
    )
    .await;
    assert_eq!(v["ok"], false, "{v}");
    let _ = child.kill().await;
}
