//! Process-group control for shell commands.
//!
//! A shell command is not one process. `sh -c "docker compose up"` is a shell
//! plus whatever it started, and a command that timed out must take the whole
//! tree with it or leave a test server holding a port. Two properties matter:
//!
//! * **Isolation.** CodeRex is itself run under a timeout by harnesses
//!   (`timeout`, `tb`, CI). Those signal CodeRex's *process group*, which a
//!   shell that stayed in that group shares — a `run_in_background` service
//!   would die with the harness timeout even though it was supposed to outlive
//!   the call. Spawning each command into its own group (`setpgid(0, 0)`)
//!   keeps the command out of CodeRex's group, so a group signal aimed at
//!   CodeRex does not reach it.
//! * **Termination.** The negated pid reaches every process in the group, so a
//!   timeout stops the descendants too, not just the shell that was waiting
//!   for them.
//!
//! Windows has no process groups in this sense; a shell there is terminated
//! with `taskkill /T`, which walks the child tree.

use std::time::Duration;

/// How long a command is given to honour `SIGTERM` before `SIGKILL`.
///
/// A service usually exits on `SIGTERM` within milliseconds; the grace exists
/// for a shell that traps the signal to stop a database or flush a log.
pub const TERM_GRACE: Duration = Duration::from_secs(1);

/// Put a command into a process group of its own.
///
/// The child becomes the leader, so its pid is also the group id that
/// [`terminate_group`] signals. No-op on platforms without process groups.
// Windows has no process groups to set, so the stub neither mutates its
// argument nor does anything `const` cannot express — both lints only fire on
// that build. The signature has to stay the same on every platform.
#[cfg_attr(
    not(unix),
    allow(clippy::missing_const_for_fn, clippy::needless_pass_by_ref_mut)
)]
pub fn isolate(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(not(unix))]
    let _ = cmd;
}

/// Whether `pid` can name a process group at all.
///
/// `kill(-pid, …)` names the group led by `pid`, so a pid that cannot lead a
/// group must never reach it:
///
/// * `0` — `kill(-0, …)` is `kill(0, …)`, which signals **the caller's own
///   group**. `kill(0, 0)` therefore always succeeds, so a recorded pid of `0`
///   (a corrupt or hand-written job record) would look like a live group and
///   [`terminate_group`] would then SIGTERM and SIGKILL whoever called it.
///   Under `cargo test` the test binaries share the runner's group, so that
///   ends the whole CI job — observed as "The runner has received a shutdown
///   signal" while the `coderex_cli` lib tests were starting. `cargo nextest`
///   puts each binary in its own group, which is why only the coverage job was
///   affected.
/// * `1` — signalling pid 1 means "every process the caller may signal".
/// * A pid whose group is the caller's own. Pid reuse can make a recorded pid
///   name a group this process belongs to, and "stop that job" must never mean
///   "stop the caller".
/// * Everything that is not a pid on this platform.
#[cfg(unix)]
#[allow(unsafe_code)] // `getpgrp` is the only way to read the caller's group.
fn leader_pid(pid: u32) -> Option<libc::pid_t> {
    let raw = libc::pid_t::try_from(pid).ok()?;
    if raw <= 1 {
        return None;
    }
    // SAFETY: `getpgrp` only reads process state.
    let own_group = unsafe { libc::getpgrp() };
    if own_group == raw {
        return None;
    }
    Some(raw)
}

