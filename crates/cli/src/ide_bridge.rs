//! IDE / editor integration — a stdio MCP server that drives a running daemon.
//!
//! An editor cannot usefully spawn `codape serve`: a daemon owns a socket, a
//! session and a working directory, and an editor's MCP client speaks a
//! different protocol over stdio. This module is the adapter between the two.
//!
//! `codape ide bridge` is what an editor launches. It is an MCP server from the
//! editor's side and a WebSocket client from the daemon's side, so an editor
//! that knows nothing about Codape — only about MCP — can send prompts, watch
//! the tools a turn runs, and answer the prompts those tools raise.
//!
//! The permission path is what makes this an integration rather than a remote
//! control. A turn that wants to write a file stops and asks; the bridge
//! reports that as the tool call's result, naming the request, and the next
//! call carries the answer. Silence holds the turn open, which the tool
//! descriptions say explicitly: an editor-side model that forgets to answer
//! leaves the daemon waiting, not guessing.
//!
//! Everything written to stdout is protocol. Nothing else in this module
//! prints there; diagnostics go through `tracing`, which goes to stderr. That
//! is also why [`maybe_run_ide_bridge`] is called before the startup
//! orchestrator runs — the startup phases narrate themselves on stdout, and a
//! sentence in the middle of a JSON-RPC stream breaks the editor's parser.

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// The MCP server name an editor registers this bridge under.
pub const SERVER_NAME: &str = "codape";

/// How long a single read may wait on the daemon before the bridge gives up.
///
/// A turn legitimately runs for minutes, so this is generous; it exists to
/// stop a wedged daemon from hanging the editor forever, not to bound a turn.
const FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// JSON-RPC's "invalid params" code.
const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC's "internal error" code.
const INTERNAL_ERROR: i64 = -32603;
/// JSON-RPC's "method not found" code.
const METHOD_NOT_FOUND: i64 = -32601;

/// Whether this process was started as the bridge, `codape ide bridge`.
///
/// Parsed by hand rather than through clap because the point is to run before
/// startup: the bridge must not load settings, spawn MCP servers of its own,
/// or print a welcome banner onto the protocol stream.
#[must_use]
pub fn is_ide_bridge() -> bool {
    let mut args = std::env::args_os();
    let _program = args.next();
    args.next().as_deref() == Some(std::ffi::OsStr::new("ide"))
        && args.next().as_deref() == Some(std::ffi::OsStr::new("bridge"))
}

/// Run the stdio MCP bridge to completion and return the process exit code.
///
/// Awaited by `main` rather than given a runtime of its own: `main` already
/// has one, and building a second inside it panics.
pub async fn run_ide_bridge() -> i32 {
    serve().await
}

/// The configuration snippet an editor needs.
///
/// The binary path is absolute because an editor does not launch the bridge
/// through a login shell, so `PATH` may not contain the directory `codape` was
/// installed into; a bare `codape` is the one spelling most likely to fail.
#[must_use]
pub fn editor_config() -> Value {
    let program = std::env::current_exe().map_or_else(
        |_| "codape".to_string(),
        |path| path.to_string_lossy().to_string(),
    );
    json!({
        "mcpServers": {
            SERVER_NAME: {
                "command": program,
                "args": ["ide", "bridge"]
            }
        }
    })
}

/// What `codape ide` prints: how to register the bridge, and where it will
/// connect.
#[must_use]
pub fn describe() -> String {
    use std::fmt::Write;

    let mut out = String::new();
    out.push_str("An editor reaches Codape by launching this bridge as an MCP server.\n");
    out.push_str("Add this to the editor's MCP configuration:\n\n");
    match serde_json::to_string_pretty(&editor_config()) {
        Ok(text) => out.push_str(&text),
        // Writing into a `String` cannot fail.
        Err(error) => {
            let _ = write!(out, "(could not render the snippet: {error})");
        }
    }
    out.push_str("\n\n");

    match crate::daemon::read_daemon_state() {
        Some(state) if crate::daemon::is_process_running(state.pid) => {
            if state.address.is_empty() {
                let _ = writeln!(
                    out,
                    "A daemon is running (PID {}) but its file records no address. Restart it \
                     with this build so an editor can find it.",
                    state.pid
                );
            } else {
                let _ = writeln!(out, "Daemon: {} (PID {})", state.websocket_url(), state.pid);
            }
            if state.token.is_none() {
                out.push_str(
                    "That daemon recorded no token, so the bridge cannot authenticate. It was \
                     started with an explicit key list; export CODAPE_API_KEYS in the shell \
                     that starts the editor.\n",
                );
            }
        }
        Some(state) => {
            let _ = writeln!(
                out,
                "A daemon file is left over (PID {}) but that process is not running. Start one \
                 with `codape serve`.",
                state.pid
            );
        }
        None => out.push_str(
            "No daemon is running. The bridge connects when the first prompt arrives, so this is \
             fine — but a prompt sent before `codape serve` is running will be refused with that \
             reason.\n",
        ),
    }
    out
}

