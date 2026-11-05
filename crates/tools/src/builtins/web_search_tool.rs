//! WebSearch tool — search query with domain filtering.
//!
//! # Backends
//!
//! The tool used to have exactly one backend: the DuckDuckGo HTML endpoint,
//! hard-coded. Anywhere that endpoint is blocked — a corporate proxy, a
//! firewall, a container with no egress except through a proxy — every search
//! failed, and it failed as `InvalidInput`, which reads like the model called
//! the tool wrong rather than like the backend being unreachable.
//!
//! Now there is a configurable, ordered list. Each entry names a backend kind
//! (and an optional endpoint for the kinds that take one); the tool tries them
//! in order and only reports failure when all of them fail. Configuration
//! comes from (highest precedence first):
//!
//! 1. `CODEREX_SEARCH_BACKENDS` — a comma-separated list, parsed by
//!    [`SearchBackendSpec::parse_list`]. An entry that fails to parse is
//!    skipped with a warning rather than silently replacing the default.
//! 2. `CODEREX_SEARCH_BACKEND` — a single entry, same syntax as one element of
//!    the list above. Kept for scripts that only need to override one.
//! 3. The built-in default: `duckduckgo`.
//!
//! Syntax per entry: `kind` or `kind:endpoint`. For example
//!
//! ```text
//! CODEREX_SEARCH_BACKENDS=json:https://search.internal/api,duckduckgo
//! ```
//!
//! tries an internal JSON endpoint first and falls back to DuckDuckGo. A
//! `$VAR` endpoint expands from the environment, so a token URL can live in
//! the environment instead of in a settings file.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Maximum number of results to return.
const MAX_RESULTS: usize = 10;

/// Environment variable naming the ordered backend list.
pub const SEARCH_BACKENDS_ENV: &str = "CODEREX_SEARCH_BACKENDS";

/// Environment variable naming a single backend, when a list is overkill.
pub const SEARCH_BACKEND_ENV: &str = "CODEREX_SEARCH_BACKEND";

/// The kind of backend an entry describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    /// `https://html.duckduckgo.com/html/` — HTML, no API key.
    DuckDuckGo,
    /// A JSON endpoint that answers `?q=` with either `{"results":[{"title",
    /// "url","snippet"}]}` or an array of the same objects. The endpoint is
    /// the entry's URL.
    Json,
}

impl BackendKind {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::DuckDuckGo => "duckduckgo",
            Self::Json => "json",
        }
    }
}

/// One configured backend: a kind plus the endpoint it talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchBackendSpec {
    /// Which protocol to speak.
    pub kind: BackendKind,
    /// Where to send it. `None` means the kind's built-in endpoint.
    pub endpoint: Option<String>,
}

impl SearchBackendSpec {
    /// The DuckDuckGo HTML endpoint.
    #[must_use]
    pub const fn duckduckgo() -> Self {
        Self {
            kind: BackendKind::DuckDuckGo,
            endpoint: None,
        }
    }

    /// A JSON backend at `endpoint`.
    #[must_use]
    pub fn json(endpoint: impl Into<String>) -> Self {
        Self {
            kind: BackendKind::Json,
            endpoint: Some(endpoint.into()),
        }
    }

    /// The URL this spec requests, before the query is appended.
    ///
    /// # Errors
    ///
    /// Returns an error when a JSON backend has no endpoint — there is no
    /// sensible default for one.
    pub fn base_url(&self) -> Result<String, ToolError> {
        match (&self.kind, &self.endpoint) {
            (BackendKind::DuckDuckGo, None) => Ok("https://html.duckduckgo.com/html/".to_string()),
            (BackendKind::DuckDuckGo | BackendKind::Json, Some(url)) => Ok(url.clone()),
            (BackendKind::Json, None) => Err(ToolError::InvalidInput {
                message: "a `json` search backend needs an endpoint (`json:https://…`)".to_string(),
            }),
        }
    }

