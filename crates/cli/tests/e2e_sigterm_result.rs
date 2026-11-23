//! What the `result` frame says when SIGTERM ends a run mid-turn.
//!
//! The turn task owns the engine for the length of a submit, so the REPL cannot
//! reach it from outside. The first version of this read `self.session.engine`
//! in its SIGTERM branch — a field the turn had already `take()`n — so the
//! branch always found `None` and wrote an `interrupted` frame with no `usage`,
//! no `model_ms` and no `tool_ms`. A terminal-bench run that had made 42 model
//! calls and ran for thirty minutes reported `tokens: unavailable` and
//! `model=0.0s tools=0.0s`, which is the run a benchmark most wants a cost for.
//!
//! A unit test cannot catch that: it hands the frame builder the tracker itself,
//! so the value the check needs is supplied by the check. Only a real process,
//! killed the way the harness kills it, exercises the call site. This test is
//! that process — it asserts on the frame the binary actually writes, and it
//! fails if the branch stops passing the tracker even though every other test
//! in the suite would still pass.
//!
//! The fix has a twin failure the same file has to rule out. The frame is built
//! from the *history* of finished turns plus the counters of the turn in
//! flight, and a finished turn stays in both until the next one starts — so a
//! run that completes reports its last turn twice. The second test below is the
//! other side of that pair: same provider, allowed to finish, asserting on the
//! totals it wrote.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// A provider that keeps calling one tool, and can leave the call unanswered.
///
/// The phases are what make the assertions meaningful. Each turn has to
/// complete so its usage is banked and the tool's time is charged; the request
/// that carries the tool result has to still be open when the signal arrives,
/// which is the state the bug was found in — a turn that had been running for
/// half an hour with the engine owned by a task.
///
/// `providers` counts only the requests the engine makes for the turn. There is
/// a second kind of request on this endpoint: the permission engine asks the
/// model to explain the call it is about to make, and that one has to be
/// answered or the tool never runs. It is recognised by its system prompt and
/// answered with plain text.
struct HangingProvider {
    base_url: String,
    turn_requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HangingProvider {
    /// `answer_follow_up` decides whether the request carrying the tool result
    /// gets an answer. `false` is the SIGTERM case: the turn is still waiting
    /// when the signal lands. `true` lets the turn continue, so the same
    /// provider measures the completed-turn frame — the pair is what shows the
    /// in-progress half is added only when there really is a turn in progress.
    fn start(answer_follow_up: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind hanging provider");
        listener.set_nonblocking(true).expect("nonblocking");
        let base_url = format!("http://{}", listener.local_addr().expect("addr"));
        let turn_requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let counted = Arc::clone(&turn_requests);
        let stopping = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Err(_) => break,
                };
                let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
                // A connection that never finishes its request must not stop
                // the loop from accepting the next one.
                let _ = serve(socket, &counted, answer_follow_up, &stopping);
            }
        });
        Self {
            base_url,
            turn_requests,
            stop,
            handle: Some(handle),
        }
    }

    fn turn_requests(&self) -> usize {
        self.turn_requests.load(Ordering::SeqCst)
    }
}

impl Drop for HangingProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Answer one request: a tool call, then either nothing or another tool call.
///
/// The permission engine's "explain what this call risks" request shares this
/// endpoint, and it has to be answered or `Bash` never runs — the turn would
/// then have no tool time to report for a reason that is the test's fault, not
/// the binary's. It carries the advisor's system prompt and gets plain text.
fn serve(
    mut socket: std::net::TcpStream,
    turn_requests: &Arc<AtomicUsize>,
    answer_follow_up: bool,
    stop: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let Some(body) = read_request(&mut socket)? else {
        return Ok(());
    };

    socket.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
    )?;

    if body.contains("You are a security advisor") {
        socket.write_all(
            b"event: content_block_start\n\
              data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
              event: content_block_delta\n\
              data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Reads a file; no side effects.\"}}\n\n\
              event: content_block_stop\n\
              data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
              event: message_delta\n\
              data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":30,\"output_tokens\":9}}\n\n\
              event: message_stop\n\
              data: {\"type\":\"message_stop\"}\n\n",
        )?;
        return socket.flush();
    }

    let ordinal = turn_requests.fetch_add(1, Ordering::SeqCst);

    // A follow-up carries the tool result, which is what tells it from the
    // first request: the turn has already done its work by then.
    let after_tool = body.contains("\"type\":\"tool_result\"");
    if after_tool || ordinal > 0 {
        if answer_follow_up {
            // Another tool call, not a closing message: the double count this
            // provider's caller looks for lands on the *last* turn, and a turn
            // that only writes text has no tool time to detect it with.
            socket.write_all(TOOL_TURN)?;
            return socket.flush();
        }
        // The turn is mid-flight and the signal arrives while this request is
        // open. A provider that answered would let the turn finish, and a
        // finished turn builds its closing frame from the engine it hands
        // back — the path that already worked.
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
        }
        return Ok(());
    }

    // A real subprocess with a real duration, so `tool_ms` has something to
    // report. `sleep` keeps the binary's own work out of that number.
    socket.write_all(TOOL_TURN)?;
    socket.flush()
}

