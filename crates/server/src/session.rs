//! Detached WebSocket session management with authentication, rate limiting,
//! and idle cleanup.

use codape_commands::registry::{CommandRegistry, register_all_commands};
use codape_core::engine::{QueryEngine, SubmitOptions};
use codape_core::types::EngineEvent;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::filter::{CommandFilter, SafetyLevel};

// ─── Structured JSON Protocol Types ──────────────────────────────────────

/// Client → Server request message.
#[derive(Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct WsRequest {
    /// Unique request ID for correlation.
    pub id: String,
    /// Request type.
    #[serde(flatten)]
    pub kind: WsRequestKind,
}

/// Type of client request.
#[derive(Debug, Serialize, Deserialize)]
#[allow(missing_docs)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsRequestKind {
    /// Send a query/prompt to the assistant.
    Query { content: String },
    /// Interrupt the current query.
    Interrupt,
    /// Ping to keep the connection alive.
    Ping,
    /// Resolve a command the session wants to run.
    ///
    /// `line` is what the client typed: a slash command when it starts with
    /// `/`, a shell command otherwise. The server answers whether the
    /// session's role may run it at all — a command outside the role's
    /// whitelist never resolves.
    Command {
        /// The command line, as typed.
        line: String,
    },
    /// Answer a `permission_request` the server sent.
    ///
    /// `id` is the request id the prompt carried. Until it arrives the
    /// engine is waiting: a client that never answers leaves the turn
    /// paused, and one that disconnects has its pending prompts denied.
    PermissionResponse {
        /// What the person decided.
        decision: PermissionDecision,
    },
    /// Answer an `elicitation_request` the server sent.
    ///
    /// Answers in the engine's own terms: a label chosen from the offered
    /// options, or free text.
    ElicitationResponse {
        /// The chosen label, or the text the person typed.
        response: String,
    },
}

/// A client's answer to a permission prompt.
///
/// Serialized as the three decisions a person can make, not as a boolean:
/// "yes, and stop asking for this tool" is a different answer from "yes,
/// once", and flattening them loses the one the person chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Allow this one call.
    Approved,
    /// Allow this tool for the rest of the session.
    ApproveSession,
    /// Refuse the call.
    Denied,
}

/// Server → Client response message.
#[derive(Debug, Serialize)]
#[allow(missing_docs)]
pub struct WsResponse {
    /// Correlates to the request ID.
    pub id: String,
    /// Response payload.
    #[serde(flatten)]
    pub kind: WsResponseKind,
}

/// Type of server response.
#[derive(Debug, Serialize)]
#[allow(missing_docs)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsResponseKind {
    /// Successful query response.
    QueryOk { content: String },
    /// Streaming chunk from a query in progress.
    StreamChunk { chunk: String },
    /// Query completed, and why it ended.
    QueryDone {
        /// How the turn finished: `completed`, `token_limit_stop`, and so on.
        /// A truncated or interrupted answer is not the same as a finished one.
        reason: String,
    },
    /// Ping response.
    Pong,
    /// The command resolved, and may run.
    CommandResolved {
        /// The command, as it was resolved.
        command: String,
        /// What the command does.
        description: String,
    },
    /// A tool call started, during the query that requested it.
    ///
    /// Client-side this is what turns a stream of text into a transcript: a
    /// client that only sees `stream_chunk` cannot show that the model ran a
    /// command, only that it said something.
    ToolCallStart {
        /// The tool call identifier, for correlating with the end frame.
        call_id: String,
        /// Tool name.
        name: String,
        /// One-line, safe description of the activity.
        summary: String,
    },
    /// A tool call finished, during the query that requested it.
    ToolCallEnd {
        /// The tool call identifier.
        call_id: String,
        /// The tool's output, and whether it failed.
        result: codape_core::wire_protocol::WireToolResult,
    },
    /// The engine needs a person to allow or refuse a tool call.
    ///
    /// The turn is paused until `permission_response` arrives with this
    /// `request_id`. A client that cannot prompt should answer `denied`
    /// rather than staying silent: silence holds the turn open.
    PermissionRequest {
        /// The id to answer with.
        request_id: String,
        /// Tool name.
        tool: String,
        /// The tool's arguments, so the client can show what is being asked.
        args: serde_json::Value,
        /// Why the permission engine asked instead of deciding.
        reason: String,
        /// Whether "allow for the session" is an option.
        allow_session: bool,
    },
    /// The engine is asking the person to choose or type an answer.
    ///
    /// This frame is not produced yet: a served session installs no
    /// elicitation handler, so the engine cancels the question instead of
    /// reading the daemon's stdin. It is listed here so the protocol does
    /// not grow a new shape when a client actually gets to answer one.
    ElicitationRequest {
        /// The id to answer with.
        request_id: String,
        /// The question.
        message: String,
    },
    /// An advisory message from the engine (degradation warnings, and the like).
    Notification {
        /// The message.
        message: String,
    },
    /// A `permission_response` was delivered to the engine.
    ///
    /// Delivered, not obeyed: the engine is still deciding what the call
    /// becomes. This frame says the answer reached the prompt it named, so a
    /// client can tell a mis-addressed answer from an accepted one.
    PermissionRecorded {
        /// What was recorded.
        decision: PermissionDecision,
    },
    /// Error response.
    Error { code: String, message: String },
}

/// Server → Client unsolicited event (not correlated to a request).
#[derive(Debug, Serialize)]
#[allow(missing_docs)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WsEvent {
    /// Session is ready to receive messages.
    SessionReady { session_id: String },
    /// Warning that the session is approaching idle timeout.
    IdleWarning { message: String },
    /// Session has been closed.
    SessionClosed { reason: String },
    /// Rate limit exceeded.
    RateLimited,
}

impl WsResponse {
    fn error(id: &str, code: &str, message: &str) -> Self {
        Self {
            id: id.to_string(),
            kind: WsResponseKind::Error {
                code: code.to_string(),
                message: message.to_string(),
            },
        }
    }

    fn pong(id: &str) -> Self {
        Self {
            id: id.to_string(),
            kind: WsResponseKind::Pong,
        }
    }
}

/// The frame an engine event becomes, when it becomes one.
///
/// A `None` is an event with no client-facing shape: the tool-use summary is
/// a renderer hint that rides in front of a `ToolCallStart` carrying the same
/// text, and the remaining engine events are internal bookkeeping. Dropping
/// them here rather than downstream keeps the pump's `select!` loop from
/// growing a match arm per event it never sends.
///
/// `PermissionRequest` is deliberately not here. A session with a
/// confirmation channel never reaches the scheduler's `Ask` branch — the
/// permission engine consumes the decision through the bus before the
/// scheduler sees it — so the event is only ever emitted by a session that
/// *cannot* answer prompts, and forwarding it would offer a client a prompt
/// whose answer nothing is listening for. The bus is where prompts come
/// from; see [`permission_frame`].
fn query_frame(id: &str, event: EngineEvent) -> Option<String> {
    let kind = match event {
        EngineEvent::TextDelta(chunk) => WsResponseKind::StreamChunk { chunk },
        EngineEvent::ToolCallStart {
            id: call_id,
            name,
            summary,
            ..
        } => WsResponseKind::ToolCallStart {
            call_id,
            name,
            summary,
        },
        EngineEvent::ToolCallEnd {
            id: call_id,
            result,
        } => WsResponseKind::ToolCallEnd {
            call_id,
            result: codape_core::wire_protocol::WireToolResult::from(result),
        },
        EngineEvent::ElicitationRequest {
            id: request_id,
            message,
        } => WsResponseKind::ElicitationRequest {
            request_id,
            message,
        },
        EngineEvent::Notification { message } => WsResponseKind::Notification { message },
        // Everything else — compaction bookkeeping, sub-agent events the
        // parent renders, mod UI the terminal owns — has no frame yet.
        _ => return None,
    };
    serde_json::to_string(&WsResponse {
        id: id.to_string(),
        kind,
    })
    .ok()
}

