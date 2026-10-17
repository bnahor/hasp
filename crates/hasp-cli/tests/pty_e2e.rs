//! PTY-level e2e: the real binary, a real terminal, the mock provider.

use portable_pty::{CommandBuilder, native_pty_system};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn spawn_reader(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<u8> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for &b in &buf[..n] {
                        if tx.send(b).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
    rx
}

fn wait_for(rx: &mpsc::Receiver<u8>, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    // The TUI redraws per changed cell, so streamed text arrives one character
    // wrapped in cursor-move escapes; strip them so needles match what a
    // human would read off the screen.
    let mut acc = String::new();
    // ... and compare without spaces: the TUI sometimes draws a space as a
    // cursor jump instead of a byte, so words are the reliable signal.
    let mut acc_words = String::new();
    let needle_words: String = needle.chars().filter(|c| !c.is_whitespace()).collect();
    let mut in_csi = false;
    let mut in_esc = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(b) => {
                if in_csi {
                    if (0x40..=0x7e).contains(&b) {
                        in_csi = false;
                    }
                } else if in_esc {
                    in_esc = false;
                    if b == b'[' {
                        in_csi = true;
                    }
                } else if b == 0x1b {
                    in_esc = true;
                } else {
                    acc.push(b as char);
                    if !b.is_ascii_whitespace() {
                        acc_words.push(b as char);
                    }
                }
                if acc_words.contains(&needle_words) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    panic!("did not see {needle:?} within {timeout:?}; output so far:\n{acc}");
}

#[test]
fn plain_repl_mock_roundtrip_over_pty() {
    let home = tempfile::tempdir().unwrap();
    let pty = native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 30,
            cols: 120,
            ..Default::default()
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_hasp"));
    cmd.args(["--plain", "--provider", "mock"]);
    cmd.env("HOME", home.path());
    cmd.cwd(home.path());

    let mut child = pty.slave.spawn_command(cmd).unwrap();
    let reader = pty.master.try_clone_reader().unwrap();
    let rx = spawn_reader(reader);
    let mut writer = pty.master.take_writer().unwrap();

    wait_for(&rx, "plain REPL", Duration::from_secs(20));
    writer.write_all(b"hello\n").unwrap();
    writer.flush().unwrap();
    wait_for(&rx, "echo: hello", Duration::from_secs(20));
    writer.write_all(b"/exit\n").unwrap();
    writer.flush().unwrap();

    let status = child.wait().expect("child exits");
    assert!(status.success(), "exit status: {status:?}");
}

#[test]
fn tui_auto_spawns_daemon_and_attaches() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("auto.sock");
    let pty = native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 30,
            cols: 120,
            ..Default::default()
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_hasp"));
    cmd.arg("--provider");
    cmd.arg("mock");
    cmd.env("HOME", home.path());
    cmd.env("HASP_SOCKET", &sock);
    cmd.cwd(home.path());

    let mut child = pty.slave.spawn_command(cmd).unwrap();
    let reader = pty.master.try_clone_reader().unwrap();
    let rx = spawn_reader(reader);
    let mut writer = pty.master.take_writer().unwrap();

    // the TUI attaches through the daemon it spawned: wait for the session
    // note, then quit; the daemon must outlive the TUI.
    wait_for(&rx, "via daemon", Duration::from_secs(30));
    writer.write_all(b"/exit\r").unwrap();
    writer.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(status.success(), "exit status: {status:?}");
                break;
            }
            Ok(None) => {
                assert!(Instant::now() < deadline, "TUI did not exit after /exit");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("wait failed: {e}"),
        }
    }

    // daemon outlives the TUI: socket present and accepting connections
    assert!(sock.exists(), "daemon socket survives TUI exit");
    let mut client =
        std::os::unix::net::UnixStream::connect(&sock).expect("daemon accepts after TUI exit");
    use std::io::{BufRead, BufReader, Write};
    let hello = "{\"id\":1,\"method\":\"initialize\",\"params\":{}}\n";
    client.write_all(hello.as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(client).read_line(&mut line).unwrap();
    assert!(
        line.contains("\"agent\":\"hasp\""),
        "daemon responds: {line}"
    );
}

/// Reopening the TUI in a workspace must adopt the still-running session and
/// put the previous turns on screen — not greet a live resident with a blank
/// transcript.
#[test]
fn tui_replays_history_when_reopening() {
    let home = tempfile::tempdir().unwrap();
    // One daemon for both runs, so the second one adopts its resident.
    let sock = home.path().join("h.sock");

    let run = |prompt_replies: &[(&str, &str)]| {
        let pty = native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 30,
                cols: 120,
                ..Default::default()
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_hasp"));
        cmd.args(["--provider", "mock"]);
        cmd.env("HOME", home.path());
        cmd.env("HASP_SOCKET", &sock);
        cmd.cwd(home.path());
        let mut child = pty.slave.spawn_command(cmd).unwrap();
        let rx = spawn_reader(pty.master.try_clone_reader().unwrap());
        let mut writer = pty.master.take_writer().unwrap();
        wait_for(&rx, "via daemon", Duration::from_secs(30));
        for (send, expect) in prompt_replies {
            // `\r`, not `\n`: in raw mode a line feed reads as ctrl+j (the
            // editor's newline key), so only a carriage return submits.
            writer.write_all(format!("{send}\r").as_bytes()).unwrap();
            writer.flush().unwrap();
            wait_for(&rx, expect, Duration::from_secs(30));
        }
        writer.write_all(b"/exit\r").unwrap();
        writer.flush().unwrap();
        let status = child.wait().expect("child exits");
        assert!(status.success(), "exit status: {status:?}");
    };

    run(&[("remember this", "echo: remember this")]);
    run(&[]);

    // The assertions are the second run's startup output, so drive one more
    // run that only reads: everything below must already have been printed.
    let pty = native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 30,
            cols: 120,
            ..Default::default()
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_hasp"));
    cmd.args(["--provider", "mock"]);
    cmd.env("HOME", home.path());
    cmd.env("HASP_SOCKET", &sock);
    cmd.cwd(home.path());
    let mut child = pty.slave.spawn_command(cmd).unwrap();
    let rx = spawn_reader(pty.master.try_clone_reader().unwrap());
    let mut writer = pty.master.take_writer().unwrap();
    wait_for(
        &rx,
        "attached to the running session",
        Duration::from_secs(30),
    );
    wait_for(&rx, "showing last", Duration::from_secs(30));
    wait_for(&rx, "remember this", Duration::from_secs(30));
    writer.write_all(b"/exit\r").unwrap();
    writer.flush().unwrap();
    let status = child.wait().expect("child exits");
    assert!(status.success(), "exit status: {status:?}");
}