/// The WebSocket the bridge holds against the daemon.
type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// One open WebSocket session with the daemon.
struct Conn {
    /// The write half.
    sink: futures::stream::SplitSink<WsStream, tokio_tungstenite::tungstenite::Message>,
    /// Frames the daemon sent, parsed, in arrival order.
    frames: tokio::sync::mpsc::UnboundedReceiver<Value>,
}

/// The bridge's state across tool calls.
#[derive(Default)]
struct Bridge {
    /// The open connection, kept across calls so a turn survives one.
    conn: Option<Conn>,
    /// The turn currently running, or paused on a prompt.
    open_turn: Option<OpenTurn>,
}

/// A turn the daemon is still running.
#[derive(Debug)]
struct OpenTurn {
    /// Output produced since the bridge last handed some to the editor.
    pending: String,
    /// The prompt the turn is waiting on, once it reached one.
    awaiting: Option<Prompt>,
}

/// A permission prompt the daemon is waiting on.
#[derive(Debug, Clone)]
struct Prompt {
    request_id: String,
    tool: String,
    reason: String,
    args: Value,
    allow_session: bool,
}

/// A JSON-RPC error, as the dispatcher returns them.
type RpcResult = Result<Value, (i64, String)>;

/// The stdio MCP loop.
async fn serve() -> i32 {
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut bridge = Bridge::default();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(line) else {
            // A malformed frame carries no id to answer; skipping it is the
            // only move that keeps the stream usable.
            continue;
        };
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            // A response to a request this bridge never sends. Nothing here is
            // waiting on one.
            continue;
        };
        // A notification has no id and takes no answer.
        let Some(id) = request.get("id").cloned() else {
            continue;
        };

        let params = request.get("params").cloned();
        let response = match bridge.dispatch(method, params.as_ref()).await {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": code, "message": message}
            }),
        };
        if write_frame(&response).await.is_err() {
            break;
        }
    }

    // Leaving drops the socket. The daemon sees the disconnect and abandons
    // any prompt the turn was waiting on — the honest outcome, since the
    // editor that would have answered it is gone.
    0
}

/// Write one JSON-RPC frame to stdout and flush it.
async fn write_frame(frame: &Value) -> std::io::Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut text = frame.to_string();
    text.push('\n');
    stdout.write_all(text.as_bytes()).await?;
    stdout.flush().await
}