/// The next permission prompt, for a select loop that must also end when the
/// turn does.
///
/// Resolves to `None` when the session has no channel — or when the channel
/// closes, which is what happens when the engine that owns the sending half
/// goes away. Both cases leave the branch able to resolve, so the loop can
/// re-arm it without spinning on a again.
async fn next_prompt(
    confirmations: Option<codape_core::confirmation_bus::ConfirmationBus>,
) -> Option<codape_core::confirmation_bus::ConfirmationRequest> {
    match confirmations {
        Some(bus) => bus.next_request().await,
        None => std::future::pending().await,
    }
}

/// The prompt frame for a request waiting on the confirmation bus.
///
/// The `id` is the query the prompt belongs to, so a client can tie the
/// question to the turn that raised it; `request_id` is what the answer has
/// to name.
fn permission_frame(
    query_id: &str,
    request: &codape_core::confirmation_bus::ConfirmationRequest,
) -> Option<String> {
    let args = serde_json::from_str(&request.arguments).unwrap_or(serde_json::Value::Null);
    serde_json::to_string(&WsResponse {
        id: query_id.to_string(),
        kind: WsResponseKind::PermissionRequest {
            request_id: request.id.clone(),
            tool: request.tool_name.clone(),
            args,
            reason: request.reason.clone(),
            allow_session: request.allow_session,
        },
    })
    .ok()
}

/// The bus decision a client's answer means.
///
/// `PermissionDecision` and `ConfirmationResponse` are the same three
/// choices; the conversion is spelled out so a new choice on either side
/// fails to compile here rather than silently becoming "denied".
const fn confirmation_response(
    decision: PermissionDecision,
) -> codape_core::confirmation_bus::ConfirmationResponse {
    match decision {
        PermissionDecision::Approved => {
            codape_core::confirmation_bus::ConfirmationResponse::Approved
        }
        PermissionDecision::ApproveSession => {
            codape_core::confirmation_bus::ConfirmationResponse::ApproveSession
        }
        PermissionDecision::Denied => codape_core::confirmation_bus::ConfirmationResponse::Denied,
    }
}

/// Token-bucket rate limiter per session.
#[derive(Debug)]
struct TokenBucket {
    /// Maximum tokens.
    capacity: u32,
    /// Current tokens available.
    tokens: f64,
    /// Tokens added per second.
    refill_rate: f64,
    /// Last time tokens were refilled.
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity: u32, refill_rate: f64) -> Self {
        Self {
            capacity,
            tokens: f64::from(capacity),
            refill_rate,
            last_refill: Instant::now(),
        }
    }

    /// Refill tokens based on elapsed time.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = elapsed
            .mul_add(self.refill_rate, self.tokens)
            .min(f64::from(self.capacity));
        self.last_refill = now;
    }

    /// Try to consume one token. Returns false if rate limit exceeded.
    fn try_consume(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// What kind of client opened a session.
///
/// The role decides which commands the session may resolve: a remote client
/// gets the read-only surface, a bridge session gets the limited-write one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    /// A client reaching the daemon over the network.
    #[default]
    Remote,
    /// A bridge session, which may also write inside its working directory.
    Bridge,
}

impl SessionRole {
    /// The command-safety level this role resolves commands at.
    #[must_use]
    pub const fn safety_level(self) -> SafetyLevel {
        match self {
            Self::Remote => SafetyLevel::Remote,
            Self::Bridge => SafetyLevel::Bridge,
        }
    }

    /// The role's name, for messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Bridge => "bridge",
        }
    }
}

/// A connected WebSocket session.
#[derive(Debug, Clone)]
pub struct Session {
    /// The unique session identifier.
    pub id: String,
    /// The API key or token used for authentication.
    pub auth_token: String,
    /// What kind of client opened the session.
    pub role: SessionRole,
    /// When the session was created.
    pub created_at: Instant,
    /// Last activity timestamp.
    pub last_activity: Instant,
    /// Whether the session is authenticated.
    pub authenticated: bool,
}

/// Configuration for the daemon server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Valid API keys for authentication.
    pub valid_keys: Vec<String>,
    /// Serve without any API keys.
    ///
    /// Off by default: with no keys configured, an open server accepts any
    /// token from anyone who can reach the port. Turning this on is an
    /// explicit decision to run unauthenticated, and is meant for a loopback
    /// development session rather than anything reachable from a network.
    pub allow_anonymous: bool,
    /// Duration after which idle sessions are cleaned up.
    pub idle_timeout: Duration,
    /// Maximum number of concurrent sessions.
    pub max_sessions: usize,
    /// Maximum messages per minute per session (rate limit).
    pub rate_limit_per_minute: u32,
    /// Optional port for the HTTP health endpoint (None = disabled).
    pub health_port: Option<u16>,
    /// Optional path to SQLite database for session persistence.
    pub persist_path: Option<std::path::PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            valid_keys: Vec::new(),
            allow_anonymous: false,
            idle_timeout: Duration::from_secs(3600),
            max_sessions: 100,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        }
    }
}

/// Builds the engine a session's queries run on.
///
/// The server cannot build one itself — an engine needs an API client, a
/// permission checker, and a tool dispatcher, and those belong to whatever
/// started the server. The factory is how they are handed over, and it also
/// returns the event stream, because whoever builds the engine is who can read
/// what it emits.
///
/// The confirmation bus travels with them for the same reason: the engine
/// asks *through* it when a call needs a person's decision, and only the
/// owner of the engine can install one. A factory that supplies no bus leaves
/// every `Ask` decision unanswered, and the engine reports those calls as
/// unapproved rather than running them.
pub type EngineFactory = Arc<dyn Fn() -> SessionEngineParts + Send + Sync>;

/// What a factory hands over for one session.
pub struct SessionEngineParts {
    /// The engine, with its tool executor and permission rules installed.
    pub engine: QueryEngine,
    /// The event stream the engine writes to.
    pub events: tokio::sync::mpsc::UnboundedReceiver<EngineEvent>,
    /// Where the engine's permission prompts wait for an answer.
    pub confirmations: Option<codape_core::confirmation_bus::ConfirmationBus>,
}

