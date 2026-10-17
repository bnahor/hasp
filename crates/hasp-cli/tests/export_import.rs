//! `harness export` / `harness import` round-trip against the real binary.

use std::process::Command;

fn harness(home: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_hasp"));
    c.env("HOME", home).env_remove("HASP_SOCKET");
    c
}

#[test]
fn export_then_import_continues_the_session_elsewhere() {
    let origin = tempfile::tempdir().unwrap();
    let home_a = tempfile::tempdir().unwrap();
    let home_b = tempfile::tempdir().unwrap();

    // A turn in workspace A.
    let out = harness(home_a.path())
        .args(["-p", "hello export", "--provider", "mock", "--new"])
        .current_dir(origin.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Export the newest session of that workspace.
    let exported = harness(home_a.path())
        .arg("export")
        .current_dir(origin.path())
        .output()
        .unwrap();
    assert!(exported.status.success());
    let body = String::from_utf8(exported.stdout).unwrap();
    assert!(
        body.contains("\"type\":\"session\""),
        "journal starts with a header"
    );
    assert!(body.contains("hello export"), "contains the original turn");

    let file = origin.path().join("carried.jsonl");
    std::fs::write(&file, &body).unwrap();

    // Import into a different machine-state (fresh HOME): the header's
    // workspace decides placement, so `continue` in the same directory finds
    // it and history replays.
    let imp = harness(home_b.path())
        .args(["import", file.to_string_lossy().as_ref()])
        .current_dir(origin.path())
        .output()
        .unwrap();
    assert!(
        imp.status.success(),
        "{:?}",
        String::from_utf8_lossy(&imp.stderr)
    );

    let again = harness(home_b.path())
        .args(["-p", "second turn", "--provider", "mock"])
        .current_dir(origin.path())
        .output()
        .unwrap();
    assert!(
        again.status.success(),
        "{:?}",
        String::from_utf8_lossy(&again.stderr)
    );
    let check = harness(home_b.path())
        .arg("export")
        .current_dir(origin.path())
        .output()
        .unwrap();
    let text = String::from_utf8(check.stdout).unwrap();
    assert!(
        text.contains("hello export") && text.contains("second turn"),
        "imported history continues in the new state"
    );
}

#[test]
fn import_refuses_garbage_without_placing_anything() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let bogus = dir.path().join("bogus.jsonl");
    std::fs::write(&bogus, "this is not json\n").unwrap();
    let out = harness(home.path())
        .args(["import", bogus.to_string_lossy().as_ref()])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "garbage must be refused");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("session header"), "says why: {err}");
}
