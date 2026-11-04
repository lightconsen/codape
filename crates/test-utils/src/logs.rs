//! Capture `tracing` events produced during a test.
//!
//! Some behavior is only observable in a log line: "this hook will never run",
//! "the response named a request nothing is waiting on", "the path entry was
//! dropped because it was relative". Without a way to assert on the message, a
//! mutation that deletes the log call survives the test suite, which is what
//! `docs/mutation.md` records.
//!
//! The capture is installed with [`tracing::subscriber::set_default`], so it
//! is scoped to the current thread and does not race other tests in the same
//! process. Spawned threads need their own capture; [`capture_logs_async`]
//! covers the common case of a future polled on a current-thread runtime.
//!
//! One process-wide detail is not per-thread: `tracing` caches each callsite's
//! interest the first time any thread reaches it, and does so with the
//! registering thread's dispatcher when only one is live. A first touch from a
//! thread without a capture would then cache "disabled" for everyone, so a
//! permanent inert dispatcher is kept alive (see `pin_dispatcher`).

use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};

/// One captured event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvent {
    /// The event level.
    pub level: Level,
    /// The module that logged it.
    pub target: String,
    /// The `message` field, when the event has one.
    pub message: String,
    /// Every other field, rendered as `key=value`, separated by spaces.
    pub fields: String,
}

impl LogEvent {
    /// The message and the other fields, for substring assertions.
    #[must_use]
    pub fn text(&self) -> String {
        if self.fields.is_empty() {
            self.message.clone()
        } else {
            format!("{} {}", self.message, self.fields)
        }
    }
}

/// Collects events into a shared buffer.
#[derive(Default)]
struct Collector {
    events: Arc<Mutex<Vec<LogEvent>>>,
}

impl<S: Subscriber> Layer<S> for Collector {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let recorded = LogEvent {
            level: *metadata.level(),
            target: metadata.target().to_string(),
            message: visitor.message,
            fields: visitor.fields.join(" "),
        };
        if let Ok(mut events) = self.events.lock() {
            events.push(recorded);
        }
    }

    fn on_new_span(&self, _attrs: &Attributes<'_>, _id: &Id, _context: Context<'_, S>) {}
    fn on_record(&self, _span: &Id, _values: &Record<'_>, _context: Context<'_, S>) {}
    fn enabled(&self, _metadata: &Metadata<'_>, _context: Context<'_, S>) -> bool {
        true
    }
}

#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: Vec<String>,
}

impl FieldVisitor {
    /// Record one field. An event may carry an explicit field literally named
    /// `message` as well as the implicit format message; keep both rather than
    /// letting whichever was recorded last win.
    fn record(&mut self, field: &Field, debug: String, display: String) {
        if field.name() == "message" && self.message.is_empty() {
            self.message = display;
        } else {
            self.fields.push(format!("{}={debug}", field.name()));
        }
    }
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        self.record(field, rendered.clone(), rendered);
    }
}

/// A live capture. Drop the guard (or let it fall out of scope) to stop
/// capturing on this thread.
pub struct LogCapture {
    events: Arc<Mutex<Vec<LogEvent>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

impl std::fmt::Debug for LogCapture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `DefaultGuard` has no `Debug`; the captured events are what a
        // failing assertion wants to print anyway.
        formatter
            .debug_struct("LogCapture")
            .field("events", &self.events())
            .finish()
    }
}

impl LogCapture {
    /// Everything captured so far.
    #[must_use]
    pub fn events(&self) -> Vec<LogEvent> {
        self.events
            .lock()
            .map(|events| events.clone())
            .unwrap_or_default()
    }

    /// The messages captured so far, in order.
    #[must_use]
    pub fn messages(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .map(|event| event.text())
            .collect()
    }

    /// Whether any captured message (with its other fields) contains `needle`.
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        self.messages()
            .iter()
            .any(|message| message.contains(needle))
    }

    /// Whether any captured event is at `level` and contains `needle`.
    #[must_use]
    pub fn contains_at(&self, level: Level, needle: &str) -> bool {
        self.events()
            .iter()
            .any(|event| event.level == level && event.text().contains(needle))
    }

    /// Forget everything captured so far.
    pub fn clear(&self) {
        if let Ok(mut events) = self.events.lock() {
            events.clear();
        }
    }
}

/// A subscriber that captures nothing and enables everything.
///
/// It exists only to be a second live dispatcher; see [`pin_dispatcher`].
#[derive(Debug)]
struct Pin;

