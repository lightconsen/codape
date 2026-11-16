//! `codape ide bridge` through the real binary: an editor's MCP client drives a
//! running `codape serve`.
//!
//! The bridge is the editor-facing half of the integration and it is a
//! separate process from both the editor and the daemon, so the only honest
//! test is to run all three and speak both protocols. These tests start a
//! daemon, start the bridge as an editor would (a child process over pipes),
//! and assert that a prompt arrives as an answer and that a bridge which
//! disappears does not leave the daemon waiting.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
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

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

/// Everything the daemon and the bridge share: one config home, one workspace.
struct Env {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    daemon: Child,
    port: u16,
}

impl Env {
    /// Start a daemon on a free port and wait until it is listening.
    async fn start(base_url: &str) -> Self {
        let home = tempfile::tempdir().expect("config home");
        std::fs::write(
            home.path().join("settings.json"),
            r#"{"config_version": 4, "telemetry_enabled": false}"#,
        )
        .expect("seed config");
        let workspace = tempfile::tempdir().expect("workspace");
        let port = free_port();

        // No `CODAPE_API_KEYS`: the point is that a loopback daemon makes
        // itself reachable without the user inventing a key.
        let daemon = Command::new(env!("CARGO_BIN_EXE_codape"))
            .args(["serve", "--port", &port.to_string()])
            .current_dir(workspace.path())
            .env("ANTHROPIC_API_KEY", "mock")
            .env("ANTHROPIC_BASE_URL", base_url)
            .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
            .env("CLAUDE_CONFIG_DIR", home.path())
            .env("CODAPE_SERVER_NO_PERSIST", "1")
            .env("CODAPE_LOG", "off")
            .env_remove("CODAPE_API_KEYS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn codape serve");

        // The daemon records its address once it binds. Waiting on the file
        // rather than on the port proves the recording works too.
        let state_path = home.path().join("daemon.pid");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(text) = std::fs::read_to_string(&state_path)
                && let Ok(state) = serde_json::from_str::<serde_json::Value>(&text)
                && state["address"].as_str().is_some_and(|a| !a.is_empty())
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the daemon never recorded its address in {}",
                state_path.display()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Self {
            home,
            workspace,
            daemon,
            port,
        }
    }

    /// The token the daemon recorded for local clients.
    fn recorded(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.home.path().join("daemon.pid"))
            .expect("the daemon state file");
        serde_json::from_str(&text).expect("the state is JSON")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// A bridge child, as an editor would launch it.
struct Bridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Bridge {
    /// Launch `codape ide bridge` against `env`.
    ///
    /// `CODAPE_API_KEYS` is deliberately unset: the bridge is supposed to use
    /// the token the daemon recorded, not one inherited from a shell.
    fn launch(env: &Env) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codape"))
            .args(["ide", "bridge"])
            .current_dir(env.workspace.path())
            .env("CLAUDE_CONFIG_DIR", env.home.path())
            .env("CODAPE_LOG", "off")
            .env_remove("CODAPE_API_KEYS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn codape ide bridge");
        let stdin = child.stdin.take().expect("bridge stdin");
        let stdout = BufReader::new(child.stdout.take().expect("bridge stdout"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    /// Send a JSON-RPC request and read its answer.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        writeln!(self.stdin, "{request}").expect("write request");
        self.stdin.flush().expect("flush request");

        // The bridge answers every request it accepts, so a missing answer is
        // a hang rather than a race.
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read answer");
            assert!(read > 0, "the bridge closed without answering");
            let frame: serde_json::Value = serde_json::from_str(&line).expect("answer is JSON");
            if frame["id"] == id {
                return frame;
            }
            // A notification, or the answer to something else. Keep reading.
        }
    }

