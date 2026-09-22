//! `harness sessions`: listing, pruning, removing — and the guard that refuses
//! to delete a journal a running daemon is holding.

use std::time::Duration;
use tokio::process::Command;

fn harness(home: &std::path::Path, cwd: &std::path::Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_harness"));
    c.args(args)
        .env("HOME", home)
        .env("HARNESS_PROVIDER", "mock")
        .env("HARNESS_OFFLINE", "1") // no catalog fetch during tests
        // The daemon in these tests listens here; `sessions` must look at the
        // same socket to see what is held.
        .env("HARNESS_SOCKET", home.join("d.sock"))
        .current_dir(cwd);
    c
}

async fn run(
    home: &std::path::Path,
    cwd: &std::path::Path,
    args: &[&str],
) -> (bool, String, String) {
    let out = harness(home, cwd, args).output().await.expect("run");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[tokio::test]
async fn lists_prunes_and_removes_sessions() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();

    for i in 0..3 {
        let prompt = format!("turn {i}");
        let (ok, _, err) = run(
            home.path(),
            work.path(),
            &["-p", &prompt, "--provider", "mock", "--new"],
        )
        .await;
        assert!(ok, "creating a session failed: {err}");
    }

    let (ok, listing, _) = run(home.path(), work.path(), &["sessions"]).await;
    assert!(ok);
    let rows: Vec<&str> = listing.lines().filter(|l| l.contains("msgs")).collect();
    assert_eq!(rows.len(), 3, "three sessions listed:\n{listing}");
    assert!(
        listing.contains("held by a running daemon"),
        "the legend explains the marker"
    );

    // Keep only the newest; the two older files must go.
    let (ok, pruned, _) = run(
        home.path(),
        work.path(),
        &["sessions", "prune", "--keep", "1"],
    )
    .await;
    assert!(ok, "{pruned}");
    assert!(pruned.contains("2 removed, 1 kept"), "{pruned}");

    let (_, listing, _) = run(home.path(), work.path(), &["sessions"]).await;
    let rows: Vec<&str> = listing.lines().filter(|l| l.contains("msgs")).collect();
    assert_eq!(rows.len(), 1, "one session left:\n{listing}");
    // The marker column is a space for unheld sessions, so the id is the first
    // token, not the second.
    let id = rows[0]
        .split_whitespace()
        .next()
        .expect("id column")
        .to_string();

    // …and removing the last one works by id prefix.
    let (ok, removed, err) = run(home.path(), work.path(), &["sessions", "rm", &id[..4]]).await;
    assert!(ok, "{removed}{err}");
    let (_, listing, _) = run(home.path(), work.path(), &["sessions"]).await;
    assert!(listing.contains("no sessions"), "{listing}");
}

#[tokio::test]
async fn refuses_to_remove_a_session_a_daemon_holds() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let sock = home.path().join("d.sock");

    // A daemon, and a session it owns — the client that opened it may leave;
    // the resident is the daemon's.
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("--serve")
        .env("HOME", home.path())
        .env("HARNESS_SOCKET", &sock)
        .env("HARNESS_PROVIDER", "mock")
        .env("HARNESS_OFFLINE", "1")
        .current_dir(work.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon");
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let opened = {
        let mut client = harness_wire::WireClient::connect(&sock)
            .await
            .expect("connect");
        client.initialize().await.expect("initialize");
        client
            .request(
                harness_wire::METHOD_SESSION_CONTINUE,
                serde_json::json!({"cwd": work.path().to_string_lossy()}),
            )
            .await
            .expect("open a session")
    };
    let sid = opened["sessionId"].as_str().expect("sessionId").to_string();

    // The daemon reports it as held, even though no client is attached.
    let (ok, listing, _) = run(home.path(), work.path(), &["sessions"]).await;
    assert!(ok, "{listing}");
    assert!(
        listing
            .lines()
            .any(|l| l.starts_with('*') && l.contains(&sid)),
        "the held session is marked:\n{listing}"
    );

    let (ok, out, err) = run(home.path(), work.path(), &["sessions", "rm", &sid[..4]]).await;
    assert!(!ok, "deleting a held session must fail: {out}{err}");
    assert!(
        err.contains("open in a running daemon"),
        "and say why: {err}"
    );

    // The journal is still there, untouched.
    let (_, listing, _) = run(home.path(), work.path(), &["sessions"]).await;
    assert!(listing.contains(&sid), "{listing}");

    let _ = daemon.kill().await;
}