impl Bridge {
    /// Answer one JSON-RPC request.
    async fn dispatch(&mut self, method: &str, params: Option<&Value>) -> RpcResult {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")}
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_definitions()})),
            "tools/call" => self.call_tool(params).await,
            other => Err((
                METHOD_NOT_FOUND,
                format!("this bridge does not implement '{other}'"),
            )),
        }
    }

    /// Run one tool call.
    async fn call_tool(&mut self, params: Option<&Value>) -> RpcResult {
        let params = params.ok_or_else(|| (INVALID_PARAMS, "missing params".to_string()))?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| (INVALID_PARAMS, "missing tool name".to_string()))?
            .to_string();
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

        let text = match name.as_str() {
            "ask" => self.ask(&arguments).await?,
            "answer_permission" => self.answer(&arguments).await?,
            "cancel" => self.cancel().await?,
            "status" => describe(),
            other => return Err((INVALID_PARAMS, format!("unknown tool '{other}'"))),
        };

        Ok(json!({
            "content": [{"type": "text", "text": text}],
            "isError": false
        }))
    }

    /// Send a prompt and pump the turn until it finishes or needs permission.
    async fn ask(&mut self, arguments: &Value) -> Result<String, (i64, String)> {
        let prompt = arguments
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|prompt| !prompt.trim().is_empty())
            .ok_or_else(|| {
                (
                    INVALID_PARAMS,
                    "'prompt' must be a non-empty string".to_string(),
                )
            })?
            .to_string();

        if let Some(turn) = &self.open_turn {
            // A second prompt would queue behind a turn that is stopped, and
            // the editor would never see either answer. Say which call is
            // blocking rather than starting a turn that cannot run.
            let detail = turn.awaiting.as_ref().map_or_else(
                || "a turn is still running".to_string(),
                |waiting| {
                    format!(
                        "the running turn is paused waiting for permission to run '{}' \
                         (request {})",
                        waiting.tool, waiting.request_id
                    )
                },
            );
            return Err((
                INVALID_PARAMS,
                format!("{detail}; answer it with answer_permission, or cancel the turn first"),
            ));
        }

        let query_id = format!("ide_{}", uuid::Uuid::new_v4().simple());
        let conn = self.connect().await?;
        conn.send(&json!({
            "id": query_id,
            "type": "query",
            "content": prompt
        }))
        .await?;

        self.open_turn = Some(OpenTurn {
            pending: String::new(),
            awaiting: None,
        });
        self.pump().await
    }

    /// Answer the prompt a paused turn is waiting on, then resume pumping.
    async fn answer(&mut self, arguments: &Value) -> Result<String, (i64, String)> {
        let request_id = arguments
            .get("request_id")
            .and_then(Value::as_str)
            .ok_or_else(|| (INVALID_PARAMS, "'request_id' is required".to_string()))?
            .to_string();
        let decision = arguments
            .get("decision")
            .and_then(Value::as_str)
            .ok_or_else(|| (INVALID_PARAMS, "'decision' is required".to_string()))?
            .to_string();
        if !matches!(decision.as_str(), "approved" | "approve_session" | "denied") {
            return Err((
                INVALID_PARAMS,
                format!(
                    "'decision' must be one of approved, approve_session, denied; got \
                     '{decision}'"
                ),
            ));
        }

        let expected = self
            .open_turn
            .as_ref()
            .ok_or_else(|| {
                (
                    INVALID_PARAMS,
                    "no turn is running, so there is no prompt to answer".to_string(),
                )
            })?
            .awaiting
            .as_ref()
            .ok_or_else(|| {
                (
                    INVALID_PARAMS,
                    "the running turn is not waiting for permission".to_string(),
                )
            })?
            .request_id
            .clone();
        if expected != request_id {
            return Err((
                INVALID_PARAMS,
                format!("the turn is waiting on request {expected}, not {request_id}"),
            ));
        }

        let conn = self.connect().await?;
        conn.send(&json!({
            "id": request_id,
            "type": "permission_response",
            "decision": decision
        }))
        .await?;
        if let Some(turn) = &mut self.open_turn {
            turn.awaiting = None;
        }
        self.pump().await
    }

    /// Interrupt the running turn.
    async fn cancel(&mut self) -> Result<String, (i64, String)> {
        if self.open_turn.is_none() {
            return Ok("No turn is running.".to_string());
        }
        let conn = self.connect().await?;
        conn.send(&json!({"id": "interrupt", "type": "interrupt"}))
            .await?;
        Ok("Interrupt sent.".to_string())
    }

    /// Read frames until the turn finishes or asks for permission.
    ///
    /// Each return hands back what the turn produced since the previous one,
    /// so a caller that resumes after a prompt sees only the new part rather
    /// than the whole transcript again.
    async fn pump(&mut self) -> Result<String, (i64, String)> {
        loop {
            let frame = {
                let conn = self
                    .conn
                    .as_mut()
                    .ok_or_else(|| (INTERNAL_ERROR, "not connected".to_string()))?;
                conn.next_frame().await?
            };

            match frame.get("type").and_then(Value::as_str) {
                Some("stream_chunk") => {
                    let chunk = text_of(&frame, "chunk");
                    if let Some(turn) = &mut self.open_turn {
                        turn.pending.push_str(&chunk);
                    }
                }
                Some("tool_call_start") => {
                    let line = format!(
                        "\n[{}] {}\n",
                        text_of(&frame, "name"),
                        text_of(&frame, "summary")
                    );
                    if let Some(turn) = &mut self.open_turn {
                        turn.pending.push_str(&line);
                    }
                }
                Some("tool_call_end") => {
                    let failed = frame["result"]["is_error"].as_bool().unwrap_or(false);
                    let output = frame["result"]["output"].as_str().unwrap_or("");
                    let line = if failed {
                        format!("\n[tool failed] {}\n", first_line(output))
                    } else {
                        format!("\n[tool done] {}\n", first_line(output))
                    };
                    if let Some(turn) = &mut self.open_turn {
                        turn.pending.push_str(&line);
                    }
                }
                Some("notification") => {
                    let message = text_of(&frame, "message");
                    if let Some(turn) = &mut self.open_turn {
                        turn.pending.push('\n');
                        turn.pending.push_str(&message);
                        turn.pending.push('\n');
                    }
                }
                Some("permission_request") => {
                    let prompt = Prompt {
                        request_id: text_of(&frame, "request_id"),
                        tool: text_of(&frame, "tool"),
                        reason: text_of(&frame, "reason"),
                        args: frame.get("args").cloned().unwrap_or(Value::Null),
                        allow_session: frame
                            .get("allow_session")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    };
                    let pending = self
                        .open_turn
                        .as_mut()
                        .map(|turn| std::mem::take(&mut turn.pending))
                        .unwrap_or_default();
                    let request_id = prompt.request_id.clone();
                    if let Some(turn) = &mut self.open_turn {
                        turn.awaiting = Some(prompt.clone());
                    }
                    // The request id sits on a line of its own: it is the one
                    // thing the next call has to echo exactly, and a reader
                    // pulling it out of prose is a reader that gets it wrong.
                    return Ok(format!(
                        "{pending}\nThe turn is paused: it wants to run '{}'.\nReason: {}\n\
                         Arguments: {}\n{}\n\nAnswer it with answer_permission:\n  \
                         request_id: {request_id}\n  decision: approved | denied{}\n\nUntil that \
                         arrives the turn stays open and no other prompt can be sent.",
                        prompt.tool,
                        prompt.reason,
                        prompt.args,
                        if prompt.allow_session {
                            "A session-wide approval is offered (decision: approve_session)."
                        } else {
                            "This call is a one-off: decision approved or denied."
                        },
                        if prompt.allow_session {
                            " | approve_session"
                        } else {
                            ""
                        }
                    ));
                }
                Some("query_done") => {
                    let reason = text_of(&frame, "reason");
                    let pending = self
                        .open_turn
                        .take()
                        .map_or_else(String::new, |turn| turn.pending);
                    let note = if reason == "completed" {
                        String::new()
                    } else {
                        format!("\n(the turn ended: {reason})")
                    };
                    return Ok(format!("{pending}{note}"));
                }
                Some("error") => {
                    let code = text_of(&frame, "code");
                    let message = text_of(&frame, "message");
                    // A refused request never gets a `query_done`, so the turn
                    // is over here whether or not the daemon agrees.
                    self.open_turn = None;
                    return Err((INTERNAL_ERROR, format!("{code}: {message}")));
                }
                // `pong`, `permission_recorded`, `command_resolved`, and frames
                // this bridge has no use for.
                _ => {}
            }
        }
    }

    /// The open connection, connecting if this is the first call.
    async fn connect(&mut self) -> Result<&mut Conn, (i64, String)> {
        if self.conn.is_none() {
            self.conn = Some(open_daemon_connection().await?);
        }
        self.conn
            .as_mut()
            .ok_or_else(|| (INTERNAL_ERROR, "not connected".to_string()))
    }
}