    /// Parse one entry: `kind` or `kind:endpoint`.
    ///
    /// `$VAR` in the endpoint expands from the environment; an unset variable
    /// is left as written so the resulting request fails visibly instead of
    /// quietly becoming an empty URL.
    pub fn parse(entry: &str) -> Result<Self, String> {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err("empty backend entry".to_string());
        }
        let (kind, endpoint) = match entry.split_once(':') {
            // A bare `https://…` splits on its scheme colon, which is not the
            // kind/endpoint separator, so the left side is only taken as a kind
            // when the right side does not start with `//`. A genuine entry
            // always names a kind on the left, and an unknown one is rejected
            // below rather than being read as a URL.
            Some((left, rest)) if !rest.starts_with("//") => (left, Some(rest)),
            _ => (entry, None),
        };
        let kind = match kind.trim().to_ascii_lowercase().as_str() {
            "duckduckgo" | "ddg" => BackendKind::DuckDuckGo,
            "json" | "sse_json" => BackendKind::Json,
            other => return Err(format!("unknown search backend kind `{other}`")),
        };
        let endpoint = endpoint.map(|raw| {
            let raw = raw.trim();
            match raw.strip_prefix('$') {
                Some(var) => std::env::var(var).unwrap_or_else(|_| {
                    tracing::warn!(
                        variable = var,
                        "search backend endpoint environment variable is unset"
                    );
                    raw.to_string()
                }),
                None => raw.to_string(),
            }
        });
        Ok(Self { kind, endpoint })
    }

    /// Parse a comma-separated list, skipping entries that do not parse.
    ///
    /// A bad entry is reported and dropped. The alternative — failing the whole
    /// list — would make one typo disable search entirely, and falling back to
    /// the default would hide the typo.
    #[must_use]
    pub fn parse_list(list: &str) -> Vec<Self> {
        list.split(',')
            .filter(|entry| !entry.trim().is_empty())
            .filter_map(|entry| match Self::parse(entry) {
                Ok(spec) => Some(spec),
                Err(error) => {
                    tracing::warn!(entry = entry.trim(), %error, "skipping search backend");
                    None
                }
            })
            .collect()
    }

    /// The configured backends, in order, or the built-in default.
    #[must_use]
    pub fn from_env() -> Vec<Self> {
        if let Ok(list) = std::env::var(SEARCH_BACKENDS_ENV) {
            let parsed = Self::parse_list(&list);
            if !parsed.is_empty() {
                return parsed;
            }
            tracing::warn!(
                variable = SEARCH_BACKENDS_ENV,
                "no usable search backend in the list; using the default"
            );
        }
        if let Ok(single) = std::env::var(SEARCH_BACKEND_ENV) {
            match Self::parse(&single) {
                Ok(spec) => return vec![spec],
                Err(error) => tracing::warn!(
                    variable = SEARCH_BACKEND_ENV,
                    %error,
                    "invalid search backend; using the default"
                ),
            }
        }
        vec![Self::duckduckgo()]
    }
}

/// Search the web with domain filtering.
#[derive(Debug, Default)]
pub struct WebSearchTool {
    /// Backends to try, in order. `None` means "read the environment at call
    /// time", so a session that exports the variable after construction still
    /// takes effect.
    backends: Option<Vec<SearchBackendSpec>>,
}

impl WebSearchTool {
    /// A tool that reads its backends from the environment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A tool with an explicit backend order, ignoring the environment.
    #[must_use]
    pub const fn with_backends(backends: Vec<SearchBackendSpec>) -> Self {
        Self {
            backends: Some(backends),
        }
    }

