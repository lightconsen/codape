//! Job tools — list, read, and stop the shell jobs the Bash tool started.
//!
//! The Bash tool records every command as a job under `.coderex/jobs/`. Those
//! records used to be write-only: nothing let the model find the job it started
//! a minute ago, read a server's log while the server ran, or stop it. Without
//! that, the only tools left were `ps`, scanning `/proc`, and `pkill` —
//! and benchmark images routinely ship none of them, so a "restart the server"
//! step became a `/proc` scan that could (and did) kill its own shell.
//!
//! These three tools are the supported way to manage background work. They
//! read the same durable store the Bash tool writes, so a job started in an
//! earlier call — or an earlier process — is still addressable by id.

use crate::context::{ToolError, ToolOutput};
use crate::job_control;
use crate::tool::{
    MaxResultSizeChars, Tool, ToolCapabilities, ToolContext, ToolDescriptor, ToolInput, ToolTier,
};
use async_trait::async_trait;
use coderex_core::job_record::{JobKind, JobRecord, JobState, JobStore};
use std::fmt::Write as _;

/// How much of a job's output one call returns.
const MAX_OUTPUT_BYTES: usize = 20_000;

/// Default number of trailing lines returned by `JobOutput`.
const DEFAULT_TAIL_LINES: usize = 200;

fn job_store(ctx: &ToolContext<'_>) -> JobStore {
    JobStore::new(ctx.cwd)
}

fn list_error(error: coderex_core::job_record::JobStoreError) -> ToolError {
    ToolError::Execution {
        name: "jobs".into(),
        message: error.to_string(),
    }
}

