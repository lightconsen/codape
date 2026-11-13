//! Rendezvous service for `codape teleport`.
//!
//! `TeleportManager` can hand a session snapshot to another *directory* on the
//! same machine, but not to another machine: the two sides need somewhere to
//! meet, and each machine is behind its own NAT. This is that meeting point.
//! It stores a snapshot under an unguessable token until the sharer revokes
//! it or its time to live runs out, and it does nothing else — it never reads
//! the snapshot, never merges, and keeps no history.
//!
//! Endpoints (all JSON):
//!
//! - `POST /teleport/shares` with `{"snapshot": {...}}` → `{"token": "..."}`
//! - `GET /teleport/shares` → `{"shares": [{"session_id","created_at"}]}`
//! - `GET /teleport/shares/{token}` → the snapshot
//! - `DELETE /teleport/shares/{token}` → `204`, or `404` when it is gone
//!
//! Every request must carry `Authorization: Bearer <token>` from
//! [`crate::auth::SERVICE_TOKENS_ENV`], except on a loopback bind with no token
//! configured. See [`crate::auth`] for the rule and why it exists.
//!
//! The service is deliberately small enough to run on a laptop: no TLS, no
//! accounts. Put it behind the deployment's own TLS (a reverse proxy, a private
//! network). The share token is a capability, and anyone who has it can read
//! the session — so [`ShareInfo`] repeating it back is only safe because the
//! listing is behind the service token; see that type for why.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::auth::{AuthError, ServiceAuth, ensure_exposure_is_authenticated, exposure_note};

/// Path every teleport endpoint hangs off.
pub const TELEPORT_SHARE_PATH: &str = "/teleport/shares";

/// How long a shared session stays readable by default.
pub const DEFAULT_TTL: Duration = Duration::from_hours(7 * 24);

/// One stored snapshot.
#[derive(Debug, Clone)]
struct Share {
    token: String,
    session_id: String,
    created_at: DateTime<Utc>,
    snapshot: Value,
}

/// What `GET /teleport/shares` reports for each share.
///
/// The token is included, and that is only safe because the endpoint is behind
/// [`crate::auth`]: it used to be reachable with no credential at all, which
/// made every share joinable by anyone who could reach the port. An operator
/// holding the service token can already revoke; being able to see what is
/// shared is the point of the listing.
#[derive(Debug, Clone, Serialize)]
pub struct ShareInfo {
    /// The capability token.
    pub token: String,
    /// The session the snapshot belongs to.
    pub session_id: String,
    /// When it was shared.
    pub created_at: String,
}

/// The rendezvous service.
#[derive(Debug)]
pub struct TeleportService {
    port: u16,
    ttl: Duration,
    shares: Arc<Mutex<HashMap<String, Share>>>,
    auth: ServiceAuth,
}

impl TeleportService {
    /// Create a service that will listen on `port` (0 picks a free one).
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            port,
            ttl: DEFAULT_TTL,
            shares: Arc::new(Mutex::new(HashMap::new())),
            auth: ServiceAuth::default(),
        }
    }

    /// Create a service with an explicit time to live.
    #[must_use]
    pub fn with_ttl(port: u16, ttl: Duration) -> Self {
        Self {
            port,
            ttl,
            shares: Arc::new(Mutex::new(HashMap::new())),
            auth: ServiceAuth::default(),
        }
    }

    /// Require a bearer token on every request.
    ///
    /// The CLI fills this from [`crate::auth::SERVICE_TOKENS_ENV`]; a caller
    /// that binds anything but loopback must call it, or [`Self::run`] refuses
    /// to start.
    #[must_use]
    pub fn with_auth(mut self, auth: ServiceAuth) -> Self {
        self.auth = auth;
        self
    }

    /// The configured port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Bind and serve until the process stops.
    ///
    /// The bind address comes from [`crate::auth::resolve_bind_addr`], which
    /// defaults to loopback.
    ///
    /// # Errors
    /// The address could not be resolved, the exposure is unauthenticated, or
    /// the port could not be bound.
    pub async fn run(&self) -> Result<(), String> {
        let addr = crate::auth::resolve_bind_addr(None, self.port)?;
        ensure_exposure_is_authenticated(&addr, &self.auth, "teleport service")?;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|error| format!("failed to bind the teleport service on {addr}: {error}"))?;
        self.run_on(listener).await
    }

    /// Serve an already-bound listener.
    ///
    /// Tests bind port 0 and ask the listener for its address; the production
    /// path binds the configured port.
    pub async fn run_on(&self, listener: TcpListener) -> Result<(), String> {
        let address = listener
            .local_addr()
            .map_err(|error| format!("could not read the listener address: {error}"))?;
        info!(
            "teleport rendezvous listening on http://{address}{TELEPORT_SHARE_PATH} ({})",
            exposure_note(&address, &self.auth)
        );
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(error) => {
                    warn!("teleport accept error: {error}");
                    continue;
                }
            };
            let shares = Arc::clone(&self.shares);
            let ttl = self.ttl;
            let auth = self.auth.clone();
            tokio::spawn(async move {
                if let Err(error) = handle(stream, shares, ttl, auth).await {
                    warn!("teleport request failed: {error}");
                }
            });
        }
    }

    /// Store a snapshot and return its token.
    pub async fn share(&self, snapshot: Value) -> Result<String, String> {
        let session_id = snapshot
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "the snapshot has no session_id".to_string())?
            .to_string();
        let token = uuid::Uuid::new_v4().simple().to_string();
        let share = Share {
            token: token.clone(),
            session_id,
            created_at: Utc::now(),
            snapshot,
        };
        self.shares.lock().await.insert(token.clone(), share);
        Ok(token)
    }

    /// The snapshot behind `token`, or `None` when it is unknown or expired.
    pub async fn join(&self, token: &str) -> Option<Value> {
        let mut shares = self.shares.lock().await;
        shares.retain(|_, share| !expired(share, self.ttl));
        shares.get(token).map(|share| share.snapshot.clone())
    }

    /// The live shares, newest first.
    pub async fn list(&self) -> Vec<ShareInfo> {
        let mut shares = self.shares.lock().await;
        shares.retain(|_, share| !expired(share, self.ttl));
        let mut live: Vec<ShareInfo> = shares
            .values()
            .map(|share| ShareInfo {
                token: share.token.clone(),
                session_id: share.session_id.clone(),
                created_at: share.created_at.to_rfc3339(),
            })
            .collect();
        live.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        live
    }

    /// Remove a share. `false` means it was not there to begin with.
    pub async fn revoke(&self, token: &str) -> bool {
        self.shares.lock().await.remove(token).is_some()
    }
}

