//! Chrome DevTools Protocol (CDP) client.
//!
//! Connects to Chrome's remote debugging endpoint (default port 9222)
//! via HTTP for discovery and WebSocket for commands.

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const DEFAULT_CDP_URL: &str = "http://localhost:9222";
const WS_TIMEOUT: Duration = Duration::from_secs(30);

/// A tab returned by the CDP HTTP endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct CdpTab {
    /// Tab identifier.
    pub id: String,
    /// Page title.
    pub title: String,
    /// Current URL.
    pub url: String,
    /// WebSocket debugger URL for this tab.
    #[serde(rename = "webSocketDebuggerUrl")]
    pub ws_url: Option<String>,
}

/// A Chrome CDP client.
///
/// The client is cheap to clone and shared by every Chrome tool in a session.
/// It remembers which tab the session is driving, because making the model
/// thread a `ws_url` through every call meant one extra round trip per step and
/// a wrong target the moment it copied the wrong id. An explicit `ws_url` on a
/// tool argument still wins, so a model that wants to pin a specific tab can.
#[derive(Debug, Clone)]
pub struct ChromeClient {
    http_client: reqwest::Client,
    cdp_url: String,
    /// The tab the session is currently driving, if any.
    current_tab: Arc<tokio::sync::RwLock<Option<String>>>,
}

impl ChromeClient {
    /// Create a new client connecting to the default CDP endpoint.
    #[must_use]
    pub fn new() -> Self {
        Self {
            http_client: reqwest::Client::new(),
            cdp_url: DEFAULT_CDP_URL.into(),
            current_tab: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// Create a client with a custom CDP endpoint.
    #[must_use]
    pub fn with_endpoint(cdp_url: impl Into<String>) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            cdp_url: cdp_url.into(),
            current_tab: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// The tab the session is driving, if one has been selected.
    pub async fn current_tab(&self) -> Option<String> {
        self.current_tab.read().await.clone()
    }

    /// Pin the session to a specific tab.
    pub async fn select_tab(&self, ws_url: impl Into<String>) {
        *self.current_tab.write().await = Some(ws_url.into());
    }

    /// Resolve the tab a command should go to.
    ///
    /// An explicit `ws_url` always wins — a model that names a target means it.
    /// Otherwise the remembered tab is used, and failing that the first tab
    /// Chrome reports, so a session that never called `navigate` can still act
    /// on whatever the person already has open.
    async fn resolve_tab(&self, explicit: Option<&str>) -> Result<String, ChromeError> {
        if let Some(ws_url) = explicit.filter(|ws_url| !ws_url.is_empty()) {
            return Ok(ws_url.to_string());
        }
        if let Some(current) = self.current_tab().await {
            return Ok(current);
        }
        let tabs = self.list_tabs().await?;
        let tab = tabs
            .into_iter()
            .find(|tab| tab.ws_url.is_some())
            .ok_or(ChromeError::NoWsUrl)?;
        let ws_url = tab.ws_url.ok_or(ChromeError::NoWsUrl)?;
        self.select_tab(ws_url.clone()).await;
        Ok(ws_url)
    }

    /// Check whether Chrome's remote debugging is reachable.
    pub async fn is_connected(&self) -> bool {
        self.http_client
            .get(format!("{}/json/version", self.cdp_url))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok()
    }

    /// List all open tabs/pages.
    pub async fn list_tabs(&self) -> Result<Vec<CdpTab>, ChromeError> {
        let resp = self
            .http_client
            .get(format!("{}/json/list", self.cdp_url))
            .send()
            .await
            .map_err(|e| ChromeError::Http(e.to_string()))?;

        let tabs: Vec<CdpTab> = resp
            .json()
            .await
            .map_err(|e| ChromeError::Http(format!("failed to parse tab list: {e}")))?;
        Ok(tabs)
    }

    /// Navigate the session's tab to a URL.
    ///
    /// When the session already has a tab, this drives that tab with
    /// `Page.navigate` rather than opening another one: the old behaviour used
    /// `PUT /json/new`, which leaves every earlier tab open and strands the
    /// cookies and login state the person had already established. A fresh tab
    /// is opened only when there is nothing to drive, and its URL is opened
    /// blank-then-navigated so the same code path applies either way.
    ///
    /// This method *is* the tab selection: after it returns, the client's
    /// current tab is the one showing `url`.
    pub async fn navigate(&self, url: &str) -> Result<String, ChromeError> {
        if let Some(ws_url) = self.current_tab().await {
            let params = serde_json::json!({ "url": url });
            match self
                .send_cdp_command(&ws_url, "Page.navigate", params)
                .await
            {
                Ok(_) => return Ok(ws_url),
                // The remembered tab was closed under us. Fall through and open
                // a fresh one rather than failing the navigation.
                Err(ChromeError::WebSocket(_) | ChromeError::Timeout) => {
                    *self.current_tab.write().await = None;
                }
                Err(e) => return Err(e),
            }
        }

        let resp = self
            .http_client
            .put(format!("{}/json/new?{}", self.cdp_url, url))
            .send()
            .await
            .map_err(|e| ChromeError::Http(e.to_string()))?;

        let tab: CdpTab = resp
            .json()
            .await
            .map_err(|e| ChromeError::Http(format!("failed to parse new tab: {e}")))?;

        let ws_url = tab.ws_url.ok_or(ChromeError::NoWsUrl)?;
        self.select_tab(ws_url.clone()).await;
        Ok(ws_url)
    }

    /// Execute JavaScript in the session's tab and return the result.
    pub async fn execute_js(
        &self,
        ws_url: Option<&str>,
        expression: &str,
    ) -> Result<Value, ChromeError> {
        let tab = self.resolve_tab(ws_url).await?;
        let params = serde_json::json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true,
        });
        self.send_cdp_command(&tab, "Runtime.evaluate", params)
            .await
    }