impl std::fmt::Debug for SessionEngineParts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionEngineParts")
            .field("has_confirmations", &self.confirmations.is_some())
            .finish_non_exhaustive()
    }
}

/// One session's engine and its event receiver.
///
/// Kept for the life of the session so a second query sees the first
/// exchange; a fresh engine per query would silently drop the conversation.
struct SessionEngine {
    engine: QueryEngine,
    events: tokio::sync::mpsc::UnboundedReceiver<EngineEvent>,
}

/// A session's engine, shared with the task running its query.
#[derive(Clone)]
struct SessionEngineSlot {
    engine: Arc<tokio::sync::Mutex<SessionEngine>>,
    interrupt: codape_core::engine::InterruptHandle,
    /// The bus the engine's permission prompts arrive on, cloned out so a
    /// `permission_response` can be delivered without taking the engine lock.
    ///
    /// Taking that lock from the connection loop would deadlock every
    /// answer: the query task holds it for the whole turn, and the turn is
    /// exactly what is waiting for the answer.
    confirmations: Option<codape_core::confirmation_bus::ConfirmationBus>,
}

impl std::fmt::Debug for SessionEngineSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SessionEngineSlot")
    }
}

/// The state every connection shares.
///
/// Behind a synchronous lock on purpose: it is only ever held for the short
/// bookkeeping a message needs, never across an `await`, and a guard that is
/// not `Send` makes the compiler enforce that in the tasks that serve
/// connections.
#[derive(Default)]
struct Shared {
    /// Active sessions.
    sessions: HashMap<String, Session>,
    /// Engines kept alive for the life of a session, keyed by session id.
    engines: HashMap<String, SessionEngineSlot>,
    /// Optional SQLite session store for persistence.
    store: Option<crate::persistence::SessionStore>,
    /// Monotonic source of session ids.
    next_session_id: u64,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Shared")
            .field("sessions", &self.sessions.len())
            .field("engines", &self.engines.len())
            .field("has_store", &self.store.is_some())
            .field("next_session_id", &self.next_session_id)
            .finish()
    }
}

/// The numeric part of a `sess_N` id, if it has one.
///
/// Restored rows are the only ids the process did not generate itself; a row
/// with a spelling this build does not recognise is skipped rather than
/// guessed at.
fn session_id_number(id: &str) -> Option<u64> {
    id.strip_prefix("sess_")?.parse().ok()
}

impl Shared {
    /// Open a session for a token that is allowed to have one.
    fn authenticate(
        &mut self,
        token: &str,
        role: SessionRole,
        config: &ServerConfig,
    ) -> Result<String, String> {
        if config.valid_keys.is_empty() {
            if !config.allow_anonymous {
                return Err(
                    "server has no API keys configured; set one, or enable anonymous \
                     access explicitly for local development"
                        .to_string(),
                );
            }
        } else if !config.valid_keys.iter().any(|k| k == token) {
            return Err("invalid API key".to_string());
        }
        if self.sessions.len() >= config.max_sessions {
            return Err("max sessions reached".to_string());
        }

        // A counter, not `sessions.len()`: removing a session lowers the
        // length, so the next id would repeat one still in the map and
        // overwrite it.
        self.next_session_id += 1;
        let id = format!("sess_{}", self.next_session_id);
        let now = Instant::now();
        let session = Session {
            id: id.clone(),
            auth_token: token.to_string(),
            role,
            created_at: now,
            last_activity: now,
            authenticated: true,
        };
        self.sessions.insert(id.clone(), session);

        // Persist to SQLite if configured. What reaches the file is a
        // fingerprint of the key, not the key: the row only has to say which
        // key opened the session.
        if let Some(ref mut store) = self.store {
            let session = self.sessions.get(&id).expect("just inserted");
            let fingerprint = store.fingerprint(&session.auth_token);
            let ps = crate::persistence::persist_session(session, fingerprint);
            if let Err(e) = store.save(&ps) {
                warn!("Failed to persist session {id}: {e}");
            }
        }

        Ok(id)
    }
}

impl Shared {
    /// Note that a session was used, so idle cleanup leaves it alone.
    fn record_activity(&mut self, session_id: &str) -> bool {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.last_activity = Instant::now();
            true
        } else {
            false
        }
    }

    /// Drop the sessions that have been idle past the timeout.
    fn cleanup_idle(&mut self, timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        let mut removed = Vec::new();

        self.sessions.retain(|id, session| {
            if now.duration_since(session.last_activity) > timeout {
                removed.push(id.clone());
                false
            } else {
                true
            }
        });
        for id in &removed {
            self.engines.remove(id);
        }
        removed
    }

    /// End a session, in memory and in the store.
    fn disconnect(&mut self, session_id: &str) -> bool {
        self.engines.remove(session_id);
        let removed = self.sessions.remove(session_id).is_some();
        if removed
            && let Some(ref mut store) = self.store
            && let Err(e) = store.remove(session_id)
        {
            warn!("Failed to remove session {session_id} from store: {e}");
        }
        removed
    }
}

/// One WebSocket connection, with what it shares and what it owns.
struct Connection {
    config: ServerConfig,
    shared: Arc<parking_lot::Mutex<Shared>>,
    session_count_atomic: Arc<AtomicUsize>,
    engine_factory: Option<EngineFactory>,
    /// Every slash command this build knows about, for resolving what a
    /// remote or bridge session may run.
    commands: Arc<CommandRegistry>,
}

impl Connection {
    fn authenticate(&self, token: &str, role: SessionRole) -> Result<String, String> {
        let id = self.shared.lock().authenticate(token, role, &self.config)?;
        self.sync_session_count();
        Ok(id)
    }

    /// The role the session opened with, or the default when it is unknown.
    fn session_role(&self, session_id: &str) -> SessionRole {
        self.shared
            .lock()
            .sessions
            .get(session_id)
            .map_or_else(SessionRole::default, |session| session.role)
    }

