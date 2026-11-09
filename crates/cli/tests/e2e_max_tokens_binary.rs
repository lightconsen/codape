//! `CODAPE_MAX_TOKENS` through the real binary: the ceiling appears on the
//! provider request, and it is clamped so a raised one cannot swallow the
//! model's whole context window.
//!
//! The clamp matters as much as the raise. A provider rejects a request whose
//! `input + max_tokens` exceeds the window, and a ceiling at or above the whole
//! window fails on the very first request — before compaction has anything to
//! compact — so there is no recovery path.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn read_body(socket: &mut TcpStream) -> serde_json::Value {
    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 1024];
        let read = socket.read(&mut chunk).await.expect("read");
        assert!(read > 0, "closed before headers");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let mut chunk = vec![0u8; length.max(1)];
        let read = socket.read(&mut chunk).await.expect("read body");
        assert!(read > 0, "closed before body");
        buffer.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&buffer[header_end..header_end + length]).expect("request body")
}

async fn respond(socket: &mut TcpStream) {
    let response = serde_json::json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string();
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                response.len()
            )
            .as_bytes(),
        )
        .await
        .expect("write");
    socket.shutdown().await.expect("shutdown");
}

fn run_codape(cwd: &Path, base_url: &str, env: &[(&str, &str)]) -> std::process::Output {
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");

    let mut command = Command::new(env!("CARGO_BIN_EXE_codape"));
    command
        .args(["-p", "say hello"])
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "test")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", config_home.path())
        .env("CODAPE_LOG", "off")
        // The developer's own ceiling must not leak into a test that asserts
        // on the default; only `env` decides here.
        .env_remove("CODAPE_MAX_TOKENS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().expect("run codape")
}

/// Serve every request the session makes (the turn, and the session-title side
/// query) and return their bodies.
async fn serve_until_idle(listener: tokio::net::TcpListener) -> Vec<serde_json::Value> {
    let mut bodies = Vec::new();
    let mut idle = Duration::from_secs(20);
    loop {
        let Ok(Ok((mut socket, _))) = tokio::time::timeout(idle, listener.accept()).await else {
            break;
        };
        bodies.push(read_body(&mut socket).await);
        respond(&mut socket).await;
        idle = Duration::from_secs(3);
    }
    bodies
}

/// Run one headless turn with `env` and return the request bodies the provider
/// saw.
async fn bodies_for(env: &[(&str, &str)]) -> Vec<serde_json::Value> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let dir = tempfile::tempdir().expect("workspace");
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect();

    let server = tokio::spawn(serve_until_idle(listener));
    let output = tokio::task::spawn_blocking(move || {
        let refs: Vec<(&str, &str)> = env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        run_codape(dir.path(), &base_url, &refs)
    })
    .await
    .expect("join");

    let bodies = server.await.expect("server task");
    assert!(
        output.status.success(),
        "codape failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!bodies.is_empty(), "no provider request was made");
    bodies
}

/// The main agent turn, as opposed to the small side queries the session also
/// makes. The side queries carry their own fixed ceilings and are not what
/// this variable governs.
fn turn_request(bodies: &[serde_json::Value]) -> &serde_json::Value {
    bodies
        .iter()
        .find(|body| {
            body.get("max_tokens").is_some()
                && body
                    .get("messages")
                    .and_then(|messages| messages.as_array())
                    .is_some_and(|messages| !messages.is_empty())
        })
        .unwrap_or_else(|| panic!("no agent turn among {bodies:#?}"))
}

#[tokio::test]
async fn the_engine_default_is_used_when_the_variable_is_unset() {
    let bodies = bodies_for(&[]).await;
    let request = turn_request(&bodies);
    assert_eq!(
        request["max_tokens"], 32_768,
        "the unset ceiling changed: {request}"
    );
}

/// The point of the variable: a reasoning model on a hard step gets more room
/// without anyone having to write an agent definition for it.
#[tokio::test]
async fn the_environment_raises_the_ceiling_on_the_wire() {
    let bodies = bodies_for(&[("CODAPE_MAX_TOKENS", "65536")]).await;
    let request = turn_request(&bodies);
    assert_eq!(request["max_tokens"], 65_536, "{request}");
}

/// The environment can lower the ceiling too: a caller who wants short answers
/// and cheap retries asks for it explicitly.
#[tokio::test]
async fn the_environment_lowers_the_ceiling_on_the_wire() {
    let bodies = bodies_for(&[("CODAPE_MAX_TOKENS", "4096")]).await;
    let request = turn_request(&bodies);
    assert_eq!(request["max_tokens"], 4_096, "{request}");
}

/// DeepSeek's window is 64 000. A 65 536 ceiling is more than the whole
/// window, and no context size could make that request acceptable — the
/// provider rejects `input + max_tokens > window` and the only remedy is
/// compaction, which cannot bring a 44 128-token request under half the window
/// either. So the ceiling itself is cut to half the window.
#[tokio::test]
async fn the_ceiling_is_clamped_to_a_small_context_window() {
    let bodies = bodies_for(&[
        ("CODAPE_MAX_TOKENS", "65536"),
        ("ANTHROPIC_MODEL", "deepseek-chat"),
    ])
    .await;
    let request = turn_request(&bodies);
    let requested = request["max_tokens"].as_u64().expect("a number");
    assert!(
        requested * 2 <= 64_000,
        "more than half of DeepSeek's window went to output: {request}"
    );
    assert_eq!(requested, 32_000, "half of 64 000: {request}");
}

/// The clamp is for a value someone asked for. A session that asked for
/// nothing keeps the engine default, even on a model whose window would have
/// forced a clamp.
#[tokio::test]
async fn the_default_is_not_clamped_by_a_small_window() {
    let bodies = bodies_for(&[("ANTHROPIC_MODEL", "deepseek-chat")]).await;
    let request = turn_request(&bodies);
    assert_eq!(
        request["max_tokens"], 32_768,
        "the unset ceiling moved with the window: {request}"
    );
}

/// A value that cannot be a ceiling is ignored rather than sent.
#[tokio::test]
async fn a_malformed_value_falls_back_to_the_default() {
    for value in ["0", "-5", "plenty"] {
        let bodies = bodies_for(&[("CODAPE_MAX_TOKENS", value)]).await;
        let request = turn_request(&bodies);
        assert_eq!(
            request["max_tokens"], 32_768,
            "{value:?} reached the provider: {request}"
        );
    }
}
