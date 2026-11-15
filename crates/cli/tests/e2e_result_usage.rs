//! What a `result` frame tells a host about what the run cost.
//!
//! The `result` frame is the turn-complete signal, and for a benchmark or a
//! billed deployment the usage it carries is the only per-run record of what
//! the run spent. It was missing for the whole of 0.2.x — `ResultFrame.data` is
//! a flattened map, so nothing failed when the field was never written; hosts
//! read a key no producer emitted and rendered an empty money column.
//!
//! These run the real binary against a provider whose SSE carries
//! `usage: {input_tokens: 10, output_tokens: 20}` on every message, so the
//! numbers below are what the engine actually received, not what a mock was
//! handed.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/cli has a parent")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn start_static_provider() -> (Child, String) {
    let responses = workspace_root().join("tests/responses.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_static-anthropic-server"))
        .args([
            "--port",
            "0",
            "--responses",
            responses.to_str().expect("responses path"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn static provider");

    let stderr = child.stderr.take().expect("server stderr");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut announced = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if !announced && let Some(address) = line.split("listening on ").nth(1) {
                let _ = tx.send(address.trim().to_string());
                announced = true;
            }
        }
    });
    let address = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("static provider did not announce its address");
    (child, format!("http://{address}"))
}

/// Run one headless turn and return its `result` frame.
///
/// `pricing` is written to `pricing.json` in a throwaway config home when
/// given, which is the only supported way to tell the binary what a model
/// costs.
fn result_frame(model: &str, pricing: Option<&str>) -> serde_json::Value {
    let (mut provider, base_url) = start_static_provider();
    let workspace = tempfile::tempdir().expect("workspace");
    let config_home = tempfile::tempdir().expect("config home");
    std::fs::write(
        config_home.path().join("settings.json"),
        r#"{"config_version": 4, "telemetry_enabled": false}"#,
    )
    .expect("settings");
    if let Some(pricing) = pricing {
        std::fs::write(config_home.path().join("pricing.json"), pricing).expect("pricing");
    }

    let output = run_stream_json(workspace.path(), config_home.path(), &base_url, model);
    let _ = provider.kill();
    let _ = provider.wait();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|frame| frame["type"] == "result")
        .unwrap_or_else(|| panic!("no result frame:\n{stdout}"))
}

fn run_stream_json(
    workspace: &Path,
    config_home: &Path,
    base_url: &str,
    model: &str,
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codape"))
        .args([
            "-p",
            "--dangerously-skip-permissions",
            "--output-format",
            "stream-json",
            "--no-session-persistence",
        ])
        .current_dir(workspace)
        .env("ANTHROPIC_API_KEY", "mock")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_MODEL", model)
        .env("CLAUDE_CONFIG_DIR", config_home)
        .env("CODAPE_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codape");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"hello\n")
        .expect("write prompt");
    child.wait_with_output().expect("wait")
}

/// The provider bills 10 in and 20 out per request. A run that makes one
/// request must report exactly that.
#[test]
fn a_result_frame_reports_the_usage_the_provider_billed() {
    let frame = result_frame("claude-sonnet-4-6", None);

    let usage = frame
        .get("usage")
        .unwrap_or_else(|| panic!("the result frame carried no usage: {frame}"));
    assert_eq!(
        usage["input_tokens"],
        serde_json::json!(10),
        "input tokens were not reported: {frame}"
    );
    assert_eq!(
        usage["output_tokens"],
        serde_json::json!(20),
        "output tokens were not reported: {frame}"
    );
    // Both spellings of the cache keys, so a host written against either the
    // reference's long form or this repository's shorter one reads the field.
    assert!(usage.get("cache_read_input_tokens").is_some(), "{frame}");
    assert!(usage.get("cache_read_tokens").is_some(), "{frame}");
    assert!(
        usage.get("cache_creation_input_tokens").is_some(),
        "{frame}"
    );
    assert!(usage.get("cache_creation_tokens").is_some(), "{frame}");
}

/// A model with no known price reports tokens and no dollar figure.
///
/// `pricing_for_model` falls back to Sonnet's rate so budgeting always has a
/// number. Reusing that fallback here would print Sonnet's price as the cost
/// of a gateway model — an invented number is worse than a missing one,
/// because a missing column makes the reader go looking for the real cost.
#[test]
fn an_unpriced_model_reports_tokens_without_inventing_a_cost() {
    let frame = result_frame("deepseek-v4.1-flash", None);

    assert!(
        frame.get("usage").is_some(),
        "an unpriced model still reports tokens: {frame}"
    );
    assert!(
        frame.get("total_cost_usd").is_none(),
        "a cost was invented for a model with no known price: {frame}"
    );
}

/// A pricing entry in the config home turns the cost column on.
///
/// This is the documented escape hatch, and it is what makes omitting the
/// field the right default: the user says what the model costs once and the
/// figure appears, rather than the binary guessing and being wrong. The model
/// here is a gateway name with no built-in price, and the numbers are its own,
/// so the assertion fails if the file is read in the wrong place or ignored.
#[test]
fn a_configured_price_is_reported() {
    let pricing = r#"{
        "deepseek-v4.1-flash": {
            "input_per_million": 100.0,
            "output_per_million": 200.0,
            "cache_read_per_million": 0.0,
            "cache_creation_per_million": 0.0
        }
    }"#;
    let frame = result_frame("deepseek-v4.1-flash", Some(pricing));

    let cost = frame
        .get("total_cost_usd")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or_else(|| panic!("no total_cost_usd after a price was configured: {frame}"));
    // 10 input tokens at $100/M is $0.001; 20 output at $200/M is $0.004.
    let expected = 0.005;
    assert!(
        (cost - expected).abs() < 1e-12,
        "reported ${cost}, expected ${expected}"
    );
}