    /// Resolve a command a remote or bridge session asked for.
    ///
    /// This is where the whitelists are enforced. A slash command has to be in
    /// the registry's availability matrix for the session's role; anything
    /// else is a shell command and has to pass the role's safety filter. A
    /// command that does not is never resolved, and the caller is told so.
    fn resolve_command(
        &self,
        role: SessionRole,
        line: &str,
    ) -> Result<(String, String), (String, String)> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err(("empty_command".to_string(), "nothing to run".to_string()));
        }
        let level = role.safety_level();

        if let Some(rest) = trimmed.strip_prefix('/') {
            let name = rest.split_whitespace().next().unwrap_or("");
            let available = self.commands.available_commands(
                true,
                level == SafetyLevel::Remote,
                level == SafetyLevel::Bridge,
            );
            return available
                .iter()
                .find(|command| command.name == name)
                .map_or_else(
                    || {
                        Err((
                            "command_not_allowed".to_string(),
                            format!("/{name} is not available to a {} session", role.name()),
                        ))
                    },
                    |command| Ok((command.name.to_string(), command.description.to_string())),
                );
        }

        let allowed = match level {
            SafetyLevel::Remote => CommandFilter::is_remote_safe(trimmed),
            SafetyLevel::Bridge => CommandFilter::is_bridge_safe(trimmed),
        };
        if allowed {
            Ok((
                trimmed.to_string(),
                "allowed by the command whitelist".to_string(),
            ))
        } else {
            Err((
                "command_not_allowed".to_string(),
                format!(
                    "`{trimmed}` is not on the {} command whitelist",
                    role.name()
                ),
            ))
        }
    }

    fn record_activity(&self, session_id: &str) -> bool {
        self.shared.lock().record_activity(session_id)
    }

    fn disconnect(&self, session_id: &str) -> bool {
        let removed = self.shared.lock().disconnect(session_id);
        if removed {
            self.sync_session_count();
        }
        removed
    }

    fn session_count(&self) -> usize {
        self.shared.lock().sessions.len()
    }

    fn sync_session_count(&self) {
        self.session_count_atomic
            .store(self.shared.lock().sessions.len(), Ordering::Relaxed);
    }

    async fn serve(&self, stream: TcpStream) {
        let ws_result = tokio_tungstenite::accept_async(stream).await;
        let mut ws = match ws_result {
            Ok(ws) => ws,
            Err(e) => {
                warn!("WebSocket handshake failed: {e}");
                return;
            }
        };

        // First message must be auth token
        let auth_msg = match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => t,
            Ok(Some(Ok(other))) => {
                warn!("expected auth text, got: {other:?}");
                let _ = ws
                    .send(Message::Text(
                        r#"{"error":"auth required"}"#.to_string().into(),
                    ))
                    .await;
                return;
            }
            Ok(Some(Err(e))) => {
                warn!("WebSocket error during auth: {e}");
                return;
            }
            Ok(None) => {
                debug!("client disconnected before auth");
                return;
            }
            Err(_) => {
                warn!("auth timeout");
                let _ = ws
                    .send(Message::Text(
                        r#"{"error":"auth timeout"}"#.to_string().into(),
                    ))
                    .await;
                return;
            }
        };

        // Parse auth JSON: {"token":"...", "role":"bridge"}
        let (token, role) = serde_json::from_str::<serde_json::Value>(&auth_msg).map_or_else(
            |_| (auth_msg.to_string(), SessionRole::default()), // fallback: raw token
            |v| {
                (
                    v.get("token")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                    v.get("role")
                        .and_then(|r| serde_json::from_value::<SessionRole>(r.clone()).ok())
                        .unwrap_or_default(),
                )
            },
        );

        let session_id = match self.authenticate(&token, role) {
            Ok(id) => id,
            Err(e) => {
                warn!("authentication failed: {e}");
                let _ = ws
                    .send(Message::Text(format!(r#"{{"error":"{e}"}}"#).into()))
                    .await;
                return;
            }
        };

        let _ = ws
            .send(Message::Text(
                format!(r#"{{"session_id":"{session_id}"}}"#).into(),
            ))
            .await;

        info!(
            "session {session_id} authenticated, {} active",
            self.session_count()
        );

        // Per-connection rate limiter
        let refill_rate = f64::from(self.config.rate_limit_per_minute) / 60.0;
        let mut limiter = TokenBucket::new(self.config.rate_limit_per_minute, refill_rate);

        // Send session-ready event
        let ready = WsEvent::SessionReady {
            session_id: session_id.clone(),
        };
        let _ = ws
            .send(Message::Text(serde_json::to_string(&ready).unwrap().into()))
            .await;

        // Message loop with structured JSON protocol.
        //
        // Outgoing frames go through a channel so a query running in its own
        // task can stream while this loop keeps reading: the socket has one
        // writer, and it is here.
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        // The queries this connection has running, so Ctrl+C and a disconnect
        // can reach them. A session belongs to one connection, so this is
        // per-connection state rather than shared.
        let mut running_queries: HashMap<String, codape_core::engine::InterruptHandle> =
            HashMap::new();

        loop {
            tokio::select! {
                // A frame from a query running in its own task.
                Some(frame) = out_rx.recv() => {
                    if let Err(e) = ws.send(Message::Text(frame.into())).await {
                        warn!("send error for session {session_id}: {e}");
                        break;
                    }
                }
                incoming = tokio::time::timeout(Duration::from_secs(300), ws.next()) => {
                    match incoming {
                        Ok(Some(Ok(Message::Text(text)))) => {
                            // Rate limit check
                            if !limiter.try_consume() {
                                let evt = WsEvent::RateLimited;
                                let _ = ws
                                    .send(Message::Text(
                                        serde_json::to_string(&evt).unwrap().into(),
                                    ))
                                    .await;
                                continue;
                            }
                            self.record_activity(&session_id);
                            debug!("session {session_id} received: {text}");

                            // A query answers through the channel as it runs;
                            // everything else answers here.
                            let Some(response) =
                                self.handle_message(&session_id, &text, &out_tx, &mut running_queries)
                            else {
                                continue;
                            };
                            let json = serde_json::to_string(&response).unwrap();
                            if let Err(e) = ws.send(Message::Text(json.into())).await {
                                warn!("send error for session {session_id}: {e}");
                                break;
                            }
                        }
                        Ok(Some(Ok(Message::Close(_)))) => {
                            debug!("session {session_id} closed connection");
                            break;
                        }
                        Ok(Some(Ok(Message::Ping(data)))) => {
                            if let Err(e) = ws.send(Message::Pong(data)).await {
                                warn!("pong error for session {session_id}: {e}");
                                break;
                            }
                        }
                        Ok(Some(Err(e))) => {
                            warn!("WebSocket error for session {session_id}: {e}");
                            break;
                        }
                        Ok(None) => {
                            debug!("session {session_id} stream ended");
                            break;
                        }
                        Err(_) => {
                            warn!("session {session_id} message timeout");
                            break;
                        }
                        _ => {
                            // Ignore Binary, Pong, Frame, and other message types
                        }
                    }
                }
            }
        }

        // A query still running when the connection goes away is stopped:
        // nothing is listening for its answer, and it would keep using the
        // account and the filesystem otherwise.
        if let Some(handle) = running_queries.remove(&session_id) {
            handle.interrupt();
        }
        // A prompt still waiting for an answer is abandoned, not denied: the
        // person did not say no, nobody is left to ask. Without this the
        // engine waits on the bus forever and the turn never ends.
        //
        // The bus is cloned out before the await: the guard is not `Send`, and
        // holding it across `abandon_all` would make the whole connection
        // future unspawnable.
        let waiting_bus = {
            let shared = self.shared.lock();
            shared
                .engines
                .get(&session_id)
                .and_then(|slot| slot.confirmations.clone())
        };
        if let Some(bus) = waiting_bus {
            let abandoned = bus.abandon_all().await;
            if abandoned > 0 {
                debug!("session {session_id} disconnected with {abandoned} unanswered prompt(s)");
            }
        }

        self.disconnect(&session_id);
        info!(
            "session {session_id} disconnected, {} active",
            self.session_count()
        );
    }

    /// Route a structured JSON message to the appropriate handler.
    ///
    /// A query does not return a response: it starts a task that streams the
    /// answer over `out`. Everything else answers immediately.
    fn handle_message(
        &self,
        session_id: &str,
        text: &str,
        out: &tokio::sync::mpsc::UnboundedSender<String>,
        running_queries: &mut HashMap<String, codape_core::engine::InterruptHandle>,
    ) -> Option<WsResponse> {
        let req: WsRequest = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(e) => {
                return Some(WsResponse::error(
                    "unknown",
                    "parse_error",
                    &format!("invalid JSON request: {e}"),
                ));
            }
        };

        match req.kind {
            WsRequestKind::Query { content } => {
                debug!(
                    "query from session {session_id}: {}",
                    content.chars().take(80).collect::<String>()
                );

                let Some(factory) = self.engine_factory.clone() else {
                    return Some(WsResponse::error(
                        &req.id,
                        "no_engine",
                        "this server was started without a query engine, so there is nothing to \
                         run the query on",
                    ));
                };

                // One engine per session: a fresh engine for every query
                // would drop the conversation between messages. The engine is
                // kept behind an async lock, which also serialises queries on
                // one session (a turn needs `&mut`).
                let slot = {
                    let mut shared = self.shared.lock();
                    if let Some(slot) = shared.engines.get(session_id) {
                        slot.clone()
                    } else {
                        let SessionEngineParts {
                            engine,
                            events,
                            confirmations,
                        } = factory();
                        let slot = SessionEngineSlot {
                            interrupt: engine.interrupt_handle(),
                            engine: Arc::new(tokio::sync::Mutex::new(SessionEngine {
                                engine,
                                events,
                            })),
                            confirmations,
                        };
                        shared.engines.insert(session_id.to_string(), slot.clone());
                        slot
                    }
                };
                running_queries.insert(session_id.to_string(), slot.interrupt.clone());

                let id = req.id.clone();
                let out = out.clone();
                // Prompts arrive on the bus, not on the event stream: the
                // engine asks through it and waits. They are pumped by the
                // same task that pumps the event stream — one writer, so a
                // prompt cannot overtake the `tool_call_start` it belongs to.
                let confirmations = slot.confirmations.clone();
                tokio::spawn(async move {
                    let mut guard = slot.engine.lock().await;
                    let SessionEngine { engine, events } = &mut *guard;

                    // Pump the answer while the turn runs, in this task, so
                    // the receiver stays with the engine for the next query.
                    //
                    // Every event the engine emits goes out, not just text:
                    // a client that only sees `stream_chunk` can show an
                    // answer, but not the command that produced it.
                    let result = {
                        let submit = engine.submit_streaming(content, SubmitOptions::default());
                        tokio::pin!(submit);
                        // Re-armed after each prompt: a pinned future that has
                        // already resolved must not be polled again.
                        let mut prompts = Box::pin(next_prompt(confirmations.clone()));
                        loop {
                            tokio::select! {
                                // Events first: a prompt that is emitted
                                // after a tool call must not overtake it.
                                biased;
                                Some(event) = events.recv() => {
                                    if let Some(frame) = query_frame(&id, event)
                                        && out.send(frame).is_err() {
                                        // The client is gone; the connection
                                        // loop will interrupt the turn on
                                        // disconnect.
                                        break Err("client disconnected".to_string());
                                    }
                                }
                                request = &mut prompts => {
                                    if let Some(request) = request
                                        && let Some(frame) = permission_frame(&id, &request)
                                        && out.send(frame).is_err() {
                                        break Err("client disconnected".to_string());
                                    }
                                    prompts = Box::pin(next_prompt(confirmations.clone()));
                                }
                                result = &mut submit => break result,
                            }
                        }
                    };

                    // Forward frames that were queued after the turn future
                    // completed, then release the engine for the next query.
                    while let Ok(event) = events.try_recv() {
                        if let Some(frame) = query_frame(&id, event) {
                            let _ = out.send(frame);
                        }
                    }
                    drop(guard);

                    let frame = match result {
                        Ok(reason) => WsResponse {
                            id,
                            kind: WsResponseKind::QueryDone {
                                reason: reason.to_string(),
                            },
                        },
                        Err(error) => WsResponse::error(&id, "query_failed", &error),
                    };
                    if let Ok(json) = serde_json::to_string(&frame) {
                        let _ = out.send(json);
                    }
                    // The handle stays until the session ends: `Interrupt`
                    // needs to find it while the query is in flight.
                });

                None
            }
            WsRequestKind::Interrupt => {
                let interrupted = running_queries.get(session_id).is_some_and(|handle| {
                    handle.interrupt();
                    true
                });
                Some(WsResponse {
                    id: req.id,
                    kind: WsResponseKind::QueryDone {
                        reason: if interrupted {
                            "interrupted".to_string()
                        } else {
                            "nothing running".to_string()
                        },
                    },
                })
            }
            WsRequestKind::Ping => Some(WsResponse::pong(&req.id)),
            WsRequestKind::Command { line } => {
                let role = self.session_role(session_id);
                match self.resolve_command(role, &line) {
                    Ok((command, description)) => Some(WsResponse {
                        id: req.id,
                        kind: WsResponseKind::CommandResolved {
                            command,
                            description,
                        },
                    }),
                    Err((code, message)) => Some(WsResponse::error(&req.id, &code, &message)),
                }
            }
            WsRequestKind::PermissionResponse { decision } => {
                let bus = self
                    .shared
                    .lock()
                    .engines
                    .get(session_id)
                    .and_then(|slot| slot.confirmations.clone());
                let Some(bus) = bus else {
                    return Some(WsResponse::error(
                        &req.id,
                        "no_pending_request",
                        "this session has no confirmation channel, so nothing is waiting for an \
                         answer",
                    ));
                };
                // Delivered without taking the engine lock: the query task
                // holds it for the whole turn, and that turn is what is
                // waiting for this answer. The send happens on its own task
                // for the same reason at the next level up — the bus is
                // async, and this handler is not.
                let id = req.id.clone();
                let out = out.clone();
                tokio::spawn(async move {
                    let delivered = bus.respond(&id, confirmation_response(decision)).await;
                    let frame = if delivered {
                        WsResponse {
                            id,
                            kind: WsResponseKind::PermissionRecorded { decision },
                        }
                    } else {
                        WsResponse::error(
                            &id,
                            "no_pending_request",
                            "no permission prompt is waiting under that id",
                        )
                    };
                    if let Ok(json) = serde_json::to_string(&frame) {
                        let _ = out.send(json);
                    }
                });
                None
            }
            WsRequestKind::ElicitationResponse { response: _ } => Some(WsResponse::error(
                &req.id,
                "unsupported",
                "a served session installs no elicitation handler, so no elicitation is waiting \
                 for an answer",
            )),
        }
    }
}
/// Server for managing detached sessions over WebSocket.
pub struct DaemonServer {
    /// Server configuration.
    config: ServerConfig,
    /// Session bookkeeping, shared with the task serving each connection.
    shared: Arc<parking_lot::Mutex<Shared>>,
    /// Thread-safe session count for the health endpoint.
    session_count_atomic: Arc<AtomicUsize>,
    /// How a session's queries reach a model, when one was supplied.
    engine_factory: Option<EngineFactory>,
    /// Called with the address the listener actually bound.
    ///
    /// The port a caller passes can be `0`, and a daemon that answers on a
    /// port nobody wrote down is unreachable. This is how the caller learns
    /// the one that was chosen, before the accept loop starts.
    listening_hook: Option<Arc<dyn Fn(SocketAddr) + Send + Sync>>,
}

impl std::fmt::Debug for DaemonServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonServer")
            .field("sessions", &self.shared.lock().sessions.len())
            .field("has_engine", &self.engine_factory.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for DaemonServer {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonServer {
    /// Supply the engine sessions run their queries on.
    ///
    /// Without one, a query is refused with `no_engine` rather than answered:
    /// a server that cannot run a query has nothing true to say about one.
    pub fn set_engine_factory(&mut self, factory: EngineFactory) {
        self.engine_factory = Some(factory);
    }

    /// Be told the address the listener bound, once it has.
    ///
    /// Called from inside [`run`](Self::run), after the bind and before the
    /// first connection is accepted, so a caller can record the port a client
    /// has to connect to. Nothing else can know it: the configured port may
    /// be `0`.
    pub fn on_listening(&mut self, hook: impl Fn(SocketAddr) + Send + Sync + 'static) {
        self.listening_hook = Some(Arc::new(hook));
    }

    /// Create a new daemon server with default config.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: ServerConfig::default(),
            shared: Arc::new(parking_lot::Mutex::new(Shared::default())),
            session_count_atomic: Arc::new(AtomicUsize::new(0)),
            engine_factory: None,
            listening_hook: None,
        }
    }

    /// Create a new daemon server with the given config.
    #[must_use]
    pub fn with_config(config: ServerConfig) -> Self {
        // Take the path out first so the config itself can move into `server`
        // instead of being cloned.
        let persist_path = config.persist_path.clone();
        let server = Self {
            config,
            shared: Arc::new(parking_lot::Mutex::new(Shared::default())),
            session_count_atomic: Arc::new(AtomicUsize::new(0)),
            engine_factory: None,
            listening_hook: None,
        };

        // Initialize persistence if configured
        if let Some(ref path) = persist_path {
            match crate::persistence::SessionStore::open(path) {
                Ok(store) => {
                    // Restore previous sessions
                    let restored = store.load_all().unwrap_or_default();
                    let mut shared = server.shared.lock();
                    for ps in &restored {
                        let session = crate::persistence::restore_session(ps);
                        shared.sessions.insert(session.id.clone(), session);
                    }
                    // The counter starts at zero in a fresh process, but the
                    // ids it would hand out may already belong to restored
                    // sessions — the first new auth would overwrite `sess_1`.
                    // Resume past the highest id that came back.
                    shared.next_session_id = restored
                        .iter()
                        .filter_map(|ps| session_id_number(&ps.id))
                        .max()
                        .unwrap_or(0);
                    if !restored.is_empty() {
                        info!("Restored {} sessions from database", restored.len());
                    }
                    shared.store = Some(store);
                }
                Err(e) => {
                    warn!("Failed to open session store at {:?}: {}", path, e);
                }
            }
        }

        server.sync_session_count();
        server
    }

    /// Authenticate a new session with the given token.
    ///
    /// The session gets the default role; [`Self::authenticate_as`] names one.
    pub fn authenticate(&mut self, token: &str) -> Result<String, String> {
        self.authenticate_as(token, SessionRole::default())
    }

    /// Authenticate a new session with the given token and role.
    pub fn authenticate_as(&mut self, token: &str, role: SessionRole) -> Result<String, String> {
        let id = self.shared.lock().authenticate(token, role, &self.config)?;
        self.sync_session_count();
        Ok(id)
    }

    /// Record activity for a session (resets idle timer).
    pub fn record_activity(&mut self, session_id: &str) -> bool {
        self.shared.lock().record_activity(session_id)
    }

    /// Remove idle sessions that have exceeded the timeout.
    pub fn cleanup_idle(&mut self) -> Vec<String> {
        let removed = self.shared.lock().cleanup_idle(self.config.idle_timeout);
        self.sync_session_count();
        removed
    }

    /// List all active sessions.
    #[must_use]
    /// The active sessions, as copies.
    ///
    /// Copies rather than references: the bookkeeping is behind a lock now, and
    /// a reference out of it would outlive the guard.
    pub fn list_sessions(&self) -> Vec<Session> {
        self.shared.lock().sessions.values().cloned().collect()
    }

    /// Get a session by ID.
    #[must_use]
    pub fn get_session(&self, session_id: &str) -> Option<Session> {
        self.shared.lock().sessions.get(session_id).cloned()
    }

    /// Disconnect a session.
    pub fn disconnect(&mut self, session_id: &str) -> bool {
        let removed = self.shared.lock().disconnect(session_id);
        if removed {
            self.sync_session_count();
        }
        removed
    }

    /// Number of active sessions.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.session_count_atomic.load(Ordering::Relaxed)
    }

    /// Update the atomic session counter (called after sessions change).
    fn sync_session_count(&self) {
        self.session_count_atomic
            .store(self.shared.lock().sessions.len(), Ordering::Relaxed);
    }

    /// Start the WebSocket server and accept connections.
    ///
    /// Binds a TCP listener on the given address, upgrades connections to
    /// WebSocket, authenticates clients, and routes messages.
    /// If `config.health_port` is set, spawns a lightweight HTTP server
    /// on that port serving a `/health` endpoint.
    pub async fn run(&mut self, addr: SocketAddr) -> Result<(), String> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("failed to bind TCP listener: {e}"))?;

        // The configured address may name port `0`; the listener knows the
        // one it actually took, and that is what a client has to be told.
        let bound = listener
            .local_addr()
            .map_err(|e| format!("bound listener has no address: {e}"))?;
        info!("WebSocket server listening on ws://{bound}");
        if let Some(hook) = self.listening_hook.clone() {
            hook(bound);
        }

        // Spawn health endpoint if configured
        if let Some(health_port) = self.config.health_port {
            let counter = Arc::clone(&self.session_count_atomic);
            tokio::spawn(async move {
                run_health_server(health_port, counter).await;
            });
        }

        let mut cleanup_interval = tokio::time::interval(Duration::from_secs(60));

        // Built once: the registry is the availability matrix a session's
        // command resolution is checked against.
        let commands = Arc::new({
            let mut registry = CommandRegistry::new();
            register_all_commands(&mut registry);
            registry
        });

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, peer)) => {
                            debug!("new TCP connection from {peer}");
                            // Served in its own task. Awaiting the connection
                            // here meant one client held the accept loop for as
                            // long as it stayed connected: no second client
                            // could connect, and the idle sweep above never
                            // ran.
                            let connection = Connection {
                                config: self.config.clone(),
                                shared: Arc::clone(&self.shared),
                                session_count_atomic: Arc::clone(&self.session_count_atomic),
                                engine_factory: self.engine_factory.clone(),
                                commands: Arc::clone(&commands),
                            };
                            tokio::spawn(async move {
                                connection.serve(stream).await;
                            });
                        }
                        Err(e) => {
                            warn!("accept error: {e}");
                        }
                    }
                }
                _ = cleanup_interval.tick() => {
                    let removed = self.cleanup_idle();
                    if !removed.is_empty() {
                        info!("cleaned up {} idle sessions: {:?}", removed.len(), removed);
                    }
                }
            }
        }
    }
}

