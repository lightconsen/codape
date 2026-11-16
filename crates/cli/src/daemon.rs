//! Daemon state file management for serve/serve-stop/serve-status.
//!
//! `kill(2)` is how a PID file is checked for liveness (`kill(pid, 0)`) and
//! how a stale daemon is asked to stop. The workspace denies `unsafe_code`;
//! both call sites are the same libc call and carry their own SAFETY note.
//!
//! The file a running daemon writes is not just a PID: a client that wants to
//! connect has to know *where* to connect, and the port is chosen at bind
//! time. Writing it here is what makes `codape serve` reachable by an editor
//! that was never told the port; see [`write_daemon_state`].
#![allow(unsafe_code)]

use std::path::PathBuf;

/// Where a running daemon can be reached, as recorded on disk.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DaemonState {
    /// The daemon's process id, for liveness and `serve-stop`.
    pub pid: u32,
    /// The bound address, as `host:port` — what a client connects to.
    pub address: String,
    /// The token a local client authenticates with, when the daemon minted one.
    ///
    /// Only set for a daemon started on loopback with no key list of its own:
    /// that daemon has to authenticate somebody, and minting a secret it
    /// records in this file (mode 0600, in the user's config directory) is
    /// what lets a local client connect without the user inventing one. A
    /// daemon given an explicit [`codape_server::SERVICE_TOKENS_ENV`] list
    /// records nothing here — that list is the user's secret, not this file's.
    ///
    /// `#[serde(default)]` because the field arrived after the file did: an
    /// older daemon wrote `{pid, address}`, and refusing to parse it would
    /// turn a running daemon into "no daemon".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl DaemonState {
    /// The `ws://` URL a client connects to.
    #[must_use]
    pub fn websocket_url(&self) -> String {
        format!("ws://{}", self.address)
    }
}

/// Path to the daemon state file.
#[must_use]
pub fn pid_file_path() -> PathBuf {
    let mut path = codape_config::config_home();
    path.push("daemon.pid");
    path
}

/// Record the running daemon's PID, bound address, and local token.
///
/// `token` is the secret a local client authenticates with, or `None` when the
/// daemon was given an explicit key list. The file is created owner-readable
/// only: it can now hold a credential.
pub fn write_daemon_state(
    address: &std::net::SocketAddr,
    token: Option<&str>,
) -> std::io::Result<()> {
    write_daemon_state_at(&pid_file_path(), address, token)
}

/// Record just the running daemon's PID, before it has bound anything.
///
/// A daemon that fails during startup still has to be stoppable, and
/// `serve-stop` looks for a PID. The address is filled in once the listener
/// exists, by [`write_daemon_state`].
pub fn write_pid_file() -> std::io::Result<()> {
    write_pid_file_at(&pid_file_path())
}

/// Remove the daemon state file.
pub fn remove_pid_file() -> std::io::Result<()> {
    remove_pid_file_at(&pid_file_path())
}

/// Read the PID from the daemon state file, if it exists.
#[must_use]
pub fn read_pid() -> Option<u32> {
    read_pid_at(&pid_file_path())
}

/// Read what a running daemon recorded, if it recorded anything.
///
/// A file written by an older build holds a bare PID and nothing else. It is
/// reported as a state with no address rather than as an error: the daemon is
/// running, this build just cannot tell where, and `serve-status` should say
/// that rather than claim there is no daemon.
#[must_use]
pub fn read_daemon_state() -> Option<DaemonState> {
    read_daemon_state_at(&pid_file_path())
}

/// Write the current process PID, bound address and token to `path`.
///
/// Split out so the tests can use a directory of their own: they used to share
/// the developer's real `~/.codape/daemon.pid`, and running in parallel meant
/// one test deleting the file another was reading.
fn write_daemon_state_at(
    path: &std::path::Path,
    address: &std::net::SocketAddr,
    token: Option<&str>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let state = DaemonState {
        pid: std::process::id(),
        address: address.to_string(),
        token: token.map(str::to_string),
    };
    let json = serde_json::to_string(&state)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    write_private(path, json.as_bytes())
}

/// Write just a PID to `path`, the shape a daemon writes before it binds.
fn write_pid_file_at(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, std::process::id().to_string())
}

