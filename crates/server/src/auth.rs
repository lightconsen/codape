//! Authentication shared by the TCP services `codape` can expose.
//!
//! `serve` has always validated `CODAPE_API_KEYS` (see
//! [`crate::session::ServerConfig`]). The control plane, the bridge gateway and
//! the teleport rendezvous did not, so anything able to reach the port could
//! register work, submit assignments and read shared sessions. This module is
//! the one implementation all of them call.
//!
//! Two rules, both cheap to state:
//!
//! - A service that listens anywhere but the loopback interface **must** have a
//!   token configured. Without one it refuses to start, rather than coming up
//!   open and waiting for somebody to notice.
//! - A loopback-only service may run without a token — that is the local
//!   development case — and says so once at startup.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

/// The environment variable holding the service tokens, comma separated.
///
/// The same variable `serve` reads, so a deployment configures it once.
pub const SERVICE_TOKENS_ENV: &str = "CODAPE_API_KEYS";

/// The environment variable naming the interface to bind.
///
/// Unset means loopback. Set it to `0.0.0.0`, `::` or a concrete address to
/// expose the service, which then requires a token.
pub const LISTEN_HOST_ENV: &str = "CODAPE_LISTEN_HOST";

/// Why a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// No `Authorization` header was sent.
    Missing,
    /// The header was sent but the token is not in the configured set.
    Invalid,
}

impl AuthError {
    /// A message safe to return to the client.
    ///
    /// It names the shape of the failure and nothing else: which of "missing"
    /// and "wrong" applies is useful to the client and harmless to an
    /// attacker, but the expected value never appears.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Missing => "authentication required: send Authorization: Bearer <token>",
            Self::Invalid => "invalid token",
        }
    }
}

/// The set of tokens a service accepts.
#[derive(Debug, Clone, Default)]
pub struct ServiceAuth {
    tokens: Vec<String>,
}

impl ServiceAuth {
    /// Build from an explicit token list. Empty tokens are dropped.
    #[must_use]
    pub fn new<I: IntoIterator<Item = String>>(tokens: I) -> Self {
        Self {
            tokens: tokens
                .into_iter()
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty())
                .collect(),
        }
    }

    /// Build from a comma-separated string, as the environment carries it.
    #[must_use]
    pub fn from_list(list: &str) -> Self {
        Self::new(list.split(',').map(str::to_string))
    }

    /// Read the token list from [`SERVICE_TOKENS_ENV`].
    ///
    /// Returns an empty set when the variable is unset, which the startup check
    /// treats as "no token configured".
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var(SERVICE_TOKENS_ENV) {
            Ok(list) => Self::from_list(&list),
            Err(_) => Self::default(),
        }
    }

    /// Whether any token is configured.
    #[must_use]
    pub const fn is_configured(&self) -> bool {
        !self.tokens.is_empty()
    }

    /// Check a value read from the `Authorization` header.
    ///
    /// Accepts `Bearer <token>` case-insensitively on the scheme. Comparison is
    /// constant-time per candidate so the failure does not leak, through timing,
    /// how much of a guessed token was right.
    ///
    /// A service with no configured token admits every request. That is only
    /// reachable on a loopback bind, because
    /// [`ensure_exposure_is_authenticated`] refuses to start anything else
    /// without one — the two are a pair, and this trust is what makes local
    /// development need no configuration.
    ///
    /// # Errors
    /// [`AuthError::Missing`] when there is no header or no bearer value,
    /// [`AuthError::Invalid`] when no configured token matches.
    pub fn check_authorization(&self, header: Option<&str>) -> Result<(), AuthError> {
        if !self.is_configured() {
            return Ok(());
        }
        let Some(header) = header else {
            return Err(AuthError::Missing);
        };
        let Some((scheme, value)) = header.split_once(' ') else {
            return Err(AuthError::Missing);
        };
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(AuthError::Missing);
        }
        let value = value.trim();
        if value.is_empty() {
            return Err(AuthError::Missing);
        }

        // Fold over every candidate rather than returning at the first match,
        // so the work done is the same whether the first or the last token hit.
        let mut matched = false;
        for token in &self.tokens {
            matched |= constant_time_eq(token.as_bytes(), value.as_bytes());
        }
        if matched {
            Ok(())
        } else {
            Err(AuthError::Invalid)
        }
    }
}

/// Compare two byte strings without short-circuiting on the first difference.
///
/// Length is compared first and separately: it is not a secret (every token is
/// the same shape) and folding it in would compare fewer bytes for a wrong
/// length, which is the one case an attacker controls cheaply.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// The TCP address a service should bind.
///
/// The default is loopback. A non-loopback address is honoured only when the
/// caller asked for one, and the startup check below is what makes that safe.
///
/// # Errors
/// The host did not resolve to an address.
pub fn resolve_bind_addr(host: Option<&str>, port: u16) -> Result<SocketAddr, String> {
    let host = host
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map_or_else(
            || std::env::var(LISTEN_HOST_ENV).ok(),
            |host| Some(host.to_string()),
        );
    let host = host
        .map(|host| host.trim().to_string())
        .filter(|host| !host.is_empty());

    let Some(host) = host else {
        return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    };

    // A bare address parses directly and avoids a resolver round trip, which
    // also keeps this usable in a container with no DNS.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }

    (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| format!("could not resolve listen host {host:?}: {error}"))?
        .next()
        .ok_or_else(|| format!("listen host {host:?} resolved to no address"))
}

/// Whether a socket address only accepts connections from this machine.
#[must_use]
pub fn is_loopback(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip == Ipv6Addr::LOCALHOST,
    }
}