/// Whether a share is past its time to live.
fn expired(share: &Share, ttl: Duration) -> bool {
    let Ok(ttl) = chrono::TimeDelta::from_std(ttl) else {
        return false;
    };
    Utc::now().signed_duration_since(share.created_at) > ttl
}

/// Serve one HTTP request.
async fn handle(
    mut stream: TcpStream,
    shares: Arc<Mutex<HashMap<String, Share>>>,
    ttl: Duration,
    auth: ServiceAuth,
) -> Result<(), String> {
    let request = read_request(&mut stream).await?;
    let service = TeleportService {
        port: 0,
        ttl,
        shares,
        auth: ServiceAuth::default(),
    };

    // Authenticate before dispatching: the route table never sees a request
    // that has not passed here, so no endpoint can be added later that forgets.
    if let Err(refusal) = auth.check_authorization(request.authorization.as_deref()) {
        let status = match refusal {
            AuthError::Missing => 401,
            AuthError::Invalid => 403,
        };
        return write_response(&mut stream, status, &json!({"error": refusal.message()})).await;
    }

    let (ref method, ref path) = (request.method, request.path);
    let body = request.body;
    let response = match (method.as_str(), path.as_str()) {
        ("POST", TELEPORT_SHARE_PATH) => match parse_snapshot(&body) {
            Ok(snapshot) => match service.share(snapshot).await {
                Ok(token) => (201, json!({"token": token})),
                Err(error) => (400, json!({"error": error})),
            },
            Err(error) => (400, json!({"error": error})),
        },
        ("GET", TELEPORT_SHARE_PATH) => {
            let shares = service.list().await;
            (200, json!({ "shares": shares }))
        }
        ("GET", path) => match share_token(path) {
            Some(token) => match service.join(token).await {
                Some(snapshot) => (200, snapshot),
                None => (404, json!({"error": "unknown or expired share"})),
            },
            None => (404, json!({"error": "not found"})),
        },
        ("DELETE", path) => match share_token(path) {
            Some(token) if service.revoke(token).await => (204, Value::Null),
            Some(_) => (404, json!({"error": "unknown or expired share"})),
            None => (404, json!({"error": "not found"})),
        },
        _ => (404, json!({"error": "not found"})),
    };

    write_response(&mut stream, response.0, &response.1).await
}

/// The token in `/teleport/shares/<token>`, when the path has one.
fn share_token(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(TELEPORT_SHARE_PATH)?.strip_prefix('/')?;
    if rest.is_empty() || rest.contains('/') || rest.contains('?') {
        return None;
    }
    Some(rest)
}

/// Read the snapshot out of a share request body.
fn parse_snapshot(body: &[u8]) -> Result<Value, String> {
    let request: Value = serde_json::from_slice(body)
        .map_err(|error| format!("the request body is not JSON: {error}"))?;
    request
        .get("snapshot")
        .cloned()
        .ok_or_else(|| "the request has no snapshot".to_string())
}

/// One parsed HTTP request.
struct HttpRequest {
    method: String,
    path: String,
    /// The raw `Authorization` header value, when one was sent.
    authorization: Option<String>,
    body: Vec<u8>,
}

