//! Custom Markdown slash commands are real commands.
//!
//! `.codape/commands/*.md` used to be scanned and logged and then dropped: the
//! startup phase parsed `.json` files it never registered, and a `.md` file was
//! not even looked at. So a project could ship a `/review` that `/help` did not
//! list, `/review` did not run, and nothing said so.
//!
//! These drive the real binary. The first asserts the file reaches `/help`; the
//! second asserts the body — with `$ARGUMENTS` substituted — is what the model
//! actually receives, by reading back the request the provider was sent.

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

/// Start the static provider, optionally recording every request body it gets.
fn start_static_provider(record: Option<&Path>) -> (Child, String) {
    let responses = workspace_root().join("tests/responses.json");
    let mut command = Command::new(env!("CARGO_BIN_EXE_static-anthropic-server"));
    command
        .args([
            "--port",
            "0",
            "--responses",
            responses.to_str().expect("responses path"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(record) = record {
        command.args(["--record", record.to_str().expect("record path")]);
    }
    let mut child = command.spawn().expect("spawn static provider");

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

fn settings(home: &Path) {
    std::fs::write(
        home.join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
}

/// Write a command file into the workspace's project command directory.
fn write_command(workspace: &Path, name: &str, body: &str) {
    let dir = workspace.join(".codape").join("commands");
    std::fs::create_dir_all(&dir).expect("command dir");
    std::fs::write(dir.join(format!("{name}.md")), body).expect("command file");
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

/// A Markdown file in `.codape/commands/` is a command `/help` knows about.
#[test]
fn a_markdown_command_appears_in_help() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("config home");
    settings(home.path());
    write_command(
        workspace.path(),
        "ship-check",
        "---\ndescription: Check the release checklist\n---\nCheck the release checklist.\n",
    );
    let (mut provider, base_url) = start_static_provider(None);

    let output = run_codape(workspace.path(), home.path(), &base_url, "/help");
    let _ = provider.kill();
    let _ = provider.wait();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("ship-check"),
        "the file-provided command is not listed by /help:\n{stdout}"
    );
    assert!(
        stdout.contains("Check the release checklist"),
        "the description from the frontmatter did not reach /help:\n{stdout}"
    );
}

/// Running a Markdown command asks the model once, with the expanded body.
#[test]
fn a_markdown_command_is_sent_to_the_model_with_its_arguments_expanded() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("config home");
    settings(home.path());
    write_command(
        workspace.path(),
        "summarize",
        "---\ndescription: Summarize a path\n---\nSummarize the module at $1 in one paragraph.\n",
    );
    let recorded = workspace.path().join("requests.log");
    let (mut provider, base_url) = start_static_provider(Some(&recorded));

    let output = run_codape(
        workspace.path(),
        home.path(),
        &base_url,
        "/summarize src/parser.rs",
    );
    let _ = provider.kill();
    let _ = provider.wait();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        !stdout.contains("Unknown command"),
        "the command was not registered:\n{stdout}"
    );

    // The provider saw the expansion, not the file's raw `$1`.
    let requests = std::fs::read_to_string(&recorded).unwrap_or_default();
    assert!(
        requests.contains("Summarize the module at src/parser.rs in one paragraph."),
        "the model was not sent the expanded body. requests:\n{requests}"
    );
    assert!(
        !requests.contains("$1"),
        "an unsubstituted placeholder reached the model:\n{requests}"
    );
    // And it produced an answer rather than only a banner.
    assert!(
        !stdout.trim().is_empty(),
        "the turn produced no output:\n{stdout}"
    );
}

/// A project command file cannot replace a built-in.
#[test]
fn a_command_file_does_not_shadow_a_built_in() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("config home");
    settings(home.path());
    write_command(
        workspace.path(),
        "status",
        "---\ndescription: Not the real status\n---\nSay something else.\n",
    );
    let (mut provider, base_url) = start_static_provider(None);

    let output = run_codape(workspace.path(), home.path(), &base_url, "/help");
    let _ = provider.kill();
    let _ = provider.wait();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        !stdout.contains("Not the real status"),
        "the file replaced the built-in /status:\n{stdout}"
    );
}
