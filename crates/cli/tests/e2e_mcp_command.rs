//! `/mcp add`, `/mcp remove` and `/mcp test` from inside a session.
//!
//! These commands used to answer with advice to run a different subcommand —
//! "MCP servers are added with `codape mcp add`" — from a session that could
//! have done it. The config parser and the client live in `codape-mcp`, which
//! the command registry cannot reach, so the registry now names the work and
//! the CLI carries it out. This drives the real binary to prove that path.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// A minimal MCP server: it answers `initialize` and `tools/list` over stdio
/// and does nothing else. Enough for `mcp test` to prove a connection.
const MCP_SERVER: &str = r#"#!/usr/bin/env python3
import json
import sys

def respond(rid, result):
    print(json.dumps({"jsonrpc": "2.0", "id": rid, "result": result}), flush=True)

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    request = json.loads(line)
    method = request.get("method")
    rid = request.get("id")
    if rid is None:
        continue
    if method == "initialize":
        respond(rid, {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock", "version": "1.0"},
        })
    elif method == "tools/list":
        respond(rid, {"tools": [{"name": "greet", "description": "Say hi",
                                 "inputSchema": {"type": "object"}}]})
    else:
        respond(rid, {})
"#;

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

    // The announcement goes to stderr, lowercased: `listening on 127.0.0.1:PORT`.
    // Read on its own thread so the pipe keeps draining past the announcement
    // — the provider would block on a full pipe otherwise — and hand the
    // address back through a channel, as the other e2e helpers do.
    let stderr = child.stderr.take().expect("provider stderr");
    let (tx, rx) = std::sync::mpsc::channel();
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
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the static provider did not announce its address");
    (child, format!("http://{address}"))
}

fn run_command(cwd: &Path, home: &Path, base_url: &str, command: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codape"))
        .args(["-p"])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("HOME", home)
        .env("CODAPE_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codape");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{command}\n").as_bytes())
        .expect("write command");
    let output = child.wait_with_output().expect("wait");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// `/mcp add` writes the server into `.mcp.json`, and `/mcp test` connects to
/// it and names the tool it offers. Before this, both answered with a pointer
/// to the CLI subcommand.
#[test]
fn mcp_add_writes_the_config_and_test_connects() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("home");

    let server = workspace.path().join("mock-mcp.py");
    std::fs::write(&server, MCP_SERVER).expect("write server");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755))
            .expect("chmod server");
    }

    let (mut provider, base_url) = start_static_provider();

    // The target is the interpreter plus the script: `add` takes the command
    // line verbatim, exactly as `codape mcp add` does.
    let target = format!("python3 {}", server.display());
    let added = run_command(
        workspace.path(),
        home.path(),
        &base_url,
        &format!("/mcp add mock {target}"),
    );
    assert!(
        added.contains("Added MCP server 'mock'"),
        "add did not report success: {added}"
    );
    assert!(
        !added.contains("codape mcp add"),
        "add still deflects to the CLI subcommand: {added}"
    );

    let config_path = workspace.path().join(".mcp.json");
    let config = std::fs::read_to_string(&config_path).expect(".mcp.json was not written");
    assert!(config.contains("mock"), "{config}");
    assert!(config.contains("mock-mcp.py"), "{config}");

    let tested = run_command(workspace.path(), home.path(), &base_url, "/mcp test mock");
    assert!(
        tested.contains("is running"),
        "the test did not connect: {tested}"
    );
    assert!(
        tested.contains("greet"),
        "the tool the server offers was not named: {tested}"
    );

    // Removal takes it back out, and a second removal says it is gone rather
    // than claiming a second success.
    let removed = run_command(workspace.path(), home.path(), &base_url, "/mcp remove mock");
    assert!(
        removed.contains("Removed MCP server 'mock'"),
        "remove did not report success: {removed}"
    );
    let config = std::fs::read_to_string(&config_path).expect(".mcp.json survives a removal");
    assert!(!config.contains("mock-mcp.py"), "{config}");

    let _ = provider.kill();
    let _ = provider.wait();
}

/// An unknown subcommand names the ones that exist instead of guessing.
#[test]
fn mcp_rejects_an_unknown_subcommand() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = tempfile::tempdir().expect("home");
    let (mut provider, base_url) = start_static_provider();

    let output = run_command(workspace.path(), home.path(), &base_url, "/mcp frobnicate");

    let _ = provider.kill();
    let _ = provider.wait();
    assert!(
        output.contains("list, add, remove or test"),
        "the error did not name the subcommands: {output}"
    );
}