    fn backends(&self) -> Vec<SearchBackendSpec> {
        self.backends
            .clone()
            .unwrap_or_else(SearchBackendSpec::from_env)
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn description(&self) -> Option<&'static str> {
        Some(
            "Searches the web and returns result titles and URLs. Use it for documentation, \
             release notes, or a fact you cannot establish from the machine; quote or read the \
             page with WebFetch before relying on it.",
        )
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "WebSearch".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "allowed_domains": { "type": "array", "items": { "type": "string" }, "description": "Only include results from these domains" },
                    "blocked_domains": { "type": "array", "items": { "type": "string" }, "description": "Exclude results from these domains" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "results": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        use std::fmt::Write as _;

        let query = extract_string(&input.arguments, "query")?;
        let allowed_domains = extract_string_array(&input.arguments, "allowed_domains");
        let blocked_domains = extract_string_array(&input.arguments, "blocked_domains");

        // Rate limit check
        if let Some(limiter) = crate::rate_limiter::get_rate_limiter(&ctx)
            && !limiter.check("WebSearch", 10)
        {
            return Err(ToolError::RateLimited {
                name: "WebSearch".into(),
            });
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ToolError::Execution {
                name: "WebSearch".into(),
                message: format!("failed to create HTTP client: {e}"),
            })?;

        let backends = self.backends();
        let mut failures: Vec<String> = Vec::new();
        let mut results: Option<Vec<SearchResult>> = None;

        for spec in &backends {
            match search_with(&client, spec, &query, &allowed_domains, &blocked_domains).await {
                Ok(found) => {
                    results = Some(found);
                    break;
                }
                Err(error) => {
                    tracing::warn!(
                        backend = spec.kind.as_str(),
                        endpoint = spec.endpoint.as_deref().unwrap_or("<default>"),
                        %error,
                        "search backend failed"
                    );
                    failures.push(format!("{}: {error}", spec.kind.as_str()));
                }
            }
        }

        // Every backend failing is the backend being unavailable, not the
        // model asking wrong. Reporting it as `InvalidInput` is what made this
        // look like a model mistake in the terminal-bench run.
        let results = results.ok_or_else(|| ToolError::Execution {
            name: "WebSearch".into(),
            message: format!(
                "every search backend failed ({}). Set {SEARCH_BACKENDS_ENV} to a reachable \
                 backend, e.g. `json:https://your-search/api`.",
                failures.join("; ")
            ),
        })?;

        let mut output = format!("Search results for: \"{query}\"\n\n");
        for (i, r) in results.iter().take(MAX_RESULTS).enumerate() {
            let _ = write!(
                output,
                "{}. [{}]({})\n   {}\n\n",
                i + 1,
                r.title,
                r.url,
                r.snippet
            );
        }

        if results.is_empty() {
            output.push_str("No results found.\n");
        }

        Ok(ToolOutput {
            output,
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Run one backend. `Err` means "this backend failed", which the caller turns
/// into a try-the-next-one.
async fn search_with(
    client: &reqwest::Client,
    spec: &SearchBackendSpec,
    query: &str,
    allowed: &[String],
    blocked: &[String],
) -> Result<Vec<SearchResult>, String> {
    let base = spec.base_url().map_err(|e| e.to_string())?;
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}q={}", url_encode(query));

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("transport error: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }

    let body = resp.text().await.map_err(|e| format!("read error: {e}"))?;

    let parsed = match spec.kind {
        BackendKind::DuckDuckGo => parse_ddg_results(&body, allowed, blocked),
        BackendKind::Json => {
            let raw = parse_json_results(&body).map_err(|e| format!("invalid JSON: {e}"))?;
            filter_results(raw, allowed, blocked)
        }
    };
    Ok(parsed)
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Parse `{"results":[…]}` or `[…]` into results, preserving title/snippet.
fn parse_json_results(body: &str) -> Result<Vec<SearchResult>, String> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let array = value
        .get("results")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array())
        .ok_or("expected a `results` array or a top-level array")?;

    Ok(array
        .iter()
        .filter_map(|entry| {
            let url = entry.get("url")?.as_str()?.to_string();
            let title = entry
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(url.as_str())
                .to_string();
            let snippet = entry
                .get("snippet")
                .or_else(|| entry.get("description"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(SearchResult {
                title,
                url,
                snippet,
            })
        })
        .collect())
}

fn filter_results(
    results: Vec<SearchResult>,
    allowed: &[String],
    blocked: &[String],
) -> Vec<SearchResult> {
    results
        .into_iter()
        .filter(|r| {
            let domain = extract_domain(&r.url);
            (allowed.is_empty() || allowed.iter().any(|d| domain.contains(d.as_str())))
                && !blocked.iter().any(|d| domain.contains(d.as_str()))
        })
        .collect()
}

fn parse_ddg_results(html: &str, allowed: &[String], blocked: &[String]) -> Vec<SearchResult> {
    let mut results = Vec::new();

    // Extract result links from DDG HTML
    for line in html.lines() {
        if let Some(url) = extract_ddg_url(line) {
            let domain = extract_domain(&url);

            // Apply domain filters
            if !allowed.is_empty() && !allowed.iter().any(|d| domain.contains(d.as_str())) {
                continue;
            }
            if blocked.iter().any(|d| domain.contains(d.as_str())) {
                continue;
            }

            results.push(SearchResult {
                title: domain.clone(),
                url,
                snippet: String::new(),
            });

            if results.len() >= 20 {
                break;
            }
        }
    }

    results
}

fn extract_ddg_url(line: &str) -> Option<String> {
    // DDG result links look like: <a rel="nofollow" class="result__a" href="https://example.com/...">
    if (line.contains("result__a") || line.contains("class=\"result\""))
        && let Some(start) = line.find("href=\"")
    {
        let rest = &line[start + 6..];
        if let Some(end) = rest.find('"') {
            let url = &rest[..end];
            if url.starts_with("http") {
                return Some(url.to_string());
            }
        }
    }
    None
}

fn extract_domain(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

/// Percent-encode UTF-8 bytes.
///
/// The previous version encoded by `char`, which produced `%4E2D` for `中`
/// instead of the byte-wise `%E4%B8%AD` — invalid percent-encoding, so any
/// non-ASCII query was mangled by the server.
fn url_encode(s: &str) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

fn extract_string_array(args: &serde_json::Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::path::Path;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext::new(ExecutionContext::Normal, Path::new("/tmp"))
    }

    fn websearch_input(query: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "WebSearch".into(),
            arguments: serde_json::json!({ "query": query }),
        }
    }

    fn websearch_with_domains(query: &str, allowed: Vec<&str>, blocked: Vec<&str>) -> ToolInput {
        let mut args = serde_json::json!({ "query": query });
        if !allowed.is_empty() {
            args["allowed_domains"] = serde_json::json!(allowed);
        }
        if !blocked.is_empty() {
            args["blocked_domains"] = serde_json::json!(blocked);
        }
        ToolInput {
            call_id: "1".into(),
            name: "WebSearch".into(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn websearch_missing_query_errors() {
        let input = ToolInput {
            call_id: "1".into(),
            name: "WebSearch".into(),
            arguments: serde_json::json!({}),
        };
        let result = WebSearchTool::new().execute(input, test_ctx()).await;
        assert!(result.is_err());
    }

    #[test]
    fn extract_domain_works() {
        assert_eq!(extract_domain("https://example.com/path"), "example.com");
        assert_eq!(extract_domain("http://sub.example.org/"), "sub.example.org");
    }

    #[test]
    fn url_encode_escapes_special_chars() {
        let encoded = url_encode("hello world");
        assert!(encoded.contains("hello") && encoded.contains("world"));
    }

    #[test]
    fn domain_filtering_logic() {
        let blocked = ["spam.com".to_string(), "ads.net".to_string()];
        let allowed = ["trusted.com".to_string()];

        // Blocked domain should be filtered
        assert!(blocked.iter().any(|d| "spam.com".contains(d.as_str())));

        // Allowed domain check
        assert!(allowed.iter().any(|d| "trusted.com".contains(d.as_str())));
        assert!(!allowed.iter().any(|d| "other.com".contains(d.as_str())));
    }

    #[test]
    fn url_encode_handles_non_ascii_bytes() {
        // Byte-wise encoding is `%E4%B8%AD`, not the old char-wise `%4E2D`.
        assert_eq!(url_encode("中"), "%E4%B8%AD");
        assert_eq!(url_encode("a b"), "a%20b");
    }

    #[test]
    fn backend_spec_parses_kind_and_endpoint() {
        assert_eq!(
            SearchBackendSpec::parse("duckduckgo").unwrap(),
            SearchBackendSpec::duckduckgo()
        );
        assert_eq!(
            SearchBackendSpec::parse("ddg").unwrap(),
            SearchBackendSpec::duckduckgo()
        );
        assert_eq!(
            SearchBackendSpec::parse("json:https://search.internal/api").unwrap(),
            SearchBackendSpec::json("https://search.internal/api")
        );
        // The colon in a bare URL is a scheme separator, not a kind separator,
        // so this should parse as an unknown kind rather than as kind `https`.
        assert!(SearchBackendSpec::parse("https://search.internal/api").is_err());
        assert!(SearchBackendSpec::parse("").is_err());
        assert!(SearchBackendSpec::parse("nope").is_err());
    }

    #[test]
    fn backend_spec_list_skips_bad_entries() {
        let specs = SearchBackendSpec::parse_list("bogus, json:https://a/api , duckduckgo");
        assert_eq!(
            specs,
            vec![
                SearchBackendSpec::json("https://a/api"),
                SearchBackendSpec::duckduckgo()
            ]
        );
    }

    #[test]
    fn json_backend_without_endpoint_is_an_error() {
        let spec = SearchBackendSpec {
            kind: BackendKind::Json,
            endpoint: None,
        };
        assert!(spec.base_url().is_err());
    }
    #[test]
    fn overridden_duckduckgo_endpoint_is_used() {
        let spec = SearchBackendSpec {
            kind: BackendKind::DuckDuckGo,
            endpoint: Some("https://ddg.example/html/".into()),
        };
        assert_eq!(spec.base_url().unwrap(), "https://ddg.example/html/");
    }

    #[test]
    fn json_results_accept_wrapped_and_bare_arrays() {
        let wrapped = r#"{"results":[{"title":"T","url":"https://a.com/x","snippet":"S"}]}"#;
        let bare = r#"[{"title":"T","url":"https://a.com/x","description":"S"}]"#;
        let from_wrapped = parse_json_results(wrapped).unwrap();
        let from_bare = parse_json_results(bare).unwrap();
        assert_eq!(from_wrapped.len(), 1);
        assert_eq!(from_bare.len(), 1);
        assert_eq!(from_wrapped[0].title, "T");
        assert_eq!(from_wrapped[0].snippet, "S");
        assert_eq!(from_bare[0].snippet, "S");
        assert!(parse_json_results("not json").is_err());
        assert!(parse_json_results(r#"{"other":1}"#).is_err());
    }

    #[test]
    fn json_results_are_domain_filtered() {
        let results = vec![
            SearchResult {
                title: "keep".into(),
                url: "https://trusted.com/a".into(),
                snippet: String::new(),
            },
            SearchResult {
                title: "drop".into(),
                url: "https://spam.com/b".into(),
                snippet: String::new(),
            },
        ];
        let filtered = filter_results(
            results,
            &["trusted.com".to_string()],
            &["spam.com".to_string()],
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title, "keep");
    }

    #[tokio::test]
    async fn unreachable_backends_are_reported_as_execution_errors() {
        // Both endpoints fail to connect. The point of the test is the error
        // *type*: a dead backend is `Execution`, not `InvalidInput`, so callers
        // (and the model) can tell "the search backend is down" apart from
        // "you passed bad arguments".
        let tool = WebSearchTool::with_backends(vec![
            SearchBackendSpec::json("http://127.0.0.1:1/first"),
            SearchBackendSpec::json("http://127.0.0.1:2/second"),
        ]);
        let error = tool
            .execute(websearch_input("anything"), test_ctx())
            .await
            .unwrap_err();
        match error {
            ToolError::Execution { name, message } => {
                assert_eq!(name, "WebSearch");
                // Both attempts are named so the operator can see what was tried.
                assert!(message.contains("first") || message.contains("second"));
                assert!(message.contains(SEARCH_BACKENDS_ENV));
            }
            other => panic!("expected Execution, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_arguments_do_not_try_backends() {
        // Missing `query` must stay `InvalidInput` — it is the model's mistake,
        // so reporting a backend outage would be a lie.
        let tool = WebSearchTool::with_backends(vec![SearchBackendSpec::json(
            "http://127.0.0.1:1/never-contacted",
        )]);
        let input = ToolInput {
            call_id: "1".into(),
            name: "WebSearch".into(),
            arguments: serde_json::json!({}),
        };
        let error = tool.execute(input, test_ctx()).await.unwrap_err();
        assert!(matches!(error, ToolError::InvalidInput { .. }));
    }

    /// A one-shot HTTP server that answers one GET with `body`, returning its
    /// URL and, asynchronously, the request target it saw.
    ///
    /// The socket is bound from inside the runtime rather than with
    /// `std::net::TcpListener` first: registering a std (blocking) socket with
    /// the tokio runtime is unsupported and panics.
    async fn serve(body: &'static str) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}/search", listener.local_addr().expect("addr"));
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0_u8; 4096];
                let Ok(n) = stream.read(&mut buf).await else {
                    return seen;
                };
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                seen.push(
                    request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or_default()
                        .to_string(),
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
            seen
        });
        (url, server)
    }

    #[tokio::test]
    async fn json_backend_returns_real_titles_and_snippets() {
        let (url, server) = serve(
            r#"{"results":[
                {"title":"Rust polyglot","url":"https://example.com/rust","snippet":"embed C in Rust"},
                {"title":"Spam","url":"https://spam.example/x","snippet":"nope"}]}"#,
        )
        .await;
        let tool = WebSearchTool::with_backends(vec![SearchBackendSpec::json(url)]);
        let output = tool
            .execute(
                websearch_with_domains("rust c polyglot", vec![], vec!["spam.example"]),
                test_ctx(),
            )
            .await
            .unwrap()
            .output;

        assert!(
            output.contains("[Rust polyglot](https://example.com/rust)"),
            "{output}"
        );
        assert!(output.contains("embed C in Rust"), "{output}");
        // `blocked_domains` is applied to JSON backends too.
        assert!(!output.contains("spam.example"), "{output}");

        let targets = server.await.unwrap();
        // The query is sent as `?q=`, percent-encoded.
        assert_eq!(targets[0], "/search?q=rust%20c%20polyglot");
    }

    #[tokio::test]
    async fn an_unreachable_first_backend_falls_through_to_the_next() {
        let (url, server) =
            serve(r#"[{"title":"Second","url":"https://ok.example/","snippet":"worked"}]"#).await;
        let tool = WebSearchTool::with_backends(vec![
            SearchBackendSpec::json("http://127.0.0.1:1/refuses"),
            SearchBackendSpec::json(url),
        ]);
        let output = tool
            .execute(websearch_input("fallback"), test_ctx())
            .await
            .unwrap()
            .output;

        assert!(output.contains("[Second](https://ok.example/)"), "{output}");
        // The first backend was never answered; the second got the query.
        let targets = server.await.unwrap();
        assert_eq!(targets[0], "/search?q=fallback");
    }

    #[tokio::test]
    async fn a_healthy_backend_does_not_contact_later_ones() {
        let (url, server) =
            serve(r#"{"results":[{"title":"First","url":"https://first.example/","snippet":""}]}"#)
                .await;
        let tool = WebSearchTool::with_backends(vec![
            SearchBackendSpec::json(url),
            SearchBackendSpec::json("http://127.0.0.1:1/never"),
        ]);
        let output = tool
            .execute(websearch_input("first wins"), test_ctx())
            .await
            .unwrap()
            .output;
        assert!(output.contains("First"), "{output}");

        // The one-shot server accepted a single connection and is done; the
        // chain stopped at the first healthy backend and never touched the
        // second endpoint.
        let targets = server.await.unwrap();
        assert_eq!(targets, vec!["/search?q=first%20wins".to_string()]);
    }
}
