//! A persisted output style through the real binary.
//!
//! `/style <name>` writes the chosen style to `output_style` in
//! settings.json, and the next start is supposed to apply it as the run's
//! appended prompt. Both modes share that promise: a headless run is still
//! the same person's preference, and it is the mode where the style matters
//! most, because nothing in the transcript can be adjusted afterwards.

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

/// Run a headless session whose settings file records `output_style`, and
/// return every request body the provider saw.
fn run_with_persisted_style(
    cwd: &Path,
    base_url: &str,
    extra_args: &[&str],
) -> std::process::Output {
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false, "output_style": "explanatory"}"#,
    )
    .expect("seed config");

    Command::new(env!("CARGO_BIN_EXE_codape"))
        .args(["-p"])
        .args(extra_args)
        .arg("hi")
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "test")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", config_home.path())
        .env("CODAPE_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run codape")
}

/// Serve every request the session makes (the turn, and the session-title
/// side query) and return their bodies.
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

#[tokio::test]
async fn a_persisted_style_reaches_a_headless_run() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let dir = tempfile::tempdir().expect("workspace");

    let server = tokio::spawn(serve_until_idle(listener));
    let output =
        tokio::task::spawn_blocking(move || run_with_persisted_style(dir.path(), &base_url, &[]))
            .await
            .expect("join");

    let bodies = server.await.expect("server task");
    assert!(
        output.status.success(),
        "codape failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!bodies.is_empty(), "no provider request was made");
    let text = serde_json::to_string(&bodies).expect("bodies");
    assert!(
        text.contains("explain the reasoning behind non-obvious decisions"),
        "the persisted output style is missing from the headless request: {text}"
    );
    assert!(
        text.contains("Operating mode: non-interactive"),
        "the headless operating note was lost: {text}"
    );
}

/// `--append-system-prompt` is a request for the run in front of the person,
/// so it outranks a persisted style. Headless folds the flag into the user
/// prompt rather than the append slot, which is why the precedence is decided
/// by the caller — a guard inside the repl would never see it.
#[tokio::test]
async fn an_explicit_append_flag_outranks_the_persisted_style_headless() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let dir = tempfile::tempdir().expect("workspace");

    let server = tokio::spawn(serve_until_idle(listener));
    let output = tokio::task::spawn_blocking(move || {
        run_with_persisted_style(
            dir.path(),
            &base_url,
            &["--append-system-prompt", "RUN-SPECIFIC-MARKER"],
        )
    })
    .await
    .expect("join");

    let bodies = server.await.expect("server task");
    assert!(
        output.status.success(),
        "codape failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = serde_json::to_string(&bodies).expect("bodies");
    assert!(
        text.contains("RUN-SPECIFIC-MARKER"),
        "the flag text is missing: {text}"
    );
    assert!(
        !text.contains("explain the reasoning behind non-obvious decisions"),
        "the persisted style was applied over an explicit flag: {text}"
    );
    assert!(
        text.contains("Operating mode: non-interactive"),
        "the operating note was lost: {text}"
    );
}