    /// Capture a screenshot of the session's tab as base64-encoded PNG bytes.
    pub async fn screenshot(&self, ws_url: Option<&str>) -> Result<String, ChromeError> {
        let tab = self.resolve_tab(ws_url).await?;
        let params = serde_json::json!({
            "format": "png",
            "fromSurface": true,
        });
        let result = self
            .send_cdp_command(&tab, "Page.captureScreenshot", params)
            .await?;

        result
            .get("data")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| ChromeError::NoResult {
                message: "screenshot data not found".into(),
            })
    }

    /// Get the full HTML content of the session's tab.
    pub async fn get_content(&self, ws_url: Option<&str>) -> Result<String, ChromeError> {
        let result = self
            .execute_js(ws_url, "document.documentElement.outerHTML")
            .await?;

        result
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| ChromeError::NoResult {
                message: "document content not found".into(),
            })
    }

    /// Click the element matching a CSS selector by scrolling it into view and
    /// dispatching real mouse events at its centre.
    ///
    /// Real events (rather than `element.click()`) are what make the click
    /// reach handlers that check `isTrusted`, and what make a click on a link
    /// navigate the page. Returns the selector that was clicked, so the caller
    /// can report what it acted on.
    pub async fn click(&self, ws_url: Option<&str>, selector: &str) -> Result<String, ChromeError> {
        let tab = self.resolve_tab(ws_url).await?;
        let centre = self.element_centre(&tab, selector).await?;
        let (x, y) = (centre.0, centre.1);
        for event_type in ["mouseMoved", "mousePressed", "mouseReleased"] {
            let params = serde_json::json!({
                "type": event_type,
                "x": x,
                "y": y,
                "button": "left",
                "clickCount": 1,
            });
            self.send_cdp_command(&tab, "Input.dispatchMouseEvent", params)
                .await?;
        }
        Ok(selector.to_string())
    }

    /// Type text into the element matching a CSS selector.
    ///
    /// Focuses the element first, then inserts the text with
    /// `Input.insertText`, which fires the same input events a paste does —
    /// the path that reaches React-controlled inputs, where setting `.value`
    /// directly is silently discarded.
    pub async fn type_text(
        &self,
        ws_url: Option<&str>,
        selector: &str,
        text: &str,
    ) -> Result<(), ChromeError> {
        let tab = self.resolve_tab(ws_url).await?;
        let escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
        let focus = format!(
            "(function(){{const el=document.querySelector('{escaped}');\
             if(!el) return false; el.focus(); return true;}})()"
        );
        let result = self
            .execute_js(Some(&tab), &focus)
            .await?
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !result {
            return Err(ChromeError::NoResult {
                message: format!("no element matched selector {selector}"),
            });
        }
        let params = serde_json::json!({ "text": text });
        self.send_cdp_command(&tab, "Input.insertText", params)
            .await
            .map(|_| ())
    }

    /// Scroll the session's tab.
    pub async fn scroll(&self, ws_url: Option<&str>, delta_y: f64) -> Result<Value, ChromeError> {
        self.execute_js(
            ws_url,
            &format!("window.scrollBy(0, {delta_y}); window.scrollY"),
        )
        .await
    }

    /// Wait until the page reports it is done loading.
    ///
    /// A navigation resolves before the page has painted anything useful, so a
    /// screenshot taken right after `navigate` catches a blank document. This
    /// polls `document.readyState` rather than sleeping a fixed time, so a fast
    /// page is not slowed down and a slow one is not cut short.
    pub async fn wait_for_load(&self, ws_url: Option<&str>) -> Result<(), ChromeError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let result = self
                .execute_js(ws_url, "document.readyState")
                .await?
                .get("result")
                .and_then(|r| r.get("value"))
                .and_then(Value::as_str)
                .map(String::from);
            if result.as_deref() == Some("complete") {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ChromeError::Timeout);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The viewport centre of the first element matching `selector`.
    ///
    /// Scrolls the element into view first, so a click lands on a below-the-fold
    /// button without the caller having to scroll by hand.
    async fn element_centre(&self, tab: &str, selector: &str) -> Result<(f64, f64), ChromeError> {
        let escaped = selector.replace('\\', "\\\\").replace('\'', "\\'");
        let script = format!(
            "(function(){{const el=document.querySelector('{escaped}');\
             if(!el) return null; el.scrollIntoView({{block:'center'}});\
             const r=el.getBoundingClientRect();\
             return {{x:r.left+r.width/2, y:r.top+r.height/2}};}})()"
        );
        let value = self
            .execute_js(Some(tab), &script)
            .await?
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null);
        let x = value.get("x").and_then(Value::as_f64);
        let y = value.get("y").and_then(Value::as_f64);
        match (x, y) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err(ChromeError::NoResult {
                message: format!("no element matched selector {selector}"),
            }),
        }
    }

    /// Send a CDP command over WebSocket and wait for the response.
    async fn send_cdp_command(
        &self,
        ws_url: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, ChromeError> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| ChromeError::WebSocket(e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        let request = serde_json::json!({
            "id": 1,
            "method": method,
            "params": params,
        });

        let msg = WsMessage::Text(request.to_string().into());
        futures_util::SinkExt::send(&mut write, msg).await.map_err(
            |e: tokio_tungstenite::tungstenite::Error| ChromeError::WebSocket(e.to_string()),
        )?;

        let response = tokio::time::timeout(WS_TIMEOUT, async {
            while let Some(Ok(msg)) = futures_util::StreamExt::next(&mut read).await {
                if let WsMessage::Text(text) = msg
                    && let Ok(value) = serde_json::from_str::<Value>(&text)
                    && value.get("id").is_some()
                {
                    return Some(value);
                }
            }
            None
        })
        .await
        .map_err(|_| ChromeError::Timeout)?
        .ok_or_else(|| ChromeError::NoResult {
            message: "no response from Chrome".into(),
        })?;

        // Gracefully close the WebSocket
        let _ = futures_util::SinkExt::close(&mut write).await;

        if let Some(error) = response.get("error") {
            return Err(ChromeError::Cdp {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(-1),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown CDP error")
                    .into(),
            });
        }

        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }
}

impl Default for ChromeClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors from Chrome CDP operations.
#[derive(Debug, thiserror::Error)]
pub enum ChromeError {
    /// HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(String),
    /// WebSocket communication failed.
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    /// CDP command returned an error.
    #[error("CDP error {code}: {message}")]
    Cdp {
        /// Error code.
        code: i64,
        /// Error message.
        message: String,
    },
    /// No WebSocket debugger URL available.
    #[error("no WebSocket debugger URL")]
    NoWsUrl,
    /// Expected result not found in response.
    #[error("missing result: {message}")]
    NoResult {
        /// Description of what was missing.
        message: String,
    },
    /// Operation timed out.
    #[error("operation timed out")]
    Timeout,
}