/// The first turn: one `Bash` call that takes long enough to measure.
const TOOL_TURN: &[u8] = b"event: content_block_start\n\
      data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"Bash\",\"input\":{}}}\n\n\
      event: content_block_delta\n\
      data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"sleep 2\\\"}\"}}\n\n\
      event: content_block_stop\n\
      data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
      event: message_delta\n\
      data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":1200,\"output_tokens\":340}}\n\n\
      event: message_stop\n\
      data: {\"type\":\"message_stop\"}\n\n";

/// Read one HTTP request. `Ok(None)` means it ended before a full request.
fn read_request(socket: &mut std::net::TcpStream) -> std::io::Result<Option<String>> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break position + 4;
        }
        let read = socket.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
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
        let read = socket.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Ok(Some(String::from_utf8_lossy(&buffer).to_string()))
}

fn wait_for_second_request(provider: &HangingProvider) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while provider.turn_requests() < 2 {
        assert!(
            Instant::now() < deadline,
            "the turn never reached its follow-up request; saw {} turn request(s)",
            provider.turn_requests()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn seed_config_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("seed config");
    home
}

fn spawn_headless(cwd: &Path, home: &Path, base_url: &str) -> Child {
    spawn_headless_with(cwd, home, base_url, &[])
}

fn spawn_headless_with(cwd: &Path, home: &Path, base_url: &str, extra: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_codape"))
        // The prompt is positional: the prompt read from stdin and a control
        // channel both want stdin, and this test speaks to neither.
        .args([
            "-p",
            "--dangerously-skip-permissions",
            "--output-format",
            "stream-json",
            "--no-session-persistence",
        ])
        .args(extra)
        .arg("wait for the build to finish")
        .current_dir(cwd)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", "claude-sonnet-4-6")
        .env("CLAUDE_CONFIG_DIR", home)
        .env("CODAPE_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn codape")
}

fn send_sigterm(pid: u32) {
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -TERM {pid} failed");
}

/// Read frames until the `result` one arrives.
fn read_frames(stdout: std::process::ChildStdout, limit: Duration) -> Vec<serde_json::Value> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + limit;
    let mut frames = Vec::new();
    while Instant::now() < deadline {
        let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let is_result = frame["type"] == "result";
        frames.push(frame);
        if is_result {
            break;
        }
    }
    frames
}

