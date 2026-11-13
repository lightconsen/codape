//! Tracing initialization and panic hook installation.

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize the tracing subscriber.
///
/// Controls:
/// - `CODAPE_LOG` — log level filter (default: `off`)
/// - `CODAPE_LOG_FORMAT` — output format: `pretty` (default) or `json`
/// - `CODAPE_LOG_FILE` — optional file path for additional file output
pub fn init_tracing() {
    let filter = EnvFilter::try_from_env("CODAPE_LOG").unwrap_or_else(|_| EnvFilter::new("off"));

    let format_json = std::env::var("CODAPE_LOG_FORMAT").is_ok_and(|v| v == "json");

    if format_json {
        tracing_subscriber::registry()
            .with(filter)
            .with(
                fmt::layer()
                    .json()
                    .with_target(false)
                    .with_writer(std::io::stderr),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_target(false).with_writer(std::io::stderr))
            .init();
    }
}

/// Install a panic hook using the telemetry crash reporter.
///
/// Crash reports are written to `~/.codape/crashes/pending/` and ingested
/// into the telemetry store on the next startup.
pub fn install_panic_hook() {
    let pending_dir = codape_config::config_home().join("crashes").join("pending");
    let reporter = codape_telemetry::CrashReporter::new(pending_dir);
    reporter.install_hook();

    // Release builds use `panic = "abort"`, so no destructor runs after a
    // panic. Everything that would normally put the process back the way it
    // found it therefore has to happen here, in order:
    //
    // 1. leave the alternate screen and raw mode, or the panic text is
    //    written into a hidden buffer and the user's shell is left unusable;
    // 2. kill plugin monitors, which would otherwise outlive us;
    // 3. let the telemetry hook and the default hook print the message.
    //
    // There is no step that writes the transcript here. The engine owns the
    // session and is moved into the turn task for the duration of a turn, so
    // the hook holds no handle to it; the transcript is also already on disk,
    // rewritten at every completed tool call and at least once a second while
    // text streams. A turn cut short by a panic therefore loses at most the
    // last second of output, not the session.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Only while the interactive TUI owns the terminal. A headless run
        // also panics with stdout attached to a terminal, and it never entered
        // raw mode or the alternate screen — writing the leave sequences there
        // would pop a keyboard-enhancement level that was never pushed.
        if crate::repl::TUI_ACTIVE.load(std::sync::atomic::Ordering::SeqCst) {
            codape_ui::terminal::restore_after_panic();
        }
        crate::plugin_monitors::terminate_all_monitors();
        previous(info);
    }));
}
