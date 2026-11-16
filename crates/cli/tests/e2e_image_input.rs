//! User-side image input through the real binary.
//!
//! The Read tool could already forward an image a *tool* found; these tests are
//! about the person attaching one. `--image` and an `@path` mention must both
//! reach an OpenAI-shaped provider as an `image_url` part on the user turn, and
//! a path that is not a usable image must fail loudly at startup rather than
//! send a text-only prompt.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn read_request(socket: &mut TcpStream) -> serde_json::Value {
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

fn chunk(delta: &serde_json::Value, finish: Option<&str>) -> String {
    let value = serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "gpt-4o",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    format!(
        "data: {}\n\n",
        serde_json::to_string(&value).expect("chunk")
    )
}

/// Run one turn and return every body the capture server saw.
async fn capture(args: &[&str], prompt: &str) -> Vec<serde_json::Value> {
    capture_in(args, prompt, |_| {}).await
}

/// Run one turn in a workspace seeded by `seed`, and return every body the
/// capture server saw.
///
/// The prompt's `@path` mentions resolve against this workspace, so a test
/// that names a relative path has to put the file here rather than in a
/// directory of its own.
async fn capture_in(
    args: &[&str],
    prompt: &str,
    seed: impl FnOnce(&Path) + Send + 'static,
) -> Vec<serde_json::Value> {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}", listener.local_addr().expect("addr"));
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let seen = Arc::clone(&bodies);

    let server = tokio::spawn(async move {
        loop {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_secs(10), listener.accept()).await
            else {
                break;
            };
            let body = read_request(&mut socket).await;
            seen.lock().unwrap().push(body);
            let sse = [
                chunk(
                    &serde_json::json!({"role": "assistant", "content": "image answer"}),
                    None,
                ),
                chunk(&serde_json::json!({}), Some("stop")),
                "data: [DONE]\n\n".to_string(),
            ]
            .concat();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
                        sse.len()
                    )
                    .as_bytes(),
                )
                .await
                .expect("write");
            socket.shutdown().await.expect("shutdown");
        }
    });

    let dir = tempfile::tempdir().expect("workspace");
    seed(dir.path());
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");
    let cwd = dir.path().to_path_buf();
    let base_for_child = base_url.clone();
    let home = config_home.path().to_path_buf();
    let owned: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    let prompt = prompt.to_string();
    tokio::task::spawn_blocking(move || run(&cwd, &base_for_child, &home, &owned, &prompt))
        .await
        .expect("join");
    server.abort();
    let _ = server.await;
    bodies.lock().unwrap().clone()
}

fn write_png(dir: &Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    // The loader never decodes; only the base64 round-trip matters.
    std::fs::write(&path, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]).expect("write png");
    path
}

fn run(
    cwd: &Path,
    base_url: &str,
    config_home: &Path,
    args: &[String],
    prompt: &str,
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codape"))
        .args(["-p", "--model", "gpt-4o"])
        .args(args)
        .current_dir(cwd)
        .env("OPENAI_API_KEY", "test-key")
        .env("OPENAI_BASE_URL", base_url)
        .env("OPENAI_MODEL", "gpt-4o")
        .env("CODAPE_PROVIDER_PROFILE", "openai")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("ANTHROPIC_MODEL")
        .env("CLAUDE_CONFIG_DIR", config_home)
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
        .write_all(prompt.as_bytes())
        .expect("write prompt");
    child.wait_with_output().expect("wait")
}

/// The user turn in the captured request, whichever message carries the
/// image part.
fn find_image_part(body: &serde_json::Value) -> Option<serde_json::Value> {
    let messages = body["messages"].as_array()?;
    for message in messages {
        if let Some(parts) = message["content"].as_array() {
            for part in parts {
                if part["type"] == "image_url" {
                    return Some(part.clone());
                }
            }
        }
    }
    None
}

#[tokio::test]
async fn an_image_flag_reaches_the_provider_as_an_image_part() {
    let dir = tempfile::tempdir().expect("workspace");
    let path = write_png(dir.path(), "shot.png");
    let bodies = capture(
        &["--image", path.to_str().expect("path")],
        "what is wrong here?",
    )
    .await;

    assert!(!bodies.is_empty(), "no provider request was made");
    let part = find_image_part(&bodies[0]).expect("no image_url part in the request");
    let url = part["image_url"]["url"].as_str().expect("data url");
    assert!(url.starts_with("data:image/png;base64,"), "{url}");
    // The text must still be there: a picture alone loses the question.
    let body = bodies[0].to_string();
    assert!(body.contains("what is wrong here?"), "{body}");
}

#[tokio::test]
async fn an_image_mention_reaches_the_provider_as_an_image_part() {
    let bodies = capture_in(&[], "describe @shot.png", |dir| {
        write_png(dir, "shot.png");
    })
    .await;

    assert!(!bodies.is_empty(), "no provider request was made");
    let part = find_image_part(&bodies[0])
        .expect("an @path naming an image must reach the provider as one");
    assert!(
        !part["image_url"]["url"]
            .as_str()
            .unwrap_or_default()
            .is_empty()
    );
}

#[tokio::test]
async fn a_file_flag_naming_an_image_is_attached_as_an_image() {
    let dir = tempfile::tempdir().expect("workspace");
    let path = write_png(dir.path(), "shot.png");
    let bodies = capture(&["--file", path.to_str().expect("path")], "look").await;

    assert!(!bodies.is_empty(), "no provider request was made");
    assert!(
        find_image_part(&bodies[0]).is_some(),
        "--file with an image used to fail read_to_string; it must attach it instead"
    );
}

#[tokio::test]
async fn an_unreadable_image_fails_before_a_request_is_made() {
    let dir = tempfile::tempdir().expect("workspace");
    let path = dir.path().join("missing.png");
    let bodies = capture(&["--image", path.to_str().expect("path")], "look").await;

    assert!(
        bodies.is_empty(),
        "a missing image must fail at startup, not send a text-only turn"
    );
}

#[tokio::test]
async fn a_non_image_file_is_refused_by_the_image_flag() {
    let dir = tempfile::tempdir().expect("workspace");
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, "hello").expect("write");
    let bodies = capture(&["--image", path.to_str().expect("path")], "look").await;

    assert!(bodies.is_empty(), "a text file is not an image");
}
