//! Job tools — list, read, and stop the shell jobs the platform's shell starts.
//!
//! The shell tool records every backgrounded command as a job under
//! `.codape/jobs/`. Those records used to be write-only: nothing let the model
//! find the job it started a minute ago, read a server's log while the server
//! ran, or stop it. Without that, the only tools left were `ps`, scanning
//! `/proc`, and `pkill` — and benchmark images routinely ship none of them, so
//! a "restart the server" step became a `/proc` scan that could (and did) kill
//! its own shell.
//!
//! These three tools are the supported way to manage background work. They
//! read the same durable store the shell tool writes, so a job started in an
//! earlier call — or an earlier process — is still addressable by id.
//!
//! Only `Bash` backgrounds, so only on a platform that registers `Bash` is
//! there anything to manage. See [`LIST_DESCRIPTION`].

/// What `JobList` says it lists.
///
/// The start call has to be named, because "a background job" alone leaves the
/// model unable to create one. It also has to be the call *this platform*
/// offers: `Bash` is registered only where the platform is not Windows, and
/// `PowerShell` takes no `run_in_background` at all — its schema advertises one
/// that `advertised_schema` strips, and its `execute` never reads it. So on
/// Windows there is no way to start a shell job, and the honest description
/// says so rather than pointing at a tool the session does not have.
const LIST_DESCRIPTION: &str = {
    if cfg!(windows) {
        "No background shell jobs can be started on this platform: `PowerShell` runs every \
         command to completion inside its call. This tool will always report an empty list."
    } else {
        "Lists the background shell jobs started with Bash `run_in_background`: the job id, \
         state, pid, start time, output file and the command. Use it to find again a job you \
         started earlier — a server, a build, a test run — instead of `ps`, which many images \
         do not have."
    }
};

/// What `JobOutput` says it reads. See [`LIST_DESCRIPTION`] for why this is
/// platform-shaped.
const OUTPUT_DESCRIPTION: &str = {
    if cfg!(windows) {
        "No background shell jobs can be started on this platform: `PowerShell` runs every \
         command to completion inside its call. This tool has nothing to read."
    } else {
        "Reads the output of a background shell job started with Bash `run_in_background`. \
         Returns the job's state and the last `tail_lines` lines of its output file. The file \
         is written while the command runs, so calling this again shows the new output — this \
         is how you check whether a server has started, instead of starting a second copy.\n\n\
         With `wait_secs`, the call blocks until the job finishes or that many seconds pass, \
         then returns the state and the new output. Use it to wait for a build or a test run \
         rather than sleeping in the foreground and reading the file again: one call replaces \
         a `sleep N; cat jobfile` loop, and it returns as soon as the job ends instead of at \
         the end of a fixed sleep."
    }
};

use crate::context::{ToolError, ToolOutput};
use crate::job_control;
use crate::tool::{
    MaxResultSizeChars, Tool, ToolCapabilities, ToolContext, ToolDescriptor, ToolInput, ToolTier,
};
use async_trait::async_trait;
use codape_core::job_record::{JobKind, JobRecord, JobState, JobStore};
use std::fmt::Write as _;
use std::time::Duration;

/// How much of a job's output one call returns.
const MAX_OUTPUT_BYTES: usize = 20_000;

/// Default number of trailing lines returned by `JobOutput`.
const DEFAULT_TAIL_LINES: usize = 200;

/// Largest `wait_secs` a `JobOutput` call may ask for.
///
/// The same ceiling `Bash` and `Sleep` use, for the same reason: a call that
/// blocks must have a deadline it advertises, and ten minutes is the longest
/// one this platform already lets a tool take. A background job can legitimately
/// run for hours, so a wait is always bounded — the model re-issues the call
/// rather than holding one forever.
const MAX_WAIT_SECS: u64 = 600;

/// How often a blocking `JobOutput` re-reads the record.
///
/// A record is one small JSON file, so a poll is cheap, and 200 ms is well
/// under the granularity of anything the model can act on. Coarser would make
/// a job that finishes just after a poll wait needlessly; finer would spin.
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(200);

fn job_store(ctx: &ToolContext<'_>) -> JobStore {
    JobStore::new(ctx.cwd)
}