/// Lightweight HTTP server serving a single `/health` endpoint.
async fn run_health_server(port: u16, session_count: Arc<AtomicUsize>) {
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!("failed to bind health endpoint on port {port}: {e}");
            return;
        }
    };
    info!("Health endpoint listening on http://0.0.0.0:{port}/health");

    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let counter = Arc::clone(&session_count);
        tokio::spawn(async move {
            let _ = handle_health_request(stream, counter).await;
        });
    }
}

async fn handle_health_request(
    stream: TcpStream,
    session_count: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut reader, mut writer) = stream.into_split();

    // Read the HTTP request (just need the first line)
    let mut buf = [0u8; 1024];
    let n = reader.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or("");

    let (status, body) = if first_line.starts_with("GET /health") {
        (
            200,
            serde_json::json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "sessions": session_count.load(Ordering::Relaxed),
            }),
        )
    } else {
        (404, serde_json::json!({ "error": "not found" }))
    };

    let body_str = serde_json::to_string(&body).unwrap();
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_str}",
        if status == 200 { "OK" } else { "Not Found" },
        body_str.len(),
    );

    writer.write_all(response.as_bytes()).await?;
    writer.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use codape_commands::CommandContext;
    use codape_commands::registry::{CommandResult, SlashCommand};

    fn server_with_keys() -> DaemonServer {
        DaemonServer::with_config(ServerConfig {
            valid_keys: vec!["key1".to_string(), "key2".to_string()],
            allow_anonymous: false,
            idle_timeout: Duration::from_secs(1),
            max_sessions: 10,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        })
    }

    #[test]
    fn authenticate_with_valid_key() {
        let mut server = server_with_keys();
        let id = server.authenticate("key1").unwrap();
        assert!(id.starts_with("sess_"));
        let session = server.get_session(&id).unwrap();
        assert!(session.authenticated);
        assert_eq!(session.auth_token, "key1");
    }

    #[test]
    fn authenticate_with_invalid_key() {
        let mut server = server_with_keys();
        let err = server.authenticate("badkey").unwrap_err();
        assert_eq!(err, "invalid API key");
    }

    #[test]
    fn unconfigured_server_refuses_every_token() {
        // No keys and no explicit opt-in: this used to accept anyone who could
        // reach the port.
        let mut server = DaemonServer::new();
        let err = server.authenticate("anything").unwrap_err();
        assert!(err.contains("no API keys"), "{err}");
        assert!(server.get_session("sess_1").is_none());
    }

    #[test]
    fn anonymous_access_is_an_explicit_opt_in() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            allow_anonymous: true,
            idle_timeout: Duration::from_secs(1),
            max_sessions: 10,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        });
        let id = server.authenticate("anything").unwrap();
        assert!(server.get_session(&id).is_some());
    }

    /// Ids must not be reused: a counter derived from `sessions.len()` repeats
    /// after a removal and overwrites a live session.
    #[test]
    fn session_ids_survive_a_removal() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            allow_anonymous: true,
            idle_timeout: Duration::from_secs(1),
            max_sessions: 10,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        });
        let first = server.authenticate("a").unwrap();
        let second = server.authenticate("b").unwrap();
        assert_ne!(first, second);

        assert!(server.disconnect(&first));

        let third = server.authenticate("c").unwrap();
        assert_ne!(third, second, "a removed session's id must not be reused");
        assert!(
            server.get_session(&second).is_some(),
            "the live session must not be overwritten"
        );
    }

    /// Restored rows already own their ids, so the first new auth must continue
    /// past them instead of handing out `sess_1` again and overwriting one.
    #[test]
    fn restored_sessions_advance_the_session_id_counter() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "codape_session_restore_{}_{nanos}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        {
            let mut store = crate::persistence::SessionStore::open(&path).unwrap();
            for id in ["sess_1", "sess_9"] {
                store
                    .save(&crate::persistence::PersistedSession {
                        id: id.to_string(),
                        token_fingerprint: "fingerprint".into(),
                        created_at: "2026-01-01T00:00:00Z".into(),
                        last_activity: "2026-01-01T00:00:00Z".into(),
                    })
                    .unwrap();
            }
        }

        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: vec!["key1".to_string()],
            allow_anonymous: false,
            idle_timeout: Duration::from_secs(3600),
            max_sessions: 10,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: Some(path.clone()),
        });

        assert!(server.get_session("sess_9").is_some(), "rows were restored");

        let id = server.authenticate("key1").unwrap();
        assert_eq!(id, "sess_10", "the counter must resume past restored ids");
        assert!(
            server.get_session("sess_1").is_some(),
            "the restored session must not be overwritten"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cleanup_removes_idle_sessions() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            allow_anonymous: true,
            idle_timeout: Duration::from_millis(50),
            max_sessions: 10,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        });
        let id = server.authenticate("tok").unwrap();
        // Wait for timeout
        std::thread::sleep(Duration::from_millis(60));
        let removed = server.cleanup_idle();
        assert_eq!(removed, vec![id]);
        assert_eq!(server.session_count(), 0);
    }

    #[test]
    fn record_activity_prevents_cleanup() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            allow_anonymous: true,
            idle_timeout: Duration::from_millis(50),
            max_sessions: 10,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        });
        let id = server.authenticate("tok").unwrap();
        // Activity right before timeout
        std::thread::sleep(Duration::from_millis(30));
        server.record_activity(&id);
        std::thread::sleep(Duration::from_millis(30));
        let removed = server.cleanup_idle();
        assert!(removed.is_empty());
        assert_eq!(server.session_count(), 1);
    }

    #[test]
    fn max_sessions_enforced() {
        let mut server = DaemonServer::with_config(ServerConfig {
            valid_keys: Vec::new(),
            allow_anonymous: true,
            idle_timeout: Duration::from_secs(3600),
            max_sessions: 2,
            rate_limit_per_minute: 100,
            health_port: None,
            persist_path: None,
        });
        server.authenticate("a").unwrap();
        server.authenticate("b").unwrap();
        let err = server.authenticate("c").unwrap_err();
        assert_eq!(err, "max sessions reached");
    }

    #[test]
    fn disconnect_removes_session() {
        let mut server = server_with_keys();
        let id = server.authenticate("key1").unwrap();
        assert!(server.disconnect(&id));
        assert!(!server.disconnect(&id)); // already gone
        assert_eq!(server.session_count(), 0);
    }

    #[test]
    fn list_sessions() {
        let mut server = server_with_keys();
        server.authenticate("key1").unwrap();
        server.authenticate("key2").unwrap();
        let sessions = server.list_sessions();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn token_bucket_allows_burst_then_limits() {
        // 5 tokens, refill 1/sec
        let mut bucket = TokenBucket::new(5, 1.0);
        // Should allow 5 consumes
        for _ in 0..5 {
            assert!(bucket.try_consume());
        }
        // 6th should fail (no time elapsed)
        assert!(!bucket.try_consume());
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let mut bucket = TokenBucket::new(10, 100.0); // fast refill for test
        // Drain all tokens
        for _ in 0..10 {
            assert!(bucket.try_consume());
        }
        assert!(!bucket.try_consume());
        // Wait a bit for refill
        std::thread::sleep(Duration::from_millis(50));
        // Should have refilled enough tokens
        assert!(bucket.try_consume());
    }

    #[test]
    fn protocol_serialize_query_request() {
        let req = WsRequest {
            id: "req_1".to_string(),
            kind: WsRequestKind::Query {
                content: "hello".to_string(),
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("req_1"));
        assert!(json.contains("hello"));
        assert!(json.contains("query"));
    }

    #[test]
    fn protocol_serialize_query_response() {
        let resp = WsResponse {
            id: "req_1".to_string(),
            kind: WsResponseKind::QueryOk {
                content: "hi back".to_string(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("query_ok"));
        assert!(json.contains("hi back"));
    }

    #[test]
    fn protocol_serialize_error_response() {
        let resp = WsResponse::error("req_1", "bad_request", "invalid JSON");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("error"));
        assert!(json.contains("bad_request"));
        assert!(json.contains("invalid JSON"));
    }

    #[test]
    fn protocol_serialize_event() {
        let evt = WsEvent::SessionReady {
            session_id: "sess_1".to_string(),
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("session_ready"));
        assert!(json.contains("sess_1"));
    }

    #[test]
    fn protocol_deserialize_query_request() {
        let json = r#"{"id":"r1","type":"query","content":"test"}"#;
        let req: WsRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, "r1");
        match req.kind {
            WsRequestKind::Query { content } => assert_eq!(content, "test"),
            _ => panic!("expected Query variant"),
        }
    }

    #[test]
    fn protocol_deserialize_ping_request() {
        let json = r#"{"id":"r2","type":"ping"}"#;
        let req: WsRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, "r2");
        assert!(matches!(req.kind, WsRequestKind::Ping));
    }

    fn test_connection() -> Connection {
        let mut registry = CommandRegistry::new();
        register_all_commands(&mut registry);
        // No production command is non-interactive *and* remote-forbidden, so
        // without this one the role filter would be untestable: every command
        // the non-interactive filter lets through is also remote-safe, and a
        // test would pass whether or not the role was consulted at all.
        registry.register(SlashCommand::local(
            "bridge-only",
            "a command a remote session may not have",
            true,
            false,
            true,
            Arc::new(|_ctx: &CommandContext, _args: &[String]| CommandResult::success("ok")),
        ));
        Connection {
            config: ServerConfig::default(),
            shared: Arc::new(parking_lot::Mutex::new(Shared::default())),
            session_count_atomic: Arc::new(AtomicUsize::new(0)),
            engine_factory: None,
            commands: Arc::new(registry),
        }
    }

    #[test]
    fn a_remote_session_resolves_only_read_only_shell_commands() {
        let connection = test_connection();

        assert!(
            connection
                .resolve_command(SessionRole::Remote, "ls -la")
                .is_ok()
        );
        assert!(
            connection
                .resolve_command(SessionRole::Remote, "git status")
                .is_ok()
        );

        for refused in ["rm -rf /", "sudo ls", "curl http://evil", "mkdir x"] {
            let denied = connection.resolve_command(SessionRole::Remote, refused);
            assert_eq!(
                denied.unwrap_err().0,
                "command_not_allowed",
                "{refused} should not resolve for a remote session"
            );
        }
    }

    #[test]
    fn a_bridge_session_may_write_but_not_destroy() {
        let connection = test_connection();

        assert!(
            connection
                .resolve_command(SessionRole::Bridge, "mkdir x")
                .is_ok()
        );
        assert!(
            connection
                .resolve_command(SessionRole::Bridge, "cargo build")
                .is_ok()
        );
        assert!(
            connection
                .resolve_command(SessionRole::Bridge, "rm -rf /")
                .is_err()
        );
    }

    #[test]
    fn a_slash_command_must_be_available_to_the_role() {
        let connection = test_connection();

        // `/clear` is flagged remote-safe and bridge-safe, so both roles see it.
        let (command, _) = connection
            .resolve_command(SessionRole::Remote, "/clear")
            .expect("/clear should resolve for a remote session");
        assert_eq!(command, "clear");
        assert!(
            connection
                .resolve_command(SessionRole::Bridge, "/clear")
                .is_ok()
        );

        // `/heapdump` is non-interactive-safe in no sense, so neither role sees
        // it — this is the non-interactive filter, not the role filter.
        for role in [SessionRole::Remote, SessionRole::Bridge] {
            assert_eq!(
                connection.resolve_command(role, "/heapdump").unwrap_err().0,
                "command_not_allowed"
            );
        }

        // Non-interactive but not remote-safe: the role filter, on its own.
        assert_eq!(
            connection
                .resolve_command(SessionRole::Remote, "/bridge-only")
                .unwrap_err()
                .0,
            "command_not_allowed",
            "a remote session got a command that is not remote-safe"
        );
        assert!(
            connection
                .resolve_command(SessionRole::Bridge, "/bridge-only")
                .is_ok()
        );

        // Remote-safe but not bridge-safe: the other direction.
        assert!(
            connection
                .resolve_command(SessionRole::Remote, "/bridge-kick")
                .is_ok()
        );
        assert_eq!(
            connection
                .resolve_command(SessionRole::Bridge, "/bridge-kick")
                .unwrap_err()
                .0,
            "command_not_allowed",
            "a bridge session got a command that is not bridge-safe"
        );
    }

    #[test]
    fn an_empty_command_resolves_to_nothing() {
        let connection = test_connection();
        assert_eq!(
            connection
                .resolve_command(SessionRole::Remote, "   ")
                .unwrap_err()
                .0,
            "empty_command"
        );
    }

    #[test]
    fn the_auth_message_may_name_a_role() {
        let json = r#"{"token":"k","role":"bridge"}"#;
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        let role = value
            .get("role")
            .and_then(|r| serde_json::from_value::<SessionRole>(r.clone()).ok())
            .unwrap_or_default();
        assert_eq!(role, SessionRole::Bridge);
        assert_eq!(role.safety_level(), crate::filter::SafetyLevel::Bridge);
    }

    #[test]
    fn a_session_without_a_role_is_remote() {
        let mut server = server_with_keys();
        let id = server.authenticate("key1").unwrap();
        assert_eq!(server.get_session(&id).unwrap().role, SessionRole::Remote);
    }

    #[test]
    fn a_session_can_open_as_a_bridge() {
        let mut server = server_with_keys();
        let id = server.authenticate_as("key1", SessionRole::Bridge).unwrap();
        assert_eq!(server.get_session(&id).unwrap().role, SessionRole::Bridge);
    }
}