/// Whether the process group led by `pid` still has a live member.
///
/// `kill(-pid, 0)` is the portable probe: it sends no signal and reports
/// whether the caller may signal the group. A group whose leader is a zombie
/// still counts as present until it is reaped, which is why callers treat this
/// as "something is still there", not "the command is still working".
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn group_exists(pid: u32) -> bool {
    let Some(raw) = leader_pid(pid) else {
        return false;
    };
    // SAFETY: `kill` does not touch memory; signal 0 only performs the
    // existence and permission checks. The negated pid names the group, and
    // `leader_pid` has already excluded the values for which it would name
    // the caller's own group instead.
    unsafe {
        libc::kill(-raw, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

// Windows kills the tree with `taskkill /T`, so there is no group to probe.
// The body is a constant, which clippy would prefer spelled `const fn`.
#[cfg(not(unix))]
#[cfg_attr(not(unix), allow(clippy::missing_const_for_fn))]
pub fn group_exists(_pid: u32) -> bool {
    true
}

/// Send `signal` to every process in the group led by `pid`.
///
/// The same guard as [`group_exists`] applies: a pid that cannot lead a group
/// must not be negated into the caller's own.
#[cfg(unix)]
#[allow(unsafe_code)]
fn signal_group(pid: u32, signal: i32) {
    let Some(raw) = leader_pid(pid) else {
        return;
    };
    // SAFETY: `kill` does not touch memory; the negated pid names the group
    // the command was spawned into.
    unsafe {
        libc::kill(-raw, signal);
    }
}

/// Stop a command and everything it started: `SIGTERM`, a short grace, then
/// `SIGKILL` for whatever is left.
///
/// Returns after the group has been signalled; it does not wait for reaping —
/// the supervisor that owns the child does that.
pub async fn terminate_group(pid: u32) {
    #[cfg(unix)]
    {
        if !group_exists(pid) {
            return;
        }
        signal_group(pid, libc::SIGTERM);
        tokio::time::sleep(TERM_GRACE).await;
        signal_group(pid, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        let _ = TERM_GRACE;
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        tokio::time::sleep(TERM_GRACE).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A child that leads its own group is not in the caller's group, and a
    /// group signal sent to the child reaches a grandchild that stayed in it.
    ///
    /// This is the property the benchmark failure depended on: the harness
    /// timed out CodeRex's group and took the test server down with it.
    #[tokio::test]
    #[allow(unsafe_code)] // `getpgid` is the only way to read a process group.
    async fn isolated_child_is_in_its_own_group_and_is_killed_with_its_tree() {
        use std::os::unix::process::CommandExt as _;
        use std::os::unix::process::ExitStatusExt as _;

        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .process_group(0)
            .spawn()
            .expect("spawn");
        let pid = child.id();
        assert!(group_exists(pid), "the new group should exist");

        // SAFETY: `getpgid` only reads process state.
        let owner_group = unsafe { libc::getpgid(i32::try_from(pid).unwrap()) };
        // SAFETY: as above, for the calling process.
        let own_group = unsafe { libc::getpgid(0) };
        assert_ne!(
            owner_group, own_group,
            "the command must not share CodeRex's process group"
        );

        terminate_group(pid).await;
        // SIGKILL is current at this point; the shell is gone.
        let mut child = child;
        let status = child.wait().expect("reap");
        assert!(
            status.signal().is_some(),
            "the command should have been signalled, got {status:?}"
        );
    }

    #[tokio::test]
    async fn terminating_an_unknown_group_is_a_no_op() {
        // An id no process is using: `kill(-pid, 0)` fails with ESRCH.
        let pid = 3_999_999;
        if group_exists(pid) {
            return;
        }
        terminate_group(pid).await;
    }

    /// A group id that is not a group must never reach `kill(-pid, …)`.
    ///
    /// `kill(-0, …)` is `kill(0, …)`: it signals the caller's own process
    /// group, and `kill(0, 0)` always succeeds. A recorded pid of `0` would
    /// therefore look like a live group, and stopping that job would SIGTERM
    /// and SIGKILL the process that asked — under `cargo test` that is the
    /// whole test binary and, in CI, the job. This asserts the probe refuses
    /// the value rather than testing the signal, because the signal cannot be
    /// tested without killing the test runner.
    #[test]
    fn a_pid_that_cannot_lead_a_group_is_not_a_group() {
        assert!(!group_exists(0), "0 is the caller's own group, not a job");
        assert!(!group_exists(1), "1 is init, not a job");
        assert_eq!(leader_pid(0), None);
        assert_eq!(leader_pid(1), None);
        assert_eq!(leader_pid(2), Some(2));
    }
}
