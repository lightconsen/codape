//! The monitor guardian, driven through the real binary.
//!
//! The unit tests exercise `run_guardian` in-process. This one proves the
//! hidden subcommand is wired in `main` before tracing and clap: the binary is
//! started with `--internal-monitor-guardian`, records a live child over
//! stdin, and must kill it when stdin closes. Windows has no guardian (the Job
//! Object is kill-on-close), so the test is Unix-only.
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn the_guardian_kills_a_recorded_child_when_stdin_closes() {
    let mut child = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the child to record");
    let start = codape_core::process_tree::process_start_time(child.id())
        .expect("the child has a start time");

    let mut guardian = Command::new(env!("CARGO_BIN_EXE_codape"))
        .arg("--internal-monitor-guardian")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the guardian");
    {
        let stdin = guardian.stdin.as_mut().expect("guardian stdin");
        writeln!(stdin, "pid {} {}", child.id(), start).expect("send the record");
    }
    // EOF is the signal: the host is gone.
    drop(guardian.stdin.take());

    let status = guardian.wait().expect("wait for the guardian");
    assert!(status.success(), "guardian exited with {status}");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the recorded child survived the guardian"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