impl Conn {
    /// Send one JSON frame.
    async fn send(&mut self, frame: &Value) -> Result<(), (i64, String)> {
        use futures::SinkExt;
        let message = tokio_tungstenite::tungstenite::Message::Text(frame.to_string().into());
        self.sink.send(message).await.map_err(|error| {
            (
                INTERNAL_ERROR,
                format!("could not reach the daemon: {error}"),
            )
        })
    }

    /// The next frame from the daemon.
    async fn next_frame(&mut self) -> Result<Value, (i64, String)> {
        match tokio::time::timeout(FRAME_TIMEOUT, self.frames.recv()).await {
            Ok(Some(frame)) => Ok(frame),
            Ok(None) => Err((
                INTERNAL_ERROR,
                "the daemon closed the connection".to_string(),
            )),
            Err(_) => Err((
                INTERNAL_ERROR,
                "the daemon sent nothing for ten minutes".to_string(),
            )),
        }
    }
}

/// Connect and authenticate against the running daemon.
async fn open_daemon_connection() -> Result<Conn, (i64, String)> {
    use futures::StreamExt;

    let state = crate::daemon::read_daemon_state().ok_or_else(|| {
        (
            INTERNAL_ERROR,
            "no daemon is running; start one with `codape serve`".to_string(),
        )
    })?;
    if state.address.is_empty() {
        return Err((
            INTERNAL_ERROR,
            "the running daemon recorded no address; restart it with this build".to_string(),
        ));
    }
    if !crate::daemon::is_process_running(state.pid) {
        return Err((
            INTERNAL_ERROR,
            format!(
                "the daemon file names PID {} but that process is not running; start one with \
                 `codape serve`",
                state.pid
            ),
        ));
    }
    // A daemon started on loopback with no key list mints its own token and
    // records it; one started with an explicit list records nothing, because
    // that list is the user's, not the daemon's to copy onto disk. The same
    // variable is usually in reach for the editor's shell.
    let token = state.token.clone().or_else(env_token);
    let Some(token) = token else {
        return Err((
            INTERNAL_ERROR,
            format!(
                "the daemon at {} recorded no token. Start it on loopback with this build, or \
                 export {} in the shell that starts the editor.",
                state.websocket_url(),
                codape_server::SERVICE_TOKENS_ENV
            ),
        ));
    };

    let url = state.websocket_url();
    let (stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|error| {
            (
                INTERNAL_ERROR,
                format!("could not connect to {url}: {error}"),
            )
        })?;
    let (mut sink, mut source) = stream.split();

    {
        use futures::SinkExt;
        let auth = json!({"token": token, "role": "remote"});
        sink.send(tokio_tungstenite::tungstenite::Message::Text(
            auth.to_string().into(),
        ))
        .await
        .map_err(|error| (INTERNAL_ERROR, format!("could not authenticate: {error}")))?;
    }

    // The handshake answers `{"session_id":...}`, then a `session_ready`
    // event, or `{"error":...}`. All of it arrives on the same stream, so it is
    // read here rather than left for the first tool call to trip over.
    for _ in 0..2 {
        let message = tokio::time::timeout(FRAME_TIMEOUT, source.next())
            .await
            .map_err(|_| {
                (
                    INTERNAL_ERROR,
                    "the daemon did not answer the handshake".to_string(),
                )
            })?
            .ok_or_else(|| {
                (
                    INTERNAL_ERROR,
                    "the daemon closed the connection during the handshake".to_string(),
                )
            })?
            .map_err(|error| (INTERNAL_ERROR, format!("handshake failed: {error}")))?;
        let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
            continue;
        };
        let frame: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if let Some(error) = frame.get("error").and_then(Value::as_str) {
            return Err((
                INTERNAL_ERROR,
                format!(
                    "the daemon refused the bridge: {error}. Restart it with this build on \
                     loopback, or export {}.",
                    codape_server::SERVICE_TOKENS_ENV
                ),
            ));
        }
    }

    let (tx, frames) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(Ok(message)) = source.next().await {
            let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
                continue;
            };
            if let Ok(frame) = serde_json::from_str::<Value>(&text)
                && tx.send(frame).is_err()
            {
                break;
            }
        }
    });

    Ok(Conn { sink, frames })
}