impl Subscriber for Pin {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, _event: &Event<'_>) {}

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// Hold one inert dispatcher alive for the whole process.
///
/// `tracing` caches each callsite's [`Interest`] process-wide the first time
/// any thread reaches it, and with exactly one live dispatcher it asks *that
/// thread's* dispatcher rather than every live one. A test thread that has no
/// capture would then register the callsite as disabled, and a capture
/// installed afterwards would never see it — the event is lost even though
/// `rebuild_interest_cache` runs when the capture starts, because the
/// callsite is not registered yet at that point.
///
/// A second, permanent dispatcher moves tracing onto the path that consults
/// every live dispatcher, so a first touch from any thread leaves the
/// callsite enabled and the event reaches whichever capture is current on
/// the logging thread.
fn pin_dispatcher() {
    static PIN: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
    PIN.get_or_init(|| tracing::Dispatch::new(Pin));
}

/// Start capturing `tracing` events on the current thread.
///
/// The default dispatcher is replaced for the lifetime of the returned guard,
/// so this must not be combined with another [`tracing::subscriber::set_default`]
/// in the same scope.
#[must_use]
pub fn capture_logs() -> LogCapture {
    // Before the capture is installed, so the dispatcher count never passes
    // through one.
    pin_dispatcher();
    let events = Arc::new(Mutex::new(Vec::new()));
    let collector = Collector {
        events: Arc::clone(&events),
    };
    let subscriber = tracing_subscriber::registry().with(collector);
    let guard = tracing::subscriber::set_default(subscriber);
    // `set_default` installs a thread-local dispatcher but does not rebuild
    // the process-wide callsite interest cache. If another test thread has
    // already hit this callsite under the no-op dispatcher, the cache can say
    // "disabled" and this capture would see nothing. Rebuilding asks every
    // callsite about the dispatcher now active on this thread.
    tracing::callsite::rebuild_interest_cache();
    LogCapture {
        events,
        _guard: guard,
    }
}

/// Run `future` to completion on a current-thread runtime while capturing
/// logs, returning its output and the captured events.
///
/// A current-thread runtime keeps the future on this thread, so the scoped
/// dispatcher applies to it. Code that spawns work onto a multi-thread
/// runtime needs its own capture around the worker.
pub fn capture_logs_async<F: std::future::Future>(future: F) -> (F::Output, Vec<LogEvent>) {
    let capture = capture_logs();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime");
    let output = runtime.block_on(future);
    let events = capture.events();
    (output, events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_logged_message_is_captured_with_its_fields() {
        let capture = capture_logs();
        tracing::warn!(tool = "read_file", "the tool name is not unique");
        assert!(capture.contains("the tool name is not unique"));
        assert!(capture.contains("tool=read_file") || capture.contains("tool=\"read_file\""));
        let events = capture.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, Level::WARN);
    }

    #[test]
    fn the_level_filter_matches_only_that_level() {
        let capture = capture_logs();
        tracing::info!("an informational line");
        assert!(!capture.contains_at(Level::ERROR, "informational"));
        assert!(capture.contains_at(Level::INFO, "informational"));
    }

    #[test]
    fn clear_drops_what_was_captured() {
        let capture = capture_logs();
        tracing::info!("first");
        capture.clear();
        assert!(!capture.contains("first"));
    }

    #[test]
    fn captures_do_not_lose_events_under_thread_contention() {
        use std::sync::{Arc, Barrier};

        for round in 0..20 {
            let barrier = Arc::new(Barrier::new(8));
            let handles: Vec<_> = (0..8)
                .map(|worker| {
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        let capture = capture_logs();
                        probe_first_touch(worker);
                        let found = capture.contains("thread contention probe");
                        (worker, found)
                    })
                })
                .collect();
            for handle in handles {
                let (worker, found) = handle.join().expect("worker");
                assert!(found, "round {round} worker {worker} lost the event");
            }
        }
    }

    /// First touched here on purpose: a callsite's interest is cached the
    /// first time any thread reaches it, and that thread may not be the one
    /// running a capture.
    #[inline(never)]
    fn probe_first_touch(worker: usize) {
        tracing::info!(worker, "thread contention probe");
    }

    /// Its own callsite, so the test above cannot have registered it first.
    #[inline(never)]
    fn probe_foreign_first_touch() {
        tracing::info!("foreign first-touch probe");
    }

    #[test]
    fn a_first_touch_from_another_thread_does_not_disable_a_live_capture() {
        let capture = capture_logs();
        // A thread with no capture of its own touches `probe_foreign_first_touch`
        // for the first time in the process. Registering that callsite must
        // still leave the capture on this thread able to see it.
        std::thread::spawn(probe_foreign_first_touch)
            .join()
            .expect("the registering thread");
        probe_foreign_first_touch();
        assert!(
            capture.contains("foreign first-touch probe"),
            "the capture saw nothing: {:?}",
            capture.messages()
        );
    }

    #[test]
    fn the_async_helper_captures_events_from_the_future() {
        let (value, events) = capture_logs_async(async {
            tracing::warn!("from the future");
            7
        });
        assert_eq!(value, 7);
        assert!(
            events
                .iter()
                .any(|event| event.message.contains("from the future")),
            "{events:?}"
        );
    }
}