fn list_error(error: codape_core::job_record::JobStoreError) -> ToolError {
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

/// Render one job's state and the tail of its output file.
fn render_job(store: &JobStore, job_id: &str, record: &JobRecord, tail_lines: usize) -> String {
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
    output
}

/// Re-read `record` until it reaches a terminal state or `secs` elapse.
///
/// The wait is a poll of the durable record rather than a subscription: the
/// record is one small JSON file that `Bash`'s background task settles when
/// the child exits, and a session has a handful of jobs, so a 200 ms re-read
/// costs nothing and needs no channel between the tool and every possible
/// producer. It also works for a job started by an earlier process.
///
/// Returns `true` when the deadline passed with the job still running. A job
/// that disappears from the store mid-wait is treated as settled: the record
/// is the only claim of liveness there is, so its absence cannot mean
/// "still running".
async fn wait_for_settle(
    store: &JobStore,
    job_id: &str,
    record: &mut JobRecord,
    secs: u64,
) -> Result<bool, ToolError> {
    if record.state.is_terminal() {
        return Ok(false);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        tokio::time::sleep(WAIT_POLL_INTERVAL).await;
        match store.load(job_id).map_err(list_error)? {
            Some(current) => {
                let settled = current.state.is_terminal();
                *record = current;
                if settled {
                    return Ok(false);
                }
            }
            None => return Ok(false),
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(true);
        }
    }
}

/// List the background shell jobs of this session.
#[derive(Debug)]
pub struct JobListTool;

#[async_trait]
impl Tool for JobListTool {
    fn description(&self) -> Option<&'static str> {
        Some(LIST_DESCRIPTION)
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
                output: if cfg!(windows) {
                    // No call can start one here, so the ordinary "none
                    // running yet" wording would read as "start one and come
                    // back", which is the misleading half.
                    LIST_DESCRIPTION.to_string()
                } else if all {
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
        Some(OUTPUT_DESCRIPTION)
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
                    },
                    "wait_secs": {
                        "type": "number",
                        "description": "Block until the job finishes, up to this many seconds \
                                        (max 600), then return its state and new output. Omit \
                                        to read the job as it is now. Use this instead of \
                                        sleeping and reading the output file again."
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
            // `wait_secs` blocks for up to ten minutes, so the executor's 60 s
            // default would cut it off with a `ToolError::Timeout` the tool
            // never asked for. `wait_for_settle` owns its deadline instead.
            self_managed_timeout: true,
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
        let wait_secs = input
            .arguments
            .get("wait_secs")
            .and_then(serde_json::Value::as_u64)
            .map_or(0, |n| n.min(MAX_WAIT_SECS));

        let store = job_store(&ctx);
        let mut record =
            store
                .load(&job_id)
                .map_err(list_error)?
                .ok_or_else(|| ToolError::InvalidInput {
                    message: format!(
                        "unknown job '{job_id}'; JobList shows the jobs of this session"
                    ),
                })?;

        let waited_out = if wait_secs > 0 {
            wait_for_settle(&store, &job_id, &mut record, wait_secs).await?
        } else {
            false
        };

        let mut output = render_job(&store, &job_id, &record, tail_lines);
        if wait_secs > 0 {
            // Say which of the two happened. A job still running at the
            // deadline means "ask again", not "it is stuck", and the model
            // has to be able to tell those apart without guessing.
            if waited_out {
                let _ = writeln!(
                    output,
                    "(waited {wait_secs}s; the job is still {})",
                    record.state.label()
                );
            } else {
                let _ = writeln!(output, "(waited until the job ended)");
            }
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
                codape_core::task::now_ms(),
                None,
                Some("stopped by JobKill".to_string()),
            );
            format!("stopped job {job_id} (pid {pid}) and its process group.")
        } else {
            let _ = store.settle(
                &job_id,
                JobState::Failed,
                codape_core::task::now_ms(),
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
    use crate::tool::BoxedTool;
    use codape_core::task::now_ms;
    use std::path::Path;

    /// Does a description's claim about how jobs are started hold up?
    ///
    /// A job tool's text says how to make one — "started with Bash
    /// `run_in_background`". That is a claim about the tool list, and it can be
    /// false in two ways: the named tool is not registered on this platform, or
    /// it is registered but strips `run_in_background` from what it sends.
    ///
    /// The second way is the one that actually bit. `PowerShell`'s
    /// `descriptor` does list `run_in_background`, but its `capabilities` is
    /// `Backgrounding::None`, and `advertised_schema` removes the property
    /// before it reaches the wire — so a check reading `descriptor()` would see
    /// the parameter and pass. This reads `input_schema` **after** that
    /// stripping, which is what the model gets.
    ///
    /// Split out from both inputs so each platform's shape can be tested on any
    /// machine: a check that read `cfg!(windows)` would only ever exercise the
    /// platform running it, and the Windows half is the broken half.
    fn claim_holds(claim: &str, offered: &[(String, BoxedTool)]) -> Result<(), String> {
        let Some(named) = ["Bash", "PowerShell"]
            .into_iter()
            .find(|n| claim.contains(n))
        else {
            return Ok(()); // a claim that names no shell tool asserts nothing
        };
        let Some((_, start_tool)) = offered.iter().find(|(name, _)| name == named) else {
            return Err(format!("{named} is not offered on this platform"));
        };
        // `advertised_schema` is private to the dispatcher, so mirror its one
        // rule here: a tool that does not background has the property removed.
        let advertised = start_tool.capabilities().backgrounding;
        let sends_it = advertised != crate::tool::Backgrounding::None
            && start_tool
                .descriptor()
                .input_schema
                .pointer("/properties/run_in_background")
                .is_some();
        if !sends_it {
            return Err(format!("{named} does not send `run_in_background`"));
        }
        Ok(())
    }

    /// The job tools name the call that *starts* the jobs they report on, so
    /// that name — and the argument it names — has to be something a model can
    /// actually use on this platform.
    ///
    /// Read against the real registry, this is the live guard.
    #[test]
    fn the_job_tools_name_only_a_start_call_this_platform_offers() {
        let mut registry = crate::registry::ToolRegistry::new();
        crate::builtins::register_builtin_tools(&mut registry);
        let offered: Vec<(String, BoxedTool)> = registry.list_tools();

        for tool in [&JobListTool as &dyn Tool, &JobOutputTool as &dyn Tool] {
            let description = tool
                .description()
                .expect("the job tools describe themselves");
            let Some(start) = description.find("started with ") else {
                // Naming no start call is only honest where none exists, so it
                // is acceptable exactly when the shell tool cannot background.
                let any_can_background = offered.iter().any(|(_, tool)| {
                    tool.capabilities().backgrounding != crate::tool::Backgrounding::None
                });
                assert!(
                    !any_can_background,
                    "{} names no way to start a job although this platform can background one",
                    tool.descriptor().name
                );
                continue;
            };
            if let Err(why) = claim_holds(&description[start..], &offered) {
                panic!("{} makes a false claim: {why}", tool.descriptor().name);
            }
        }
    }

    /// The guard above is worth nothing if `claim_holds` cannot fail. These are
    /// the two shapes it has to reject, built here rather than borrowed from
    /// the machine the test runs on — so both are checked on every platform.
    #[test]
    fn a_claim_about_a_start_call_is_judged_against_what_is_offered() {
        let with_bash: Vec<(String, BoxedTool)> = {
            let mut registry = crate::registry::ToolRegistry::new();
            registry.register(std::sync::Arc::new(crate::builtins::BashTool::new()));
            registry.list_tools()
        };
        let bash_claim = "started with Bash `run_in_background`: the job id";
        claim_holds(bash_claim, &with_bash).expect("Bash takes run_in_background and is offered");

        // The Windows shape: PowerShell is the shell tool and takes no
        // `run_in_background`, so the Bash wording is false AND PowerShell
        // wording would be false too.
        let powershell_only: Vec<(String, BoxedTool)> = {
            let mut registry = crate::registry::ToolRegistry::new();
            registry.register(std::sync::Arc::new(crate::builtins::PowerShellTool));
            registry.list_tools()
        };
        assert!(
            claim_holds(bash_claim, &powershell_only).is_err(),
            "a Bash claim where Bash is not offered must be rejected"
        );
        assert!(
            claim_holds(
                "started with PowerShell `run_in_background`",
                &powershell_only
            )
            .is_err(),
            "PowerShell does not take run_in_background, so naming it is equally false"
        );

        // Nothing offered at all.
        assert!(claim_holds(bash_claim, &[]).is_err());

        // A claim naming no shell tool asserts nothing and is not an error —
        // this is the wording Windows ships, which must stay allowed.
        claim_holds(
            "No background shell jobs can be started on this platform",
            &[],
        )
        .expect("a description that names no start call makes no claim");
    }

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

    /// Process groups are a Unix idea; Windows `JobKill` uses `taskkill /T`,
    /// which has no `process_group` to set up here.
    #[cfg(unix)]
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

    #[tokio::test]
    async fn wait_returns_as_soon_as_the_job_settles() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "shell-w-0", JobState::Running);
        let store = JobStore::new(dir.path());
        let id = "shell-w-0".to_string();
        let root = dir.path().to_path_buf();

        // Settle the record from another task after ~300 ms, which is more than
        // one poll interval — so this fails if the wait never re-reads.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = JobStore::new(&root).settle(&id, JobState::Done, now_ms(), Some(0), None);
        });

        let started = std::time::Instant::now();
        let output = JobOutputTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "JobOutput".into(),
                    arguments: serde_json::json!({ "job_id": "shell-w-0", "wait_secs": 30 }),
                },
                ctx(dir.path()),
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();

        // The point of the tool: it returns when the job ends, not when the
        // deadline the model guessed at expires. Asserting the deadline
        // (30 s) would pass even with the wait removed, so assert the early
        // return instead — with room for a loaded CI machine.
        assert!(
            elapsed < Duration::from_secs(10),
            "the wait did not return on settle: {elapsed:?}"
        );
        assert!(output.output.contains("done"), "{}", output.output);
        assert!(
            output.output.contains("waited until the job ended"),
            "{}",
            output.output
        );
        assert!(
            store
                .load("shell-w-0")
                .unwrap()
                .unwrap()
                .state
                .is_terminal()
        );
    }

    #[tokio::test]
    async fn wait_stops_at_the_deadline_and_says_the_job_is_still_running() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "shell-w-1", JobState::Running);

        let started = std::time::Instant::now();
        let output = JobOutputTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "JobOutput".into(),
                    arguments: serde_json::json!({ "job_id": "shell-w-1", "wait_secs": 1 }),
                },
                ctx(dir.path()),
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed >= Duration::from_secs(1),
            "the wait gave up early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "the wait overshot its deadline: {elapsed:?}"
        );
        assert!(
            output.output.contains("the job is still running"),
            "{}",
            output.output
        );
    }

    /// A wait that asks for more than the ceiling is clamped, not honoured and
    /// not rejected: the caller gets a bounded call either way.
    #[tokio::test]
    async fn wait_secs_is_clamped_to_the_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "shell-w-2", JobState::Done);
        let output = JobOutputTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "JobOutput".into(),
                    arguments: serde_json::json!({ "job_id": "shell-w-2", "wait_secs": 999_999 }),
                },
                ctx(dir.path()),
            )
            .await
            .unwrap();
        // Already terminal, so it returns immediately — but the clamp must not
        // have turned into a 999999-second sleep before checking state.
        assert!(output.output.contains("done"), "{}", output.output);
    }

    /// The executor cuts every non-`self_managed_timeout` call at 60 s, so a
    /// blocking `JobOutput` has to opt out or `wait_secs` above that is
    /// silently truncated into a `ToolError::Timeout`.
    #[test]
    fn waiting_takes_over_its_own_deadline_from_the_executor() {
        assert!(
            JobOutputTool.capabilities().self_managed_timeout,
            "a blocking JobOutput would be cut off at the executor's 60 s default"
        );
        // A ceiling at or below the executor's default would make the opt-out
        // pointless, so the relationship is checked at compile time.
        const { assert!(MAX_WAIT_SECS > 60) };
    }

    /// End to end against a real background child: the point of `wait_secs` is
    /// that it replaces `sleep N; cat jobfile`, so both halves of that have to
    /// be beaten — it must return when the job ends, and it must return the
    /// output the read would have found.
    #[tokio::test]
    async fn wait_on_a_real_background_job_returns_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let started = crate::builtins::BashTool::new()
            .execute(
                ToolInput {
                    call_id: "bg".into(),
                    name: "Bash".into(),
                    arguments: serde_json::json!({
                        "command": "sleep 0.4; echo waited-result",
                        "run_in_background": true,
                    }),
                },
                ctx(dir.path()),
            )
            .await
            .unwrap();
        let handle: serde_json::Value = serde_json::from_str(&started.output).unwrap();
        let id = handle["job_id"].as_str().unwrap().to_string();
        assert_eq!(handle["note"], "command running in background");

        let before = std::time::Instant::now();
        let output = JobOutputTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "JobOutput".into(),
                    arguments: serde_json::json!({ "job_id": id, "wait_secs": 30 }),
                },
                ctx(dir.path()),
            )
            .await
            .unwrap();
        let elapsed = before.elapsed();

        assert!(
            elapsed < Duration::from_secs(10),
            "the wait ran to its deadline instead of returning on settle: {elapsed:?}"
        );
        assert!(output.output.contains("done"), "{}", output.output);
        assert!(
            output.output.contains("waited-result"),
            "the wait returned without the output the job wrote: {}",
            output.output
        );
    }
}