#[test]
fn a_sigterm_result_frame_reports_what_the_turn_spent() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = seed_config_home();
    let provider = HangingProvider::start(false);

    let mut child = spawn_headless(workspace.path(), home.path(), &provider.base_url);
    let stdout = child.stdout.take().expect("stdout");

    // The second request exists only after the first turn completed and its
    // tool ran, so the usage and the tool time below are already banked when
    // the signal arrives.
    wait_for_second_request(&provider);
    // Let the open request accumulate some provider time: the frame's
    // `model_ms` is read from the turn in progress, and a clock that has not
    // ticked reports zero for a reason that is not the bug.
    std::thread::sleep(Duration::from_millis(300));

    send_sigterm(child.id());
    let frames = read_frames(stdout, Duration::from_secs(60));
    let status = child.wait().expect("wait");
    drop(provider);

    let result = frames
        .iter()
        .find(|frame| frame["type"] == "result")
        .unwrap_or_else(|| panic!("no result frame after SIGTERM: {frames:#?}"));

    assert_eq!(
        result["subtype"], "interrupted",
        "a SIGTERM did not produce an interrupted result: {result}"
    );
    // `timeout --signal=TERM` is how every benchmark harness ends a run that
    // exceeds its deadline, and this is the shape it observes.
    assert_eq!(
        status.code(),
        Some(143),
        "the process did not exit as a signal-terminated run: {status:?}"
    );

    // The three keys this test exists for. Each is absent when the SIGTERM
    // branch cannot reach the tracker, which is the bug: the frame carried
    // `num_turns` and `duration_ms` and nothing about what the run cost.
    let usage = result
        .get("usage")
        .unwrap_or_else(|| panic!("the interrupted result carried no usage: {result}"));
    assert_eq!(
        usage["input_tokens"], 1200,
        "the interrupted result lost the turn's input tokens: {result}"
    );
    assert_eq!(
        usage["output_tokens"], 340,
        "the interrupted result lost the turn's output tokens: {result}"
    );

    let model_ms = result
        .get("model_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("no model_ms on the interrupted result: {result}"));
    let tool_ms = result
        .get("tool_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("no tool_ms on the interrupted result: {result}"));

    // The completed first turn is in `turn_history` and the open request is in
    // the in-progress counters; both halves have to be summed, because a run
    // killed during its first turn has no history at all.
    assert!(
        model_ms > 0,
        "the interrupted result reported no provider time: {result}"
    );
    assert!(
        tool_ms > 0,
        "the interrupted result reported no tool time for a turn that ran `sleep 2`: {result}"
    );

    let turns = result["num_turns"].as_u64().expect("num_turns");
    assert!(
        turns >= 1,
        "the interrupted result reported no turns: {result}"
    );
}

/// The same run allowed to finish reports each turn exactly once.
///
/// `timing` adds the completed turns' history to the counters for the turn in
/// progress. A turn that already closed is in the history and is still sitting
/// in those counters, so this frame must *not* include it again: the bug that
/// makes an interrupted frame empty has an opposite twin that makes a completed
/// frame report its last turn twice, and only this side of the pair shows it.
#[test]
fn a_completed_result_frame_does_not_double_count_its_last_turn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let home = seed_config_home();
    let provider = HangingProvider::start(true);

    // Two turns, so the *last* one carries the tool. That is the only shape
    // that exposes the double count: `timing` adds the history to the
    // in-progress counters, and a closed turn is still in those counters, so
    // it is the final turn — the one whose numbers sit in both books — that
    // gets counted twice. With a single text turn the doubled turn has no tool
    // time and the assertion below passes either way.
    let mut child = spawn_headless_with(
        workspace.path(),
        home.path(),
        &provider.base_url,
        &["--max-turns", "2"],
    );
    let stdout = child.stdout.take().expect("stdout");
    let frames = read_frames(stdout, Duration::from_secs(90));
    let status = child.wait().expect("wait");
    drop(provider);

    let result = frames
        .iter()
        .find(|frame| frame["type"] == "result")
        .unwrap_or_else(|| panic!("no result frame: {frames:#?}"));
    assert_eq!(
        result["subtype"], "error_max_turns",
        "the run did not stop at its turn limit: {result}"
    );
    assert!(status.success(), "codape exited {status:?}");

    let tool_ms = result
        .get("tool_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("no tool_ms on the result frame: {result}"));
    let turns = result["num_turns"].as_u64().expect("num_turns");

    // One `sleep 2` per turn, so two turns spend about four seconds in tools
    // and the run as a whole lasts a shade over that. Tool time is a subset of
    // wall clock and can never exceed it — unless the last turn is in the books
    // twice, which adds a whole extra `sleep 2` and puts the total a clear two
    // seconds over. The bound needs no tuned constant, so a slow machine moves
    // both sides together instead of tripping it.
    assert!(tool_ms > 0, "the tool turns reported no time: {result}");
    assert_eq!(
        turns, 2,
        "expected one tool call on each of two turns: {result}"
    );
    let duration_ms = result["duration_ms"].as_u64().expect("duration_ms");
    assert!(
        tool_ms < duration_ms,
        "the last turn's {tool_ms}ms of tool time cannot fit in a {duration_ms}ms run; \
         it was counted in both the history and the in-progress counters: {result}"
    );
}