/// Read one HTTP request.
async fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("the connection closed before the request head".to_string());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 64 * 1024 {
            return Err("the request head is larger than 64 KiB".to_string());
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut authorization = None;
    let mut content_length = 0_usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse::<usize>().unwrap_or(0);
        }
    }
    if content_length > 64 * 1024 * 1024 {
        return Err("the request body is larger than 64 MiB".to_string());
    }

    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(HttpRequest {
        method,
        path,
        authorization,
        body,
    })
}

/// Write one JSON response.
async fn write_response(stream: &mut TcpStream, status: u16, body: &Value) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Unknown",
    };
    let payload = if status == 204 {
        Vec::new()
    } else {
        serde_json::to_vec(body).map_err(|error| error.to_string())?
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        payload.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(&payload)
        .await
        .map_err(|error| error.to_string())?;
    let _ = stream.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(session: &str) -> Value {
        json!({
            "session_id": session,
            "title": "a session",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}],
        })
    }

    #[tokio::test]
    async fn a_shared_snapshot_can_be_joined_by_its_token() {
        let service = TeleportService::new(0);
        let token = service.share(snapshot("sess-1")).await.unwrap();
        let joined = service.join(&token).await.expect("the share is live");
        assert_eq!(joined["session_id"], "sess-1");
    }

    #[tokio::test]
    async fn an_unknown_token_is_not_found() {
        let service = TeleportService::new(0);
        assert!(service.join("no-such-token").await.is_none());
    }

    #[tokio::test]
    async fn revoking_removes_the_share() {
        let service = TeleportService::new(0);
        let token = service.share(snapshot("sess-1")).await.unwrap();
        assert!(service.revoke(&token).await);
        assert!(service.join(&token).await.is_none());
        assert!(
            !service.revoke(&token).await,
            "a second revoke is not a success"
        );
    }

    #[tokio::test]
    async fn listing_reports_live_shares_without_the_snapshot() {
        let service = TeleportService::new(0);
        service.share(snapshot("sess-1")).await.unwrap();
        let listed = service.list().await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, "sess-1");
        assert!(!listed[0].token.is_empty());
    }

    #[tokio::test]
    async fn listing_requires_a_token_when_one_is_configured() {
        // The listing repeats share tokens, so it is exactly the endpoint that
        // must never answer without a credential.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let service = TeleportService::new(0).with_auth(ServiceAuth::from_list("secret"));
        let served = tokio::spawn(async move { service.run_on(listener).await });

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"GET /teleport/shares HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("read");
        assert!(
            response.starts_with("HTTP/1.1 401"),
            "unauthenticated listing: {response}"
        );

        served.abort();
    }

    #[tokio::test]
    async fn an_unauthenticated_request_is_refused_before_routing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let service = TeleportService::new(0).with_auth(ServiceAuth::from_list("secret"));
        let served = tokio::spawn(async move { service.run_on(listener).await });

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"GET /teleport/shares HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("read");
        assert!(
            response.starts_with("HTTP/1.1 401"),
            "unauthenticated read: {response}"
        );

        served.abort();
    }

    #[tokio::test]
    async fn an_authenticated_request_reaches_the_route() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let service = TeleportService::new(0).with_auth(ServiceAuth::from_list("secret"));
        let served = tokio::spawn(async move { service.run_on(listener).await });

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(
                b"GET /teleport/shares HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer secret\r\n\r\n",
            )
            .await
            .expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("read");
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "authenticated read: {response}"
        );

        served.abort();
    }

    #[tokio::test]
    async fn a_wrong_token_is_forbidden_not_unauthorized() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let service = TeleportService::new(0).with_auth(ServiceAuth::from_list("secret"));
        let served = tokio::spawn(async move { service.run_on(listener).await });

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(
                b"GET /teleport/shares HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n",
            )
            .await
            .expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("read");
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "wrong token: {response}"
        );

        served.abort();
    }

    #[tokio::test]
    async fn a_share_past_its_ttl_is_gone() {
        let service = TeleportService::with_ttl(0, Duration::from_secs(0));
        let token = service.share(snapshot("sess-1")).await.unwrap();
        // Sleep just past the zero TTL so the comparison is unambiguous.
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(service.join(&token).await.is_none());
        assert!(service.list().await.is_empty());
    }

    #[tokio::test]
    async fn a_snapshot_without_a_session_id_is_refused() {
        let service = TeleportService::new(0);
        assert!(service.share(json!({"title": "no id"})).await.is_err());
    }

    #[test]
    fn only_a_bare_token_after_the_path_is_a_share() {
        assert_eq!(share_token("/teleport/shares/abc"), Some("abc"));
        assert_eq!(share_token("/teleport/shares"), None);
        assert_eq!(share_token("/teleport/shares/"), None);
        assert_eq!(share_token("/teleport/shares/a/b"), None);
        assert_eq!(share_token("/other"), None);
    }
}
