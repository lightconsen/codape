//! A skill on disk reaches the model.
//!
//! The startup phase used to `read_dir` `.codape/skills/` and log the entry
//! count. Nothing was parsed, so nothing was loaded, and nothing was said about
//! a skill in the prompt — the `Skill` tool was a tool the model had to guess a
//! name for, and `/skills` listed a different directory than the loader read.
//!
//! This drives the real binary against the static provider and reads back the
//! request it was sent: if the advert is missing, the model was never told the
//! skill exists.

use std::io::{BufRead, BufReader};
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

fn start_static_provider(record: &Path) -> (Child, String) {
    let responses = workspace_root().join("tests/responses.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_static-anthropic-server"))
        .args([
            "--port",
            "0",
            "--responses",
            responses.to_str().expect("responses path"),
            "--record",
            record.to_str().expect("record path"),
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

fn write_skill(workspace: &Path, name: &str, description: &str) {
    let dir = workspace.join(".codape").join("skills").join(name);
    std::fs::create_dir_all(&dir).expect("skill dir");
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nDo the thing.\n"),
    )
    .expect("skill file");
}

fn run_codape(workspace: &Path, home: &Path, base_url: &str, prompt: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codape"))
        .args([
            "-p",
            prompt,
            "--dangerously-skip-permissions",
            "--no-session-persistence",
        ])
        .current_dir(workspace)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("CODAPE_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codape");
    child.stdin.take();
    child.wait_with_output().expect("wait")
}

/// A project skill directory is announced in the request the model receives.
#[test]
fn a_directory_skill_is_announced_to_the_model() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    write_skill(
        workspace.path(),
        "pdf",
        "Extract text and tables from a PDF file",
    );

    let recorded = workspace.path().join("requests.log");
    let (mut provider, base_url) = start_static_provider(&recorded);

    let output = run_codape(workspace.path(), home.path(), &base_url, "hello");
    let _ = provider.kill();
    let _ = provider.wait();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");

    let requests = std::fs::read_to_string(&recorded).unwrap_or_default();
    assert!(
        requests.contains("pdf"),
        "the model was not told the skill exists:\n{requests}"
    );
    assert!(
        requests.contains("Extract text and tables from a PDF file"),
        "the skill's description did not reach the prompt:\n{requests}"
    );
}

/// `/skills` lists what the loader loaded, in the same directory.
#[test]
fn the_skills_command_lists_a_project_skill() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    write_skill(workspace.path(), "spreadsheet", "Read an .xlsx file");

    let recorded = workspace.path().join("requests.log");
    let (mut provider, base_url) = start_static_provider(&recorded);

    let output = run_codape(workspace.path(), home.path(), &base_url, "/skills");
    let _ = provider.kill();
    let _ = provider.wait();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("spreadsheet"),
        "the project skill is missing from /skills:\n{stdout}"
    );
}