/// Refuse to expose a service without a token.
///
/// Called immediately before binding. A loopback bind without a token is the
/// local development case and is allowed; anything else must be authenticated.
///
/// # Errors
/// The address is not loopback and no token is configured.
pub fn ensure_exposure_is_authenticated(
    addr: &SocketAddr,
    auth: &ServiceAuth,
    service: &str,
) -> Result<(), String> {
    if auth.is_configured() || is_loopback(addr) {
        return Ok(());
    }
    Err(format!(
        "refusing to start the {service} on {addr} without a token: set {SERVICE_TOKENS_ENV}, \
         or bind loopback (the default) and reach it through a local proxy"
    ))
}

/// A one-line startup note about the authentication a service is running with.
#[must_use]
pub fn exposure_note(addr: &SocketAddr, auth: &ServiceAuth) -> String {
    if auth.is_configured() {
        format!("token required ({SERVICE_TOKENS_ENV})")
    } else if is_loopback(addr) {
        "no token configured; loopback only".to_string()
    } else {
        "no token configured".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_bearer_token_is_accepted() {
        let auth = ServiceAuth::from_list("alpha,beta");
        assert!(auth.check_authorization(Some("Bearer alpha")).is_ok());
        assert!(auth.check_authorization(Some("Bearer beta")).is_ok());
    }

    #[test]
    fn the_scheme_is_case_insensitive() {
        let auth = ServiceAuth::from_list("alpha");
        assert!(auth.check_authorization(Some("bearer alpha")).is_ok());
        assert!(auth.check_authorization(Some("BEARER alpha")).is_ok());
    }

    #[test]
    fn a_missing_header_is_reported_as_missing() {
        let auth = ServiceAuth::from_list("alpha");
        assert_eq!(auth.check_authorization(None), Err(AuthError::Missing));
        assert_eq!(
            auth.check_authorization(Some("alpha")),
            Err(AuthError::Missing),
            "a bare value without a scheme is not a bearer credential"
        );
        assert_eq!(
            auth.check_authorization(Some("Basic alpha")),
            Err(AuthError::Missing)
        );
        assert_eq!(
            auth.check_authorization(Some("Bearer ")),
            Err(AuthError::Missing)
        );
    }

    #[test]
    fn a_wrong_token_is_invalid_not_missing() {
        let auth = ServiceAuth::from_list("alpha");
        assert_eq!(
            auth.check_authorization(Some("Bearer alph")),
            Err(AuthError::Invalid)
        );
        assert_eq!(
            auth.check_authorization(Some("Bearer alphaa")),
            Err(AuthError::Invalid)
        );
    }

    #[test]
    fn an_unconfigured_service_admits_every_request() {
        // Only reachable on a loopback bind: the startup check refuses an
        // unauthenticated public bind, so this is the local-development path.
        let auth = ServiceAuth::default();
        assert!(!auth.is_configured());
        assert!(auth.check_authorization(None).is_ok());
        assert!(auth.check_authorization(Some("Bearer anything")).is_ok());
    }

    #[test]
    fn a_configured_service_still_rejects_a_missing_header() {
        let auth = ServiceAuth::from_list("alpha");
        assert_eq!(auth.check_authorization(None), Err(AuthError::Missing));
    }

    #[test]
    fn blank_entries_in_the_list_are_dropped() {
        let auth = ServiceAuth::from_list(" alpha , ,beta ,");
        assert!(auth.check_authorization(Some("Bearer alpha")).is_ok());
        assert!(auth.check_authorization(Some("Bearer beta")).is_ok());
        assert_eq!(
            auth.check_authorization(Some("Bearer ")),
            Err(AuthError::Missing)
        );
    }

    #[test]
    fn constant_time_eq_matches_ordinary_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn the_default_bind_is_loopback() {
        let addr = resolve_bind_addr(None, 1234).expect("loopback resolves");
        assert!(is_loopback(&addr));
        assert_eq!(addr.port(), 1234);
    }

    #[test]
    fn an_explicit_wildcard_bind_is_honoured() {
        let addr = resolve_bind_addr(Some("0.0.0.0"), 0).expect("wildcard parses");
        assert!(!is_loopback(&addr));
        assert_eq!(addr.ip().to_string(), "0.0.0.0");
    }

    #[test]
    fn a_service_on_a_public_address_needs_a_token() {
        let addr: SocketAddr = "0.0.0.0:9000".parse().expect("valid address");
        let error = ensure_exposure_is_authenticated(&addr, &ServiceAuth::default(), "gateway")
            .expect_err("an open public bind must be refused");
        assert!(error.contains("refusing to start"));
        assert!(error.contains(SERVICE_TOKENS_ENV));
    }

    #[test]
    fn a_service_on_loopback_may_run_without_a_token() {
        let addr: SocketAddr = "127.0.0.1:9000".parse().expect("valid address");
        assert!(
            ensure_exposure_is_authenticated(&addr, &ServiceAuth::default(), "gateway").is_ok()
        );
    }

    #[test]
    fn a_token_allows_a_public_bind() {
        let addr: SocketAddr = "0.0.0.0:9000".parse().expect("valid address");
        let auth = ServiceAuth::from_list("secret");
        assert!(ensure_exposure_is_authenticated(&addr, &auth, "gateway").is_ok());
    }

    #[test]
    fn the_exposure_note_names_the_mode() {
        let loopback: SocketAddr = "127.0.0.1:1".parse().expect("valid address");
        let public: SocketAddr = "0.0.0.0:1".parse().expect("valid address");
        assert_eq!(
            exposure_note(&loopback, &ServiceAuth::default()),
            "no token configured; loopback only"
        );
        assert_eq!(
            exposure_note(&public, &ServiceAuth::from_list("secret")),
            "token required (CODAPE_API_KEYS)"
        );
    }
}
