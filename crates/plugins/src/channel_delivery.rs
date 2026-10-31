//! Deliver plugin channel notifications out of the process.
//!
//! Two transports live here:
//!
//! - **Webhook** — a channel that declares `"deliver": {"url": "…"}` gets an
//!   event POSTed as JSON to that URL.
//! - **MCP tool** — a channel that declares
//!   `"deliver": {"server": "…", "tool": "…"}` gets each event passed to that
//!   tool on another MCP server as its arguments (the event object itself), so
//!   a server that already knows how to reach Slack, Jira or PagerDuty can do
//!   the delivery without a webhook or a script.
//!
//! Both share [`DeliveryStats`] and the same lossy contract below.
//!
//! Delivery is **lossy on purpose**. A webhook that is slow, hanging, or
//! returning errors must not stall `PluginManager::route_channel_notification`:
//! that call runs inside the MCP notification path, and blocking it would back
//! up every server behind one unavailable endpoint. So:
//!
//! - [`ChannelSink::deliver`] is synchronous and only enqueues; a full queue
//!   drops the event and counts it;
//! - a background task drains the queue, posting each event with a timeout;
//! - counters — delivered, dropped, failed, and the last error — are readable so
//!   `/plugin components` can show the truth instead of a claim of success.
//!
//! [`ChannelSink::deliver`]: crate::plugins::ChannelSink::deliver
//! [`PluginManager::route_channel_notification`]: crate::plugins::PluginManager::route_channel_notification

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::plugins::{ChannelSink, PluginChannelEvent};

/// How many undelivered events one webhook may hold.
pub const WEBHOOK_QUEUE_CAPACITY: usize = 256;

/// How long one POST may take before it is abandoned.
pub const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// What a webhook sink has done so far.
#[derive(Debug, Default)]
pub struct DeliveryStats {
    delivered: AtomicU64,
    dropped: AtomicU64,
    failed: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl DeliveryStats {
    /// Events the endpoint accepted.
    #[must_use]
    pub fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::Relaxed)
    }

    /// Events dropped because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Events the endpoint refused, or the POST failed.
    #[must_use]
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Why the most recent delivery failed, when one did.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record_error(&self, error: String) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
    }

    /// One line for a report: counts, and the last error when there is one.
    #[must_use]
    pub fn render(&self) -> String {
        let mut line = format!(
            "delivered {}, dropped {}, failed {}",
            self.delivered(),
            self.dropped(),
            self.failed()
        );
        if let Some(error) = self.last_error() {
            use std::fmt::Write as _;
            let _ = write!(line, "; last error: {error}");
        }
        line
    }
}

/// A sink that POSTs each event to a URL.
#[derive(Debug)]
pub struct WebhookChannelSink {
    sender: tokio::sync::mpsc::Sender<PluginChannelEvent>,
    stats: Arc<DeliveryStats>,
    url: String,
}

impl WebhookChannelSink {
    /// Create a sink and start its delivery task.
    ///
    /// Must be called from within a Tokio runtime: the task lives on it and
    /// ends when the sink (and every sender clone) is dropped.
    #[must_use]
    pub fn spawn(url: impl Into<String>) -> Arc<Self> {
        let url = url.into();
        let (sender, mut receiver) =
            tokio::sync::mpsc::channel::<PluginChannelEvent>(WEBHOOK_QUEUE_CAPACITY);
        let stats = Arc::new(DeliveryStats::default());
        let task_stats = Arc::clone(&stats);
        let task_url = url.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(WEBHOOK_TIMEOUT)
                .build()
                .unwrap_or_default();
            while let Some(event) = receiver.recv().await {
                match client.post(&task_url).json(&event).send().await {
                    Ok(response) if response.status().is_success() => {
                        task_stats.delivered.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(response) => {
                        task_stats.failed.fetch_add(1, Ordering::Relaxed);
                        task_stats.record_error(format!("endpoint answered {}", response.status()));
                    }
                    Err(error) => {
                        task_stats.failed.fetch_add(1, Ordering::Relaxed);
                        task_stats.record_error(error.to_string());
                    }
                }
            }
        });
        Arc::new(Self { sender, stats, url })
    }

    /// The URL events are posted to.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The counters this sink keeps.
    #[must_use]
    pub const fn stats(&self) -> &Arc<DeliveryStats> {
        &self.stats
    }
}

