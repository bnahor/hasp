//! Signalling a child's process group, safely.
//!
//! Commands run in their own process group so a timeout or a cancel can stop
//! the whole tree. The signal goes through `kill(2)` directly. Shelling out to
//! `kill -SIG -PGID` is not safe: procps 3.3 (Ubuntu 22.04 and the SWE-bench
//! images) misparses a negative PID after a signal option and signalled the
//! caller's own processes — every `bash` tool call was killing the harness.

/// Send `signal` to process group `pgid`. Refuses 0 and 1: `kill(0, …)` means
/// "my own group" and `kill(-1, …)` "every process I may signal". Returns
/// whether the signal was delivered (false when the group is already gone).
#[cfg(unix)]
pub fn signal_group(pgid: u32, signal: i32) -> bool {
    let Ok(pgid) = i32::try_from(pgid) else {
        return false;
    };
    if pgid <= 1 {
        return false;
    }
    // SAFETY: kill(2) takes no pointers; a negative pid names a process group.
    unsafe { libc::kill(-pgid, signal) == 0 }
}

#[cfg(unix)]
pub use libc::{SIGKILL, SIGTERM};

/// Whether process `pid` exists (it may belong to another user: EPERM means
/// it exists but may not be signalled).
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs the permission and existence checks only.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn alive(_pid: u32) -> bool {
    true
}

#[cfg(not(unix))]
pub fn signal_group(_pgid: u32, _signal: i32) -> bool {
    false
}

#[cfg(not(unix))]
pub const SIGKILL: i32 = 9;
#[cfg(not(unix))]
pub const SIGTERM: i32 = 15;

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn liveness() {
        assert!(alive(std::process::id()));
        assert!(!alive(0));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!alive(pid), "reaped child is gone");
    }

    #[test]
    fn refuses_the_groups_that_mean_everyone() {
        assert!(!signal_group(0, 0));
        assert!(!signal_group(1, 0));
        assert!(
            !signal_group(u32::MAX, 0),
            "out of range, not wrapped to -1"
        );
    }

    #[test]
    fn signals_a_real_group_and_reports_a_missing_one() {
        let mut child = std::process::Command::new("sleep");
        child.arg("30");
        std::os::unix::process::CommandExt::process_group(&mut child, 0);
        let mut child = child.spawn().unwrap();
        let pgid = child.id();
        assert!(signal_group(pgid, SIGKILL));
        assert!(!child.wait().unwrap().success());
        assert!(!signal_group(pgid, SIGKILL), "gone now");
        // The test process itself is untouched (we are still running).
    }
}
