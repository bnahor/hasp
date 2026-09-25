//! Environments end to end with real Docker: an environment's limits are
//! enforced, a session's tools run inside it through the guest agent, the
//! workspace is shared, and nothing is left behind.
//!
//! Skipped (not failed) when Docker is unavailable or there is no guest
//! binary for its architecture: on Linux the test's own `harness` build is the
//! guest (run in an image with the same C library as the host), elsewhere the
//! one `harness env build-guest` put in `~/.harness/guest/`.

use std::path::{Path, PathBuf};
use std::process::Command;

const IMAGE: &str = "ubuntu:24.04";

fn guest_binary() -> Option<PathBuf> {
    let out = Command::new("docker")
        .args(["info", "--format", "{{.Architecture}}"])
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!("skipping: Docker is not running");
        return None;
    }
    let arch = harness_env::guest::normalize_arch(String::from_utf8_lossy(&out.stdout).trim())?;
    let own = PathBuf::from(env!("CARGO_BIN_EXE_harness"));
    if harness_env::guest::elf_arch(&own) == Some(arch) {
        return Some(own);
    }
    let home = std::env::var_os("HOME")?;
    let built = Path::new(&home).join(format!(".harness/guest/harness-linux-{arch}"));
    if harness_env::guest::elf_arch(&built) == Some(arch) {
        return Some(built);
    }
    eprintln!("skipping: no guest agent for linux/{arch} (harness env build-guest)");
    None
}

/// A git workspace with one environment definition, and an isolated HOME.
fn workspace() -> (tempfile::TempDir, tempfile::TempDir) {
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(ws.path())
        .status()
        .unwrap();
    std::fs::create_dir_all(ws.path().join(".harness/environments")).unwrap();
    std::fs::write(
        ws.path().join(".harness/environments/box.toml"),
        format!(
            "image = \"{IMAGE}\"\ncpus = 1\nmemory = \"256m\"\npids = 100\nnetwork = \"none\"\n\
             setup = [\"echo prepared > /tmp/setup-mark\"]\n[env]\nBOX_MODE = \"e2e\"\n"
        ),
    )
    .unwrap();
    (ws, home)
}

fn harness(ws: &Path, home: &Path, guest: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_harness"));
    c.current_dir(ws)
        .env("HOME", home)
        .env("HARNESS_GUEST_BINARY", guest)
        .env_remove("HARNESS_ENV");
    c
}

fn containers_for(ws: &Path) -> usize {
    let root = ws.canonicalize().unwrap();
    let out = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label=harness.workspace={}", root.display()),
            "-q",
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().count()
}

#[test]
fn check_reports_the_limits_the_kernel_enforces() {
    let Some(guest) = guest_binary() else { return };
    let (ws, home) = workspace();
    let out = harness(ws.path(), home.path(), &guest)
        .args(["env", "check", "box"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("cpus    100000 100000"), "1 cpu: {text}");
    assert!(text.contains("memory  268435456"), "256m: {text}");
    assert!(text.contains("pids    100"), "{text}");
    assert!(text.contains("network none (no route out)"), "{text}");
    assert!(text.contains("bash in the workspace → ok"), "{text}");
    assert_eq!(containers_for(ws.path()), 0, "check leaves nothing behind");
}

#[test]
fn a_session_in_an_environment_runs_its_tools_inside() {
    let Some(guest) = guest_binary() else { return };
    let (ws, home) = workspace();
    let script = serde_json::json!([
        {"tools": [
            {"name": "write", "input": {"path": "from-agent.txt", "content": "written inside\n"}},
            {"name": "bash", "input": {"command":
                "test -f /.dockerenv && echo in-container; cat /tmp/setup-mark; echo mode=$BOX_MODE; \
                 cat from-agent.txt; getent hosts example.com || echo offline"}}
        ]},
        {"text": "done"}
    ]);
    let out = harness(ws.path(), home.path(), &guest)
        .args(["--provider", "mock", "--env", "box", "-p", "go", "--yolo"])
        .env("HARNESS_MOCK_SCRIPT", script.to_string())
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{all}");
    assert!(all.contains("▪ environment box:"), "{all}");
    assert!(
        all.contains("in-container"),
        "bash ran inside the container: {all}"
    );
    assert!(all.contains("prepared"), "setup ran: {all}");
    assert!(all.contains("mode=e2e"), "[env] applied: {all}");
    assert!(
        all.contains("written inside"),
        "the guest's write is visible to its bash: {all}"
    );
    assert!(all.contains("offline"), "network = none: {all}");
    assert_eq!(
        std::fs::read_to_string(ws.path().join("from-agent.txt")).unwrap(),
        "written inside\n",
        "the workspace is shared with the host"
    );
    assert_eq!(
        containers_for(ws.path()),
        0,
        "the session's environment is torn down"
    );
}

#[test]
fn an_unknown_environment_is_a_clear_error() {
    let Some(guest) = guest_binary() else { return };
    let (ws, home) = workspace();
    let out = harness(ws.path(), home.path(), &guest)
        .args(["--provider", "mock", "--env", "nope", "-p", "go"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no environment named nope"), "{err}");
}
