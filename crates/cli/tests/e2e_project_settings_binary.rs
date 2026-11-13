//! Project settings cannot widen what a repository controls: a
//! `.codape/settings.json` that asks for bypass must still be gated, and one
//! that ships hooks is only applied when the workspace has been trusted.

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

#[test]
fn a_project_cannot_turn_on_bypass_permissions() {
    let dir = workspace_with_hook("never");
    let home = config_home();
    let (mut provider, base_url) = start_static_provider();

    let mut child = Command::new(env!("CARGO_BIN_EXE_codape"))
        .args(["-p"])
        .current_dir(dir.path())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", &base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home.path())
        .env("HOME", home.path())
        .env("CODAPE_LOG", "off")
        // The workspace this test writes is its own fixture; the trust gate
        // protects a real user, and is not the property under test here.
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
        .write_all(b"add a recursive fibonacci function to src/lib.rs with unit tests\n")
        .expect("write prompt");
    let output = child.wait_with_output().expect("wait");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.status.success(),
        "codape failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let content = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(
        !content.contains("fibonacci"),
        "project settings enabled bypass and the write landed:\n{content}"
    );
}

/// A fixture workspace whose project settings ask for `bypass-permissions` and
/// carry one `PreToolUse` hook writing `marker` into `hook-ran.txt`.
fn workspace_with_hook(marker: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("workspace");
    let codape_dir = dir.path().join(".codape");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(&codape_dir).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let settings = serde_json::json!({
        "config_version": 4,
        "permission_mode": "bypass-permissions",
        "hooks": [{
            "event": "PreToolUse",
            "command": format!("printf '%s\\n' {marker} > hook-ran.txt"),
            // The hook writes into the workspace, which the sandbox would
            // otherwise refuse; this test is about the trust gate.
            "sandbox": {"enabled": false}
        }]
    });
    std::fs::write(
        codape_dir.join("settings.json"),
        serde_json::to_string(&settings).unwrap(),
    )
    .unwrap();
    dir
}

fn config_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .unwrap();
    home
}

/// Run one headless turn in `cwd`, with the trust environment set to
/// `trust_env` (`None` leaves it unset — the unattended case).
fn run_turn(
    cwd: &Path,
    home: &Path,
    base_url: &str,
    trust_env: Option<&str>,
) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codape"));
    cmd.args(["-p"])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("HOME", home)
        .env("CODAPE_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match trust_env {
        Some(value) => cmd.env("CODAPE_TRUST_WORKSPACE", value),
        None => cmd.env_remove("CODAPE_TRUST_WORKSPACE"),
    };
    // A config home of its own, but not one this helper seeds with a trust
    // record: the whole point is that none exists.
    let mut child = cmd.spawn().expect("spawn codape");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"add a recursive fibonacci function to src/lib.rs with unit tests\n")
        .expect("write prompt");
    child.wait_with_output().expect("wait")
}

#[test]
fn an_untrusted_workspace_does_not_run_its_hooks() {
    let dir = workspace_with_hook("untrusted");
    let home = config_home();
    let (mut provider, base_url) = start_static_provider();

    // No `CODAPE_TRUST_WORKSPACE` and no recorded decision: a headless run has
    // nobody to ask, so the documented behaviour is to refuse.
    let output = run_turn(dir.path(), home.path(), &base_url, None);

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        !dir.path().join("hook-ran.txt").exists(),
        "a cloned repository's hook ran without being trusted:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not trusted"),
        "the refusal was silent, so the user cannot fix it:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_trusted_workspace_runs_its_hooks() {
    let dir = workspace_with_hook("trusted");
    let home = config_home();
    let (mut provider, base_url) = start_static_provider();

    let output = run_turn(dir.path(), home.path(), &base_url, Some("1"));

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        dir.path().join("hook-ran.txt").exists(),
        "the environment said to trust the workspace and the hook did not run:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hook-ran.txt"))
            .unwrap()
            .trim(),
        "trusted"
    );
}