impl ChannelSink for WebhookChannelSink {
    fn deliver(&self, event: PluginChannelEvent) {
        // `try_send`, not `send`: this runs on the MCP notification path, and
        // waiting for a slow endpoint there would stall every server behind it.
        if self.sender.try_send(event).is_err() {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Calls a tool on an MCP server.
///
/// The plugin crate cannot depend on the MCP crate (it is the other way
/// round), so the CLI implements this over its `McpManager` and hands it to
/// [`McpToolChannelSink`].
#[async_trait::async_trait]
pub trait ToolCaller: Send + Sync + std::fmt::Debug {
    /// Call `tool` on `server` with `input` as its arguments.
    ///
    /// `Err` means the call did not succeed; the event is counted as failed
    /// and the reason is kept for the report.
    async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        input: serde_json::Value,
    ) -> Result<(), String>;
}

/// How long one tool delivery may take.
pub const TOOL_DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// A sink that passes each event to a tool on another MCP server.
///
/// The event object is sent verbatim as the tool's arguments: the target tool
/// declares the shape it accepts, and the channel's `method`/`params` are part
/// of it. A server that wants a different shape can expose a thin tool that
/// reshapes what it receives.
#[derive(Debug)]
pub struct McpToolChannelSink {
    sender: tokio::sync::mpsc::Sender<PluginChannelEvent>,
    stats: Arc<DeliveryStats>,
    server: String,
    tool: String,
}

impl McpToolChannelSink {
    /// Create a sink and start its delivery task.
    ///
    /// Must be called from within a Tokio runtime.
    #[must_use]
    pub fn spawn(
        server: impl Into<String>,
        tool: impl Into<String>,
        caller: Arc<dyn ToolCaller>,
    ) -> Arc<Self> {
        let server = server.into();
        let tool = tool.into();
        let (sender, mut receiver) =
            tokio::sync::mpsc::channel::<PluginChannelEvent>(WEBHOOK_QUEUE_CAPACITY);
        let stats = Arc::new(DeliveryStats::default());
        let task_stats = Arc::clone(&stats);
        let task_server = server.clone();
        let task_tool = tool.clone();
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                let Ok(input) = serde_json::to_value(&event) else {
                    task_stats.failed.fetch_add(1, Ordering::Relaxed);
                    task_stats.record_error("the event could not be serialised".to_string());
                    continue;
                };
                match tokio::time::timeout(
                    TOOL_DELIVERY_TIMEOUT,
                    caller.call_tool(&task_server, &task_tool, input),
                )
                .await
                {
                    Ok(Ok(())) => {
                        task_stats.delivered.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Err(error)) => {
                        task_stats.failed.fetch_add(1, Ordering::Relaxed);
                        task_stats.record_error(error);
                    }
                    Err(_) => {
                        task_stats.failed.fetch_add(1, Ordering::Relaxed);
                        task_stats.record_error(format!(
                            "{}#{} did not answer within {}s",
                            task_server,
                            task_tool,
                            TOOL_DELIVERY_TIMEOUT.as_secs()
                        ));
                    }
                }
            }
        });
        Arc::new(Self {
            sender,
            stats,
            server,
            tool,
        })
    }

    /// The server and tool events are delivered to.
    #[must_use]
    pub fn target(&self) -> (&str, &str) {
        (&self.server, &self.tool)
    }

    /// The counters this sink keeps.
    #[must_use]
    pub const fn stats(&self) -> &Arc<DeliveryStats> {
        &self.stats
    }
}