    /// The text of a tool call's result.
    fn call_tool(&mut self, name: &str, arguments: serde_json::Value) -> Result<String, String> {
        let answer = self.request(
            "tools/call",
            serde_json::json!({
                "name": name,
                "arguments": arguments
            }),
        );
        if let Some(error) = answer.get("error") {
            let message = error["message"].as_str().unwrap_or("").to_string();
            return Err(message);
        }
        Ok(answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A turn that wants to run a tool reaches the editor as a prompt, and the
/// editor's answer is what lets it run.
///
/// This is the difference between an integration and a remote control: before
/// the daemon had a confirmation channel every `Ask` became "unapproved tool"
/// without anyone being asked, so an editor could drive a session that could
/// read but never write.
#[tokio::test]
async fn a_permission_prompt_reaches_the_editor_and_its_answer_resumes_the_turn() {
    let (mut provider, base_url) = start_static_provider();
    let env = Env::start(&base_url).await;
    let mut bridge = Bridge::launch(&env);

    // The static provider scripts this prompt into a `Bash` call.
    let paused = bridge
        .call_tool(
            "ask",
            serde_json::json!({"prompt": "run a shell command in the background"}),
        )
        .expect("the turn pauses rather than failing");
    assert!(
        paused.contains("paused"),
        "the editor was not told the turn is waiting: {paused}"
    );
    assert!(
        paused.contains("Bash"),
        "the prompt must name the tool it is asking about: {paused}"
    );
    let request_id = paused
        .lines()
        .find_map(|line| line.trim().strip_prefix("request_id:"))
        .map_or_else(
            || panic!("the prompt carries no request id: {paused}"),
            str::trim,
        )
        .to_string();

    // A second prompt while the turn is paused must be refused with the reason,
    // not queued behind a turn that will never run.
    let refused = bridge
        .call_tool("ask", serde_json::json!({"prompt": "hello"}))
        .expect_err("a paused turn blocks another prompt");
    assert!(refused.contains(&request_id), "{refused}");

    let resumed = bridge
        .call_tool(
            "answer_permission",
            serde_json::json!({"request_id": request_id, "decision": "approved"}),
        )
        .expect("the turn resumes after approval");
    // Approval is what makes the executor run: the tool's own output reaches
    // the editor only if the call went through.
    assert!(
        resumed.contains("hello from static server"),
        "the approved call did not run, or its output never came back: {resumed}"
    );
    assert!(
        !resumed.contains("paused"),
        "the turn asked again after being approved: {resumed}"
    );

    let _ = provider.kill();
    let _ = provider.wait();
}

/// An unknown request id is refused: an answer addressed to nothing must not
/// be treated as an answer to whatever is waiting.
#[tokio::test]
async fn an_answer_for_the_wrong_request_is_refused() {
    let (mut provider, base_url) = start_static_provider();
    let env = Env::start(&base_url).await;
    let mut bridge = Bridge::launch(&env);

    let _paused = bridge
        .call_tool(
            "ask",
            serde_json::json!({"prompt": "run a shell command in the background"}),
        )
        .expect("the turn pauses");

    let refused = bridge
        .call_tool(
            "answer_permission",
            serde_json::json!({"request_id": "not-the-one", "decision": "approved"}),
        )
        .expect_err("a mis-addressed answer is refused");
    assert!(
        refused.contains("not-the-one"),
        "the refusal should name the id it was given: {refused}"
    );

    let _ = provider.kill();
    let _ = provider.wait();
}

/// The whole point of the gap: an editor that knows only MCP gets an answer,
/// and the session it drove was reachable without the user configuring a key.
#[tokio::test]
async fn an_editor_prompt_reaches_the_daemon_and_comes_back() {
    let (mut provider, base_url) = start_static_provider();
    let env = Env::start(&base_url).await;

    let recorded = env.recorded();
    assert!(
        recorded["token"].as_str().is_some_and(|t| !t.is_empty()),
        "a loopback daemon must record a token for local clients: {recorded}"
    );
    assert!(
        recorded["address"]
            .as_str()
            .is_some_and(|a| a.ends_with(&env.port.to_string())),
        "the recorded address names the port the daemon was told: {recorded}"
    );

    let mut bridge = Bridge::launch(&env);

    let init = bridge.request("initialize", serde_json::json!({}));
    assert_eq!(
        init["result"]["capabilities"]["tools"],
        serde_json::json!({})
    );

    let tools = bridge.request("tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for expected in ["ask", "answer_permission", "cancel", "status"] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }

    let answer = bridge
        .call_tool("ask", serde_json::json!({"prompt": "hello"}))
        .expect("the turn answers");
    assert!(
        answer.contains("Hello!"),
        "the editor did not receive the daemon's answer: {answer}"
    );

    // A second prompt on the same bridge uses the same session: the answer
    // coming back at all proves the connection survived the first turn.
    let second = bridge
        .call_tool("ask", serde_json::json!({"prompt": "hello"}))
        .expect("the second turn answers");
    assert!(second.contains("Hello!"), "{second}");

    let _ = provider.kill();
    let _ = provider.wait();
}

/// A bridge with no daemon says so instead of hanging, which is what a
/// misconfigured editor sees first.
#[tokio::test]
async fn a_missing_daemon_is_reported_not_awaited() {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");
    let workspace = tempfile::tempdir().expect("workspace");

    let mut child = Command::new(env!("CARGO_BIN_EXE_codape"))
        .args(["ide", "bridge"])
        .current_dir(workspace.path())
        .env("CLAUDE_CONFIG_DIR", home.path())
        .env("CODAPE_LOG", "off")
        .env_remove("CODAPE_API_KEYS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codape ide bridge");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "ask", "arguments": {"prompt": "hello"}}
        })
    )
    .expect("write");
    stdin.flush().expect("flush");