/// A short timestamp for a job listing.
fn started_at(record: &JobRecord) -> String {
    chrono::DateTime::from_timestamp_millis(record.started_at_ms).map_or_else(
        || record.started_at_ms.to_string(),
        |time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
}

/// The tail of a byte buffer as text, cutting on a line boundary when it had
/// to drop the front.
fn tail_text(bytes: &[u8], max_bytes: usize, tail_lines: Option<usize>) -> (String, bool) {
    let truncated = bytes.len() > max_bytes;
    let start = bytes.len().saturating_sub(max_bytes);
    let mut text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    if truncated && let Some(newline) = text.find('\n') {
        text = text[newline + 1..].to_string();
    }
    if let Some(limit) = tail_lines {
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() > limit {
            text = lines[lines.len() - limit..].join("\n");
        }
    }
    (text, truncated)
}

/// List the background shell jobs of this session.
#[derive(Debug)]
pub struct JobListTool;

#[async_trait]
impl Tool for JobListTool {
    fn description(&self) -> Option<&'static str> {
        Some(
            "Lists the background shell jobs started with Bash `run_in_background`: the job id, \
             state, pid, start time, output file and the command. Use it to find again a job you \
             started earlier — a server, a build, a test run — instead of `ps`, which many images \
             do not have.",
        )
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "JobList".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "all": {
                        "type": "boolean",
                        "description": "Include finished jobs as well as running ones (default false)"
                    }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "jobs": { "type": "array", "items": { "type": "object" } }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            max_result_size_chars: MaxResultSizeChars::Tiny,
            // The set of jobs and their states change between calls, and a
            // cached listing would keep saying "running" long after the job
            // exited.
            skip_result_cache: true,
            ..ToolCapabilities::default()
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let all = input
            .arguments
            .get("all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let jobs: Vec<JobRecord> = job_store(&ctx)
            .list()
            .map_err(list_error)?
            .into_iter()
            .filter(|job| job.kind == JobKind::Shell)
            .filter(|job| all || job.state == JobState::Running)
            .collect();

        if jobs.is_empty() {
            return Ok(ToolOutput {
                output: if all {
                    "No background shell jobs.".to_string()
                } else {
                    "No running background shell jobs. `JobList` with `all: true` also shows \
                     finished ones."
                        .to_string()
                },
                is_display: true,
                images: Vec::new(),
            });
        }

        let store = job_store(&ctx);
        let mut output = String::new();
        for job in &jobs {
            let _ = writeln!(
                output,
                "- {}  {}  pid {}{}  started {}  output {}\n    {}",
                job.id,
                job.state.label(),
                job.pid
                    .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
                job.exit_code
                    .map_or_else(String::new, |code| format!("  exit {code}")),
                started_at(job),
                store.root().join(format!("{}.out", job.id)).display(),
                job.description,
            );
            if let Some(error) = &job.error {
                let _ = writeln!(output, "    error: {error}");
            }
        }
        Ok(ToolOutput {
            output,
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Read a background shell job's output.
#[derive(Debug)]
pub struct JobOutputTool;

#[async_trait]
impl Tool for JobOutputTool {
    fn description(&self) -> Option<&'static str> {
        Some(
            "Reads the output of a background shell job started with Bash `run_in_background`. \
             Returns the job's state and the last `tail_lines` lines of its output file. The file \
             is written while the command runs, so calling this again shows the new output — this \
             is how you check whether a server has started, instead of starting a second copy.",
        )
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "JobOutput".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["job_id"],
                "properties": {
                    "job_id": { "type": "string" },
                    "tail_lines": {
                        "type": "number",
                        "description": "How many trailing lines to return (default 200)"
                    }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "output": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
            is_concurrency_safe: true,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            max_result_size_chars: MaxResultSizeChars::Small,
            // Polling is the point: the same call must see output written
            // since the last one.
            skip_result_cache: true,
            ..ToolCapabilities::default()
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let job_id = extract_string(&input.arguments, "job_id")?;
        let tail_lines = input
            .arguments
            .get("tail_lines")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(DEFAULT_TAIL_LINES);

        let store = job_store(&ctx);
        let record =
            store
                .load(&job_id)
                .map_err(list_error)?
                .ok_or_else(|| ToolError::InvalidInput {
                    message: format!(
                        "unknown job '{job_id}'; JobList shows the jobs of this session"
                    ),
                })?;

        let output_path = store.root().join(format!("{job_id}.out"));
        let bytes = std::fs::read(&output_path).unwrap_or_default();
        let (text, truncated) = tail_text(&bytes, MAX_OUTPUT_BYTES, Some(tail_lines));

        let mut output = format!(
            "job {job_id}: {}{}\n",
            record.state.label(),
            record
                .exit_code
                .map_or_else(String::new, |code| format!(", exit code {code}")),
        );
        if let Some(error) = &record.error {
            let _ = writeln!(output, "error: {error}");
        }
        output.push_str("---\n");
        if truncated {
            output.push_str("... (earlier output omitted)\n");
        }
        output.push_str(&text);
        if !text.is_empty() && !text.ends_with('\n') {
            output.push('\n');
        }

        Ok(ToolOutput {
            output,
            is_display: true,
            images: Vec::new(),
        })
    }
}

/// Stop a background shell job.
#[derive(Debug)]
pub struct JobKillTool;

#[async_trait]
impl Tool for JobKillTool {
    fn description(&self) -> Option<&'static str> {
        Some(
            "Stops a background shell job and every process in its process group: SIGTERM, then \
             SIGKILL after a short grace period. The job is recorded as failed. Use it instead of \
             `kill`/`pkill`, which many images do not have and which is easy to aim at the wrong \
             process — `pkill -f` can match the shell that is running it.",
        )
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "JobKill".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["job_id"],
                "properties": {
                    "job_id": { "type": "string" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "stopped": { "type": "boolean" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
            is_concurrency_safe: false,
            interrupt_behavior: crate::tool::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let job_id = extract_string(&input.arguments, "job_id")?;
        let store = job_store(&ctx);
        let record =
            store
                .load(&job_id)
                .map_err(list_error)?
                .ok_or_else(|| ToolError::InvalidInput {
                    message: format!(
                        "unknown job '{job_id}'; JobList shows the jobs of this session"
                    ),
                })?;

        let output = if record.state != JobState::Running {
            format!(
                "job {job_id} already ended ({}{}); nothing to stop.",
                record.state.label(),
                record
                    .exit_code
                    .map_or_else(String::new, |code| format!(", exit code {code}"))
            )
        } else if let Some(pid) = record.pid.filter(|pid| job_control::group_exists(*pid)) {
            job_control::terminate_group(pid).await;
            let _ = store.settle(
                &job_id,
                JobState::Failed,
                coderex_core::task::now_ms(),
                None,
                Some("stopped by JobKill".to_string()),
            );
            format!("stopped job {job_id} (pid {pid}) and its process group.")
        } else {
            let _ = store.settle(
                &job_id,
                JobState::Failed,
                coderex_core::task::now_ms(),
                None,
                Some("the process was already gone".to_string()),
            );
            format!("job {job_id} was not running: its process group is already gone.")
        };

        Ok(ToolOutput {
            output,
            is_display: true,
            images: Vec::new(),
        })
    }
}

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use coderex_core::task::now_ms;
    use std::path::Path;

    fn ctx(cwd: &Path) -> ToolContext<'_> {
        ToolContext::new(ExecutionContext::Normal, cwd)
    }

    fn record(cwd: &Path, id: &str, state: JobState) -> JobRecord {
        let store = JobStore::new(cwd);
        let mut record = JobRecord::new(id, JobKind::Shell, "sleep 30", cwd, None, now_ms());
        record.state = state;
        store.write(&record).unwrap();
        record
    }

    #[tokio::test]
    async fn list_shows_running_jobs_and_hides_finished_ones() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "shell-1-0", JobState::Running);
        record(dir.path(), "shell-2-0", JobState::Done);

        let running = JobListTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "JobList".into(),
                    arguments: serde_json::json!({}),
                },
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(running.output.contains("shell-1-0"));
        assert!(!running.output.contains("shell-2-0"));

        let all = JobListTool
            .execute(
                ToolInput {
                    call_id: "2".into(),
                    name: "JobList".into(),
                    arguments: serde_json::json!({ "all": true }),
                },
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(all.output.contains("shell-2-0"));
        assert!(all.output.contains("sleep 30"));
    }

    #[tokio::test]
    async fn output_returns_the_tail_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "shell-1-0", JobState::Running);
        let path = JobStore::new(dir.path()).root().join("shell-1-0.out");
        std::fs::write(&path, "line one\nline two\nline three\n").unwrap();

        let output = JobOutputTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "JobOutput".into(),
                    arguments: serde_json::json!({ "job_id": "shell-1-0", "tail_lines": 1 }),
                },
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(output.output.contains("running"), "{}", output.output);
        assert!(output.output.contains("line three"));
        assert!(!output.output.contains("line one"));
    }

    #[tokio::test]
    async fn output_for_an_unknown_job_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let error = JobOutputTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "JobOutput".into(),
                    arguments: serde_json::json!({ "job_id": "shell-nope" }),
                },
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("JobList"), "{error}");
    }

    #[tokio::test]
    async fn kill_stops_the_group_and_settles_the_record() {
        use std::os::unix::process::CommandExt as _;
        use std::os::unix::process::ExitStatusExt as _;

        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .process_group(0)
            .spawn()
            .unwrap();
        let store = JobStore::new(dir.path());
        let mut job = JobRecord::new(
            "shell-k-0",
            JobKind::Shell,
            "sleep 30",
            dir.path(),
            None,
            now_ms(),
        );
        job.pid = Some(child.id());
        store.write(&job).unwrap();

        let output = JobKillTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "JobKill".into(),
                    arguments: serde_json::json!({ "job_id": "shell-k-0" }),
                },
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(output.output.contains("stopped job"), "{}", output.output);

        let settled = store.load("shell-k-0").unwrap().unwrap();
        assert_eq!(settled.state, JobState::Failed);
        assert!(settled.error.unwrap().contains("JobKill"));
        let status = child.wait().unwrap();
        assert!(
            status.signal().is_some(),
            "child was not signalled: {status:?}"
        );
    }

    #[tokio::test]
    async fn kill_on_a_finished_job_is_harmless() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "shell-1-0", JobState::Done);
        let output = JobKillTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "JobKill".into(),
                    arguments: serde_json::json!({ "job_id": "shell-1-0" }),
                },
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(output.output.contains("already ended"), "{}", output.output);
    }
}