/// The first token in [`codape_server::SERVICE_TOKENS_ENV`], when it is set.
fn env_token() -> Option<String> {
    std::env::var(codape_server::SERVICE_TOKENS_ENV)
        .ok()
        .and_then(|list| {
            list.split(',')
                .map(str::trim)
                .find(|entry| !entry.is_empty())
                .map(str::to_string)
        })
}

/// A string field of a frame, or the empty string.
fn text_of(frame: &Value, key: &str) -> String {
    frame
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// How much of a tool's first line a transcript entry keeps.
const FIRST_LINE_LIMIT: usize = 200;

/// The first line of a tool's output, so a transcript entry stays one line.
fn first_line(output: &str) -> String {
    let line = output.lines().next().unwrap_or("").trim();
    if line.len() <= FIRST_LINE_LIMIT {
        return line.to_string();
    }
    // A byte offset is not a character boundary: cutting at the limit would
    // panic on a line of multi-byte characters.
    let cut = line
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= FIRST_LINE_LIMIT)
        .last()
        .unwrap_or(0);
    format!("{}…", &line[..cut])
}

/// The tools an editor's model sees.
fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "ask",
            "description": "Send a prompt to the Codape session running in this workspace and \
                return its reply, including the tools it ran. Codape has the filesystem, the \
                shell, and this project's working directory, so prefer it over answering a \
                question about the code yourself. If the turn wants to run a tool that needs \
                approval, this returns the prompt instead of an answer and the turn stays \
                open: call answer_permission with the request id it reports.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "What to ask, or what to have Codape do, in natural \
                            language."
                    }
                },
                "required": ["prompt"]
            }
        }),
        json!({
            "name": "answer_permission",
            "description": "Answer the permission prompt a paused turn is waiting on, then \
                return the rest of the turn. Only valid while `ask` or a previous \
                answer_permission reported a paused turn.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "request_id": {
                        "type": "string",
                        "description": "The request id the paused turn reported."
                    },
                    "decision": {
                        "type": "string",
                        "enum": ["approved", "approve_session", "denied"],
                        "description": "approved allows this one call; approve_session allows \
                            that tool for the rest of the session; denied refuses it."
                    }
                },
                "required": ["request_id", "decision"]
            }
        }),
        json!({
            "name": "cancel",
            "description": "Interrupt the turn that is currently running.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "status",
            "description": "Report whether a Codape daemon is running, where it listens, and the \
                configuration that registers this bridge.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_config_names_this_bridge() {
        let config = editor_config();
        let server = &config["mcpServers"][SERVER_NAME];
        assert_eq!(server["args"][0], "ide");
        assert_eq!(server["args"][1], "bridge");
        assert!(
            server["command"]
                .as_str()
                .is_some_and(|path| !path.is_empty()),
            "the command must be usable"
        );
    }

    #[test]
    fn every_tool_has_a_description_and_a_schema() {
        for tool in tool_definitions() {
            assert!(tool["name"].as_str().is_some_and(|name| !name.is_empty()));
            assert!(
                tool["description"]
                    .as_str()
                    .is_some_and(|text| text.len() > 20),
                "{} needs a real description",
                tool["name"]
            );
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn a_long_first_line_is_truncated_on_a_character_boundary() {
        // Every character is two bytes, so a byte-offset cut at 200 would land
        // inside one and panic.
        let output = "é".repeat(300);
        let line = first_line(&output);
        assert!(line.len() <= 204, "{} bytes", line.len());
        assert!(line.ends_with('…'));
    }

    #[test]
    fn a_short_line_is_returned_intact() {
        assert_eq!(first_line("hello\nsecond"), "hello");
    }

    #[tokio::test]
    async fn an_unknown_tool_is_refused_not_ignored() {
        let mut bridge = Bridge::default();
        let error = bridge
            .call_tool(Some(&json!({"name": "nope", "arguments": {}})))
            .await
            .unwrap_err();
        assert_eq!(error.0, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn a_prompt_is_required() {
        let mut bridge = Bridge::default();
        let error = bridge
            .call_tool(Some(&json!({"name": "ask", "arguments": {"prompt": "  "}})))
            .await
            .unwrap_err();
        assert_eq!(error.0, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn answering_without_a_paused_turn_is_refused() {
        let mut bridge = Bridge::default();
        let error = bridge
            .call_tool(Some(&json!({
                "name": "answer_permission",
                "arguments": {"request_id": "r1", "decision": "approved"}
            })))
            .await
            .unwrap_err();
        assert_eq!(error.0, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn a_decision_outside_the_three_choices_is_refused() {
        let mut bridge = Bridge::default();
        let error = bridge
            .call_tool(Some(&json!({
                "name": "answer_permission",
                "arguments": {"request_id": "r1", "decision": "yes"}
            })))
            .await
            .unwrap_err();
        assert_eq!(error.0, INVALID_PARAMS);
    }

    #[tokio::test]
    async fn initialize_advertises_tools() {
        let mut bridge = Bridge::default();
        let result = bridge
            .dispatch("initialize", None)
            .await
            .expect("initialize answers");
        assert_eq!(result["capabilities"]["tools"], json!({}));
    }

    #[tokio::test]
    async fn an_unimplemented_method_is_refused() {
        let mut bridge = Bridge::default();
        let error = bridge.dispatch("prompts/list", None).await.unwrap_err();
        assert_eq!(error.0, METHOD_NOT_FOUND);
    }
}