/// Write a file only the owner can read.
///
/// The state file can carry a credential, so it is created 0600 rather than
/// whatever the umask allows. `std::fs::write` cannot do that: it opens with
/// the default mode and a reader could win the race between create and
/// `set_permissions`, which is why the mode is set at creation instead.
fn write_private(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // An existing file keeps its old mode through `open`, so a file left
        // by an earlier build is tightened here rather than trusted.
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        file.write_all(contents)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

fn read_daemon_state_at(path: &std::path::Path) -> Option<DaemonState> {
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if let Ok(state) = serde_json::from_str::<DaemonState>(trimmed) {
        return Some(state);
    }
    // A bare PID: written before the address was recorded.
    trimmed.parse::<u32>().ok().map(|pid| DaemonState {
        pid,
        address: String::new(),
        token: None,
    })
}

fn remove_pid_file_at(path: &std::path::Path) -> std::io::Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn read_pid_at(path: &std::path::Path) -> Option<u32> {
    read_daemon_state_at(path).map(|state| state.pid)
}

/// Check if a process with the given PID is running.
// PIDs fit in i32 on all supported platforms; the cast cannot wrap in practice.
#[cfg(unix)]
#[allow(clippy::cast_possible_wrap)]
pub fn is_process_running(pid: u32) -> bool {
    // SAFETY: kill with sig=0 checks if process exists, doesn't send a signal
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
pub const fn is_process_running(_pid: u32) -> bool {
    // On Windows, no reliable way without WinAPI; assume running if file exists
    true
}

/// Stop the daemon by sending SIGTERM to the running process.
// PIDs fit in i32 on all supported platforms; the cast cannot wrap in practice.
#[allow(clippy::cast_possible_wrap)]
pub fn stop_daemon() -> Result<String, String> {
    let pid = read_pid().ok_or_else(|| "No PID file found. Is the server running?".to_string())?;

    if !is_process_running(pid) {
        let _ = remove_pid_file();
        return Err(format!(
            "Process {pid} is not running. Cleaned up stale PID file."
        ));
    }

    #[cfg(unix)]
    {
        // SAFETY: sending SIGTERM to a known PID
        let result = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if result != 0 {
            return Err(format!("Failed to send SIGTERM to process {pid}"));
        }
    }

    #[cfg(not(unix))]
    {
        Err("Stopping daemon is only supported on Unix systems".to_string())
    }

    #[cfg(unix)]
    {
        // Brief wait for graceful shutdown
        std::thread::sleep(std::time::Duration::from_millis(500));

        if is_process_running(pid) {
            Ok(format!(
                "Sent SIGTERM to process {pid}. Server is shutting down..."
            ))
        } else {
            let _ = remove_pid_file();
            Ok(format!("Process {pid} has stopped. PID file cleaned."))
        }
    }
}

/// Get the status of the running daemon.
#[must_use]
pub fn serve_status() -> String {
    let Some(state) = read_daemon_state() else {
        return "No running daemon (no PID file found).".to_string();
    };
    let pid = state.pid;

    if is_process_running(pid) {
        // The address is what a client actually needs, so it is reported when
        // the daemon recorded one. An old file holds only a PID, and saying so
        // is better than printing a URL that is a guess.
        if state.address.is_empty() {
            format!("Daemon is running (PID: {pid}); its file records no address.")
        } else if state.token.is_some() {
            format!(
                "Daemon is running (PID: {pid}) at {} — an editor can reach it with `codape ide`.",
                state.websocket_url()
            )
        } else {
            format!(
                "Daemon is running (PID: {pid}) at {} — it requires the keys in {}.",
                state.websocket_url(),
                codape_server::SERVICE_TOKENS_ENV
            )
        }
    } else {
        let _ = remove_pid_file();
        format!("Stale PID file found (PID: {pid}), but process is not running. Cleaned up.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_file_path_is_correct() {
        let path = pid_file_path();
        assert!(path.ends_with("daemon.pid"));
    }

    #[test]
    fn write_and_read_pid_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        write_pid_file_at(&path).unwrap();
        let pid = read_pid_at(&path);
        assert!(pid.is_some());
        assert_eq!(pid.unwrap(), std::process::id());
        remove_pid_file_at(&path).unwrap();
    }

    /// The address is the whole point of the file: a client that is never told
    /// the port — which is chosen at bind time when `--port 0` — cannot
    /// connect, and `serve-status` used to print a PID and stop.
    #[test]
    fn the_address_and_token_survive_a_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        let address: std::net::SocketAddr = "127.0.0.1:9123".parse().unwrap();
        write_daemon_state_at(&path, &address, Some("s3cret")).unwrap();

        let state = read_daemon_state_at(&path).expect("the state parses");
        assert_eq!(state.pid, std::process::id());
        assert_eq!(state.address, "127.0.0.1:9123");
        assert_eq!(state.websocket_url(), "ws://127.0.0.1:9123");
        assert_eq!(state.token.as_deref(), Some("s3cret"));
    }

    /// A daemon handed an explicit key list records no token: that list is the
    /// user's secret, not this file's to hold.
    #[test]
    fn a_daemon_with_its_own_keys_records_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        let address: std::net::SocketAddr = "127.0.0.1:9124".parse().unwrap();
        write_daemon_state_at(&path, &address, None).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("token"), "{raw}");
        assert!(read_daemon_state_at(&path).unwrap().token.is_none());
    }

    /// The file can hold a credential, so it must not be world-readable.
    #[cfg(unix)]
    #[test]
    fn the_state_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        let address: std::net::SocketAddr = "127.0.0.1:9125".parse().unwrap();
        write_daemon_state_at(&path, &address, Some("t")).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    /// A file written before the address existed still names a live daemon.
    #[test]
    fn a_bare_pid_file_still_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        std::fs::write(&path, "4242").unwrap();

        let state = read_daemon_state_at(&path).expect("a bare PID parses");
        assert_eq!(state.pid, 4242);
        assert!(state.address.is_empty());
        assert!(state.token.is_none());
    }

    #[test]
    fn read_pid_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_pid_at(&dir.path().join("daemon.pid")).is_none());
    }

    #[test]
    fn remove_pid_file_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.pid");
        remove_pid_file_at(&path).unwrap();
        remove_pid_file_at(&path).unwrap(); // No error on second call
    }

    #[test]
    fn current_process_is_running() {
        assert!(is_process_running(std::process::id()));
    }

    #[test]
    fn stop_daemon_no_pid_file() {
        let _ = remove_pid_file();
        let result = stop_daemon();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No PID file"));
    }
}
