//! `/keybindings` through the real binary.
//!
//! The keymap had no way in: every shortcut was a literal key comparison in
//! the REPL, and a `keybindings.json` would have been read by nothing. These
//! run the command against files the session actually loads, so the table it
//! prints is the table the REPL resolves keys through.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/cli has a parent")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn start_static_provider() -> (Child, String) {
    let responses = workspace_root().join("tests/responses.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_static-anthropic-server"))
        .args([
            "--port",
            "0",
            "--responses",
            responses.to_str().expect("responses path"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn static provider");

    let stderr = child.stderr.take().expect("server stderr");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut announced = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !announced && let Some(address) = line.split("listening on ").nth(1) {
                let _ = tx.send(address.trim().to_string());
                announced = true;
            }
        }
    });
    let address = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("static provider did not announce its address");
    (child, format!("http://{address}"))
}

fn seed_home(settings: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(home.path().join("settings.json"), settings).expect("settings");
    home
}

fn run(cwd: &Path, home: &Path, base_url: &str, command: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codape"))
        .args(["-p", command])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("CODAPE_LOG", "off")
        .env("CODAPE_TRUST_WORKSPACE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codape");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"")
        .expect("close stdin");
    child.wait_with_output().expect("wait")
}

/// With no file at all the command still answers, and says so: the defaults
/// are what the session uses, and a table with no provenance would read as if
/// the person's file had been loaded and contained nothing.
#[test]
fn the_keybindings_command_reports_the_defaults_and_no_files() {
    let dir = tempfile::tempdir().expect("workspace");
    let home = seed_home(r#"{"config_version": 4, "telemetry_enabled": false}"#);
    let (mut provider, base_url) = start_static_provider();

    let output = run(dir.path(), home.path(), &base_url, "/keybindings");

    let _ = provider.kill();
    let _ = provider.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("built-in defaults only"), "{stdout}");
    assert!(stdout.contains("app:toggleToolOutput"), "{stdout}");
    assert!(stdout.contains("ctrl+o"), "{stdout}");
    assert!(stdout.contains("chat:cancel"), "{stdout}");
    assert!(stdout.contains("esc"), "{stdout}");
}

/// A binding in the user's file is what the table shows — the command reads
/// the file rather than printing a copy of the defaults.
#[test]
fn a_bound_key_appears_in_the_table() {
    let dir = tempfile::tempdir().expect("workspace");
    let home = seed_home(r#"{"config_version": 4, "telemetry_enabled": false}"#);
    std::fs::write(
        home.path().join("keybindings.json"),
        r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+j":"app:toggleToolOutput"}}]}"#,
    )
    .expect("keybindings file");
    let (mut provider, base_url) = start_static_provider();

    let output = run(dir.path(), home.path(), &base_url, "/keybindings");

    let _ = provider.kill();
    let _ = provider.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("keybindings.json"), "{stdout}");
    // Both keystrokes are listed for the action: bindings are additive.
    assert!(stdout.contains("ctrl+o"), "{stdout}");
    assert!(stdout.contains("ctrl+j"), "{stdout}");
}

/// A malformed file does not take the session down with it: the startup still
/// completes, the defaults still work, and `/keybindings` says what was wrong
/// with the file. A config error that only appears in a log file is one the
/// person never sees.
#[test]
fn a_broken_file_is_reported_and_the_session_still_starts() {
    let dir = tempfile::tempdir().expect("workspace");
    let home = seed_home(r#"{"config_version": 4, "telemetry_enabled": false}"#);
    std::fs::write(
        home.path().join("keybindings.json"),
        r#"{"bindings":[{"context":"Global","bindings":{"ctl+j":"app:toggleToolOutput"}}]}"#,
    )
    .expect("keybindings file");
    let (mut provider, base_url) = start_static_provider();

    let output = run(dir.path(), home.path(), &base_url, "/keybindings");

    let _ = provider.kill();
    let _ = provider.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("problem(s) in the files"), "{stdout}");
    assert!(stdout.contains("not a modifier"), "{stdout}");
    // The rejected binding was not applied, and the default survived it.
    assert!(stdout.contains("ctrl+o"), "{stdout}");
}

/// A chord written in `keybindings.json` is bound, shown and reported as one.
///
/// The parser used to refuse any binding with a space in it outright, so
/// `ctrl+k ctrl+s` could not be expressed at all. This is the file-level
/// version of that: the binding reaches the live keymap, and the table prints
/// the chord as the two keystrokes a person types.
#[test]
fn a_chord_from_the_file_is_bound_and_shown() {
    let dir = tempfile::tempdir().expect("workspace");
    let home = seed_home(r#"{"config_version": 4, "telemetry_enabled": false}"#);
    std::fs::write(
        home.path().join("keybindings.json"),
        r#"{"bindings":[{"context":"Global","bindings":{"ctrl+k ctrl+s":"app:toggleToolOutput"}}]}"#,
    )
    .expect("keybindings file");
    let (mut provider, base_url) = start_static_provider();

    let output = run(dir.path(), home.path(), &base_url, "/keybindings");

    let _ = provider.kill();
    let _ = provider.wait();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        !stdout.contains("problem(s) in the files"),
        "a valid chord was reported as a problem: {stdout}"
    );
    assert!(stdout.contains("ctrl+k ctrl+s"), "{stdout}");
    // Additive, like every other binding: the default stays.
    assert!(stdout.contains("ctrl+o"), "{stdout}");
}