impl ChannelSink for McpToolChannelSink {
    fn deliver(&self, event: PluginChannelEvent) {
        if self.sender.try_send(event).is_err() {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sequence: u64) -> PluginChannelEvent {
        PluginChannelEvent {
            channel: "ops".into(),
            plugin: "p".into(),
            server: "s".into(),
            method: "notifications/resources/updated".into(),
            params: Some(serde_json::json!({"uri": "file:///a"})),
            sequence,
        }
    }

    /// A loopback HTTP server that records each body and answers `status`.
    #[cfg(unix)]
    async fn receiver(
        status: u16,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let address = listener.local_addr().expect("local address");
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&bodies);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                    let mut buffer = vec![0_u8; 8192];
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    let text = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    if let Some((_, body)) = text.split_once("\r\n\r\n") {
                        recorder.lock().unwrap().push(body.to_string());
                    }
                    let response = format!(
                        "HTTP/1.1 {status} {}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        if status < 300 { "OK" } else { "Error" }
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://{address}/hook"), bodies, handle)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_event_is_posted_as_json() {
        let (url, bodies, server) = receiver(200).await;
        let sink = WebhookChannelSink::spawn(url);
        sink.deliver(event(7));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while bodies.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();

        let body = bodies.lock().unwrap()[0].clone();
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("JSON body");
        assert_eq!(parsed["channel"], "ops");
        assert_eq!(parsed["method"], "notifications/resources/updated");
        assert_eq!(parsed["sequence"], 7);
        assert_eq!(parsed["params"]["uri"], "file:///a");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_successful_delivery_is_counted() {
        let (url, bodies, server) = receiver(200).await;
        let sink = WebhookChannelSink::spawn(url);
        sink.deliver(event(1));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while sink.stats().delivered() == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();
        assert_eq!(sink.stats().delivered(), 1);
        assert_eq!(sink.stats().failed(), 0);
        assert_eq!(sink.stats().dropped(), 0);
        assert!(sink.stats().last_error().is_none());
        assert_eq!(bodies.lock().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_delivery_is_counted_with_its_status() {
        let (url, _bodies, server) = receiver(500).await;
        let sink = WebhookChannelSink::spawn(url);
        sink.deliver(event(1));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while sink.stats().failed() == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();
        assert_eq!(sink.stats().failed(), 1);
        assert!(
            sink.stats()
                .last_error()
                .is_some_and(|error| error.contains("500")),
            "{:?}",
            sink.stats().last_error()
        );
        assert!(sink.stats().render().contains("failed 1"));
    }

    /// A full queue drops rather than blocks: `deliver` runs on the
    /// notification path and must return immediately.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_full_queue_drops_and_counts() {
        // Nothing listens on port 9 in a test container, so the task stalls on
        // connection errors while the queue fills.
        let sink = WebhookChannelSink::spawn("http://127.0.0.1:9/hook");
        for sequence in 0..(WEBHOOK_QUEUE_CAPACITY as u64 + 50) {
            sink.deliver(event(sequence));
        }
        assert!(
            sink.stats().dropped() > 0,
            "a full queue should have dropped events, stats: {}",
            sink.stats().render()
        );
    }

    /// Records every call and answers with a fixed result.
    #[derive(Debug)]
    struct RecordingCaller {
        calls: Mutex<Vec<(String, String, serde_json::Value)>>,
        outcome: Result<(), String>,
        delay: Duration,
    }

    impl RecordingCaller {
        fn new(outcome: Result<(), String>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                outcome,
                delay: Duration::ZERO,
            })
        }

        fn calls(&self) -> Vec<(String, String, serde_json::Value)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ToolCaller for RecordingCaller {
        async fn call_tool(
            &self,
            server: &str,
            tool: &str,
            input: serde_json::Value,
        ) -> Result<(), String> {
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.calls
                .lock()
                .unwrap()
                .push((server.to_string(), tool.to_string(), input));
            self.outcome.clone()
        }
    }

    #[tokio::test]
    async fn a_tool_delivery_passes_the_event_as_arguments() {
        let caller = RecordingCaller::new(Ok(()));
        let sink = McpToolChannelSink::spawn("pager", "raise", caller.clone());
        assert_eq!(sink.target(), ("pager", "raise"));
        sink.deliver(event(3));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while caller.calls().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let calls = caller.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "pager");
        assert_eq!(calls[0].1, "raise");
        assert_eq!(calls[0].2["channel"], "ops");
        assert_eq!(calls[0].2["sequence"], 3);
        assert_eq!(calls[0].2["params"]["uri"], "file:///a");
        assert_eq!(sink.stats().delivered(), 1);
    }

    #[tokio::test]
    async fn a_failed_tool_delivery_is_counted_with_its_reason() {
        let caller = RecordingCaller::new(Err("tool returned isError".to_string()));
        let sink = McpToolChannelSink::spawn("pager", "raise", caller);
        sink.deliver(event(1));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while sink.stats().failed() == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(sink.stats().failed(), 1);
        assert!(
            sink.stats()
                .last_error()
                .is_some_and(|error| error.contains("isError")),
            "{:?}",
            sink.stats().last_error()
        );
    }

    /// A tool that never answers is counted as a failure, not left pending.
    #[tokio::test]
    async fn a_hanging_tool_delivery_times_out() {
        let caller = Arc::new(RecordingCaller {
            calls: Mutex::new(Vec::new()),
            outcome: Ok(()),
            // Longer than TOOL_DELIVERY_TIMEOUT, but the test does not wait for
            // the sleep: the timeout fires first.
            delay: TOOL_DELIVERY_TIMEOUT + Duration::from_secs(30),
        });
        let sink = McpToolChannelSink::spawn("pager", "raise", caller);
        sink.deliver(event(1));

        let deadline = std::time::Instant::now() + TOOL_DELIVERY_TIMEOUT + Duration::from_secs(5);
        while sink.stats().failed() == 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(sink.stats().failed(), 1);
        assert!(
            sink.stats()
                .last_error()
                .is_some_and(|error| error.contains("did not answer")),
            "{:?}",
            sink.stats().last_error()
        );
    }

    #[test]
    fn the_url_is_reported() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let sink = WebhookChannelSink::spawn("https://hooks.example.com/x");
            assert_eq!(sink.url(), "https://hooks.example.com/x");
        });
    }
}