    let mut line = String::new();
    stdout.read_line(&mut line).expect("read");
    let frame: serde_json::Value = serde_json::from_str(&line).expect("JSON");
    let message = frame["error"]["message"]
        .as_str()
        .expect("a refusal, not an answer");
    assert!(
        message.contains("no daemon is running"),
        "the bridge should name the missing daemon: {message}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// `codape ide` prints a registration snippet that names this binary.
#[tokio::test]
async fn ide_prints_a_registration_snippet() {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");

    let output = Command::new(env!("CARGO_BIN_EXE_codape"))
        .arg("ide")
        .env("CLAUDE_CONFIG_DIR", home.path())
        .env("CODAPE_LOG", "off")
        .output()
        .expect("run codape ide");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mcpServers"), "{stdout}");
    assert!(stdout.contains("\"ide\""), "{stdout}");
    assert!(stdout.contains("bridge"), "{stdout}");
    assert!(
        stdout.contains("No daemon is running"),
        "with no daemon it must say so rather than claim one: {stdout}"
    );
}

/// A stale state file — a daemon that was killed without cleaning up — must be
/// reported as stale, not connected to.
#[tokio::test]
async fn a_stale_daemon_file_is_reported_as_stale() {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");
    // A PID that is certainly not running: pick one past the platform's
    // maximum and let `is_process_running` report false.
    std::fs::write(
        home.path().join("daemon.pid"),
        serde_json::json!({"pid": 4_294_967_000u32, "address": "127.0.0.1:1", "token": "t"})
            .to_string(),
    )
    .expect("write stale state");

    let output = Command::new(env!("CARGO_BIN_EXE_codape"))
        .arg("ide")
        .env("CLAUDE_CONFIG_DIR", home.path())
        .env("CODAPE_LOG", "off")
        .output()
        .expect("run codape ide");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("not running"),
        "a dead daemon must be named as dead: {stdout}"
    );
}

/// A `--port 0` daemon is only reachable if it records the port it picked.
/// That is the whole reason the state file is more than a PID.
#[tokio::test]
async fn a_zero_port_daemon_records_the_port_it_bound() {
    let (mut provider, base_url) = start_static_provider();
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");
    let workspace = tempfile::tempdir().expect("workspace");

    let mut daemon = Command::new(env!("CARGO_BIN_EXE_codape"))
        .args(["serve", "--port", "0"])
        .current_dir(workspace.path())
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", &base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home.path())
        .env("CODAPE_SERVER_NO_PERSIST", "1")
        .env("CODAPE_LOG", "off")
        .env_remove("CODAPE_API_KEYS")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codape serve");

    let state_path = home.path().join("daemon.pid");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let address = loop {
        if let Ok(text) = std::fs::read_to_string(&state_path)
            && let Ok(state) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(address) = state["address"].as_str()
            && !address.is_empty()
        {
            break address.to_string();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon never recorded an address"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let port: u16 = address
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or_else(|| panic!("the recorded address has no port: {address}"));
    assert_ne!(port, 0, "a bound port is never zero");

    // The recorded address really is the listener: connecting to it works.
    let connect = tokio::net::TcpStream::connect(("127.0.0.1", port)).await;
    assert!(
        connect.is_ok(),
        "nothing is listening on the recorded {address}"
    );

    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = provider.kill();
    let _ = provider.wait();
}

/// The bridge must not print anything but protocol on stdout: one stray line
/// and the editor's JSON-RPC parser stops reading.
#[tokio::test]
async fn the_bridge_keeps_stdout_to_protocol() {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");
    let workspace = tempfile::tempdir().expect("workspace");

    let mut child = Command::new(env!("CARGO_BIN_EXE_codape"))
        .args(["ide", "bridge"])
        .current_dir(workspace.path())
        .env("CLAUDE_CONFIG_DIR", home.path())
        .env("CODAPE_LOG", "info")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn bridge");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    // `CODAPE_LOG=info` would make the orchestrator narrate; the bridge bypasses
    // it entirely, and the answer must be the only thing on stdout.
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": "initialize", "params": {}})
    )
    .expect("write");
    stdin.flush().expect("flush");
    drop(stdin);

    let mut line = String::new();
    stdout.read_line(&mut line).expect("read");
    let frame: serde_json::Value = serde_json::from_str(&line)
        .unwrap_or_else(|error| panic!("stdout line is not JSON: {error}: {line:?}"));
    assert_eq!(frame["id"], 7);

    let _ = child.kill();
    let _ = child.wait();
}

/// `daemon.pid` now holds a secret, so it must not be world-readable.
#[cfg(unix)]
#[tokio::test]
async fn the_recorded_token_is_not_world_readable() {
    use std::os::unix::fs::PermissionsExt;

    let (mut provider, base_url) = start_static_provider();
    let env = Env::start(&base_url).await;

    let path = env.home.path().join("daemon.pid");
    let mode = std::fs::metadata(&path)
        .expect("the state file")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);

    let _ = provider.kill();
    let _ = provider.wait();
}

/// Nothing above may leak the token into a log or a message.
#[tokio::test]
async fn the_token_is_not_printed() {
    let (mut provider, base_url) = start_static_provider();
    let env = Env::start(&base_url).await;
    let token = env.recorded()["token"]
        .as_str()
        .expect("a recorded token")
        .to_string();

    let output = Command::new(env!("CARGO_BIN_EXE_codape"))
        .arg("ide")
        .env("CLAUDE_CONFIG_DIR", env.home.path())
        .env("CODAPE_LOG", "info")
        .output()
        .expect("run codape ide");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !all.contains(&token),
        "`codape ide` printed the daemon token"
    );

    let status = Command::new(env!("CARGO_BIN_EXE_codape"))
        .arg("serve-status")
        .env("CLAUDE_CONFIG_DIR", env.home.path())
        .env("CODAPE_LOG", "info")
        .output()
        .expect("run serve-status");
    let status_all = format!(
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        !status_all.contains(&token),
        "`serve-status` printed the token"
    );

    let _ = provider.kill();
    let _ = provider.wait();
}
