//! Bash tool — shell command execution with timeout.
//!
//! When a [`SandboxConfig`] is present in `ToolContext.extra` under the key
//! `"sandbox_config"`, the command is executed inside the platform sandbox:
//!
//! - **Linux**: seccomp BPF + rlimits applied via `pre_exec`.
//! - **macOS**: command rewritten to run through `sandbox-exec`.
//! - **Windows**: Job Object (requires synchronous spawn).
//!
//! Every command — foreground or background — is a job: it is spawned into a
//! process group of its own, its output goes straight to a file under
//! `.coderex/jobs/`, and its record is settled when it exits. That is what
//! makes a server started with `run_in_background` outlive the call (and a
//! harness timeout aimed at CodeRex), and what lets a foreground command that
//! runs past its timeout be moved to the background instead of being orphaned.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::context::{ToolError, ToolOutput};
use crate::job_control;
use crate::tool::{
    Backgrounding, JOB_SESSION_ID_KEY, Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier,
};
use async_trait::async_trait;
use coderex_core::job_record::{JobKind, JobRecord, JobState, JobStore};
use coderex_core::shell_security::BashAst;
use coderex_core::task::now_ms;
use tokio::process::{Child, Command};

/// Build the shell command under the session's sandbox policy.
///
/// The platform logic lives in [`crate::sandbox_shell`], shared with the
/// PowerShell tool: the sandbox is a property of *the shell tool*, and having it
/// inside this one is how the other shell ended up unconfined.
fn build_sandboxed_command(
    ctx: &ToolContext<'_>,
    command: &str,
) -> Result<(Command, Option<coderex_core::sandbox::SandboxHandle>), ToolError> {
    crate::sandbox_shell::sandboxed_shell(ctx, "sh", &["-c", command])
}

/// Default timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Maximum timeout in seconds.
const MAX_TIMEOUT_SECS: u64 = 600;

/// Maximum output size in bytes before truncation.
const MAX_OUTPUT_BYTES: usize = 50000;

/// Execute shell commands with timeout control.
#[derive(Debug, Default)]
pub struct BashTool {
    /// Optional allowlist of permitted commands.
    /// When `Some`, only commands whose first token is in the list are allowed.
    allowed_commands: Option<Vec<String>>,
}

/// What a running job leaves for its caller.
#[derive(Debug, Clone)]
struct JobHandle {
    id: String,
    pid: Option<u32>,
    store: JobStore,
    output_path: PathBuf,
}

impl JobHandle {
    /// The JSON handle the model gets back for a job it should come back to.
    fn as_output(&self, note: &str) -> ToolOutput {
        ToolOutput {
            output: serde_json::json!({
                "job_id": self.id,
                "pid": self.pid,
                "output_file": self.output_path,
                "note": note,
            })
            .to_string(),
            is_display: true,
            images: Vec::new(),
        }
    }
}

/// How a job ended.
#[derive(Debug, Clone)]
struct JobOutcome {
    state: JobState,
    exit_code: Option<i32>,
    error: Option<String>,
}

impl BashTool {
    /// Create a new Bash tool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Restrict execution to a list of permitted command names.
    #[must_use]
    pub fn with_allowed_commands(mut self, commands: Vec<String>) -> Self {
        self.allowed_commands = Some(commands);
        self
    }

    /// Validate that `command` does not contain shell metacharacters
    /// and that its first token is in the allowlist (if configured).
    fn validate_command(&self, command: &str) -> Result<(), ToolError> {
        if let Some(ref allowed) = self.allowed_commands {
            // Reject common shell metacharacters that enable command injection
            let forbidden = [';', '|', '&', '`', '$', '(', ')', '<', '>'];
            if command.chars().any(|c| forbidden.contains(&c)) {
                return Err(ToolError::InvalidInput {
                    message: "shell metacharacters are not permitted when an allowlist is active"
                        .into(),
                });
            }

            let first_token = command.split_whitespace().next().unwrap_or("");
            if !allowed.iter().any(|a| a == first_token) {
                return Err(ToolError::PermissionDenied {
                    name: first_token.into(),
                });
            }
        }
        Ok(())
    }

    /// Spawn a command as a job: its own process group, output straight to a
    /// file, and a durable record written before this returns.
    ///
    /// The record is written after the spawn so it carries the pid; closing
    /// the process leaves it `Running`, which the adopt path reports as
    /// interrupted.
    fn spawn_job(
        ctx: &ToolContext<'_>,
        command: &str,
        cmd: &mut Command,
        sandbox_handle: Option<coderex_core::sandbox::SandboxHandle>,
    ) -> Result<
        (
            JobHandle,
            Child,
            Option<coderex_core::sandbox::SandboxHandle>,
        ),
        ToolError,
    > {
        let store = JobStore::new(ctx.cwd);
        let started_at_ms = now_ms();
        let id = next_job_id(started_at_ms);
        let output_path = store.root().join(format!("{id}.out"));
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).map_err(ToolError::Io)?;
        }

        // The child writes straight to the file, so the output is readable
        // while the command runs — which is how a service is polled. Copying
        // from a pipe at exit made the file appear only once the command had
        // finished, i.e. never for a server.
        let output = std::fs::File::create(&output_path).map_err(ToolError::Io)?;
        let errors = output.try_clone().map_err(ToolError::Io)?;
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(output))
            .stderr(std::process::Stdio::from(errors));

        // A command that stayed in CodeRex's process group would be killed by
        // any harness that times out the run (`timeout`, a benchmark runner)
        // and signalled that group.
        job_control::isolate(cmd);

        let child = cmd.spawn().map_err(ToolError::Io)?;
        let pid = child.id();
        crate::sandbox_shell::finish_spawn(sandbox_handle.as_ref(), &child)?;

        let session_id = ctx
            .extra
            .get(JOB_SESSION_ID_KEY)
            .and_then(|value| value.downcast_ref::<String>())
            .map(String::as_str);
        let mut record = JobRecord::new(
            id.clone(),
            JobKind::Shell,
            job_description(command),
            ctx.cwd,
            session_id,
            started_at_ms,
        );
        record.pid = pid;
        if let Err(error) = store.write(&record) {
            // Refusing to run a working command because its record could not
            // be written would be a new failure mode; run it, but make the
            // lost durability visible.
            tracing::warn!(
                job = %id,
                path = %store.root().display(),
                error = %error,
                "could not write the shell job record"
            );
        }

        let handle = JobHandle {
            id,
            pid,
            store,
            output_path,
        };
        Ok((handle, child, sandbox_handle))
    }
}

/// Wait for a job, optionally stopping its process group at a deadline, and
/// settle its durable record.
///
/// `kill_after` is the only deadline this function enforces: a foreground
/// call's own timeout is enforced by the caller, which keeps this task running
/// (and the job alive) when that timeout fires.
async fn supervise(mut child: Child, job: JobHandle, kill_after: Option<Duration>) -> JobOutcome {
    let deadline_secs = kill_after.map(|limit| limit.as_secs());
    let (status, timed_out) = if let Some(limit) = kill_after {
        if let Ok(status) = tokio::time::timeout(limit, child.wait()).await {
            (status, false)
        } else {
            if let Some(pid) = job.pid {
                job_control::terminate_group(pid).await;
            }
            // Reap the direct child so the outcome carries its status.
            (child.wait().await, true)
        }
    } else {
        (child.wait().await, false)
    };

    let outcome = match status {
        Ok(status) => {
            let code = status.code().unwrap_or(-1);
            let (state, error) = if timed_out {
                (
                    JobState::Failed,
                    Some(format!("timed out after {}s", deadline_secs.unwrap_or(0))),
                )
            } else if status.success() {
                (JobState::Done, None)
            } else {
                (
                    JobState::Failed,
                    Some(format!("command exited with status {code}")),
                )
            };
            JobOutcome {
                state,
                exit_code: Some(code),
                error,
            }
        }
        Err(error) => JobOutcome {
            state: JobState::Failed,
            exit_code: None,
            error: Some(error.to_string()),
        },
    };

    let _ = job.store.settle(
        &job.id,
        outcome.state,
        now_ms(),
        outcome.exit_code,
        outcome.error.clone(),
    );
    outcome
}

/// A file a foreground command writes its final `$PWD` into.
///
/// Shell state does not survive a call, but the working directory should: a
/// `cd` in one call moves the next one. The command is run with a sentinel
/// appended that records where it ended, and the exit status is preserved by
/// capturing it before the sentinel and exiting with it.
struct CwdTracker {
    path: PathBuf,
}

impl CwdTracker {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let n = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!("coderex-cwd-{}-{n}", std::process::id())),
        }
    }

    /// The command with the sentinel appended.
    fn wrap(&self, command: &str) -> String {
        let path = self.path.to_string_lossy().replace('\'', "'\\''");
        format!(
            "{command}\n\
             __coderex_status=$?\n\
             pwd -P > '{path}' 2>/dev/null\n\
             exit $__coderex_status"
        )
    }

    /// Move the session's shell directory when the command changed it.
    ///
    /// A command that exits without reaching the sentinel (`exit`, `exec`, or
    /// `set -e` on a failure) leaves no file, and the directory stays where it
    /// was.
    fn apply(&self, ctx: &ToolContext<'_>) {
        let Some(shared) = crate::tool::session_cwd_from(ctx) else {
            return;
        };
        let Ok(contents) = std::fs::read_to_string(&self.path) else {
            return;
        };
        let path = Path::new(contents.trim());
        if path.is_absolute() && path.is_dir() {
            *shared.write() = path.to_path_buf();
        }
    }
}

impl Drop for CwdTracker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The directory a command should start in: where the last call left the
/// shell, when the session tracks it, else the session's own directory.
fn start_cwd(ctx: &ToolContext<'_>) -> PathBuf {
    crate::tool::session_cwd_from(ctx)
        .map(|shared| shared.read().clone())
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| ctx.cwd.to_path_buf())
}

#[async_trait]
impl Tool for BashTool {
    fn description(&self) -> Option<&'static str> {
        Some(
            "Executes a shell command and returns its output. This is the way to inspect the \
             machine and to run programs: list files, read what a service returns, check \
             processes, ports, and environment variables.\n\n\
             - The shell starts in the directory the previous call left it in, so `cd` persists \
             between calls. Exported variables and other shell state do not; pass a sequence to \
             one command or use absolute paths.\n\
             - The shell is non-interactive. Anything that waits for input blocks until the \
             timeout.\n\
             - `timeout` is in seconds (default 60, max 600). A foreground command still running \
             at its timeout is not killed: it moves to the background and the call answers with \
             a job handle instead of an error.\n\
             - `run_in_background` returns at once with `job_id`, `pid` and an `output_file` that \
             is written while the command runs. Use it for a server or a long job: start it \
             once, read the output with JobOutput and stop it with JobKill. Do not start a second \
             copy to check on it, and do not use `ps`/`pkill`, which many images do not have. A \
             background command runs until it exits; it stops only at its own `timeout` (when you \
             set one) or through JobKill.",
        )
    }

    /// The command is this tool's rule content, as it is in the reference
    /// (`Bash=command`). Declaring the field means a deny rule is judged
    /// against the command line the call is about to run — including a command
    /// with no slash in it, which the path-shaped fallback would not offer.
    fn capabilities(&self) -> crate::tool::ToolCapabilities {
        crate::tool::ToolCapabilities {
            rule_content_field: Some(crate::tool::RuleContentField::Scalar("command")),
            // `run_in_background` is genuinely handled: the tool spawns the
            // child, returns a handle, and records the job before returning.
            backgrounding: Backgrounding::SelfManaged,
            // The call's own `timeout` (up to 600 s) governs; the executor's
            // 60 s default would cut it off early.
            self_managed_timeout: true,
            ..crate::tool::ToolCapabilities::default()
        }
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Bash".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": { "type": "string" },
                    "timeout": { "type": "number", "description": "Seconds. Foreground: how long to wait before moving the command to the background (default 60, max 600). Background: stop the job after this many seconds (default: no limit)." },
                    "run_in_background": { "type": "boolean", "description": "Start the command as a background job and return its job id and output file" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "stdout": { "type": "string" },
                    "exit_code": { "type": "number" },
                    "job_id": { "type": "string" },
                    "pid": { "type": "number" },
                    "output_file": { "type": "string" },
                    "note": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: false,
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

        let command = extract_string(&input.arguments, "command")?;
        let default_timeout = i64::try_from(DEFAULT_TIMEOUT_SECS).unwrap_or(i64::MAX);
        let max_timeout = i64::try_from(MAX_TIMEOUT_SECS).unwrap_or(i64::MAX);
        let requested_timeout = extract_number(&input.arguments, "timeout");
        let timeout_secs = u64::try_from(
            requested_timeout
                .unwrap_or(default_timeout)
                .clamp(1, max_timeout),
        )
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
        let run_in_background = input
            .arguments
            .get("run_in_background")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        // Validate against allowlist and metacharacters
        self.validate_command(&command)?;

        // Shell security analysis
        let ast = BashAst::parse(&command);
        let security_warning = if ast.is_dangerous() {
            Some(format!("[SECURITY WARNING: {}]\n", ast.summary()))
        } else {
            None
        };

        // A foreground command carries a sentinel that reports where it left
        // the shell; a background one has no later call to inherit it.
        let mut tracker = None;
        let script = if run_in_background {
            command.clone()
        } else {
            let guard = CwdTracker::new();
            let script = guard.wrap(&command);
            tracker = Some(guard);
            script
        };

        let start = start_cwd(&ctx);
        let (mut cmd, sandbox_handle) = build_sandboxed_command(&ctx, &script)?;
        if start != ctx.cwd {
            cmd.current_dir(&start);
        }

        // The shell is not a provider client, so it is given no way to read
        // this session's credentials out of the environment it inherits.
        for key in coderex_core::sandbox::credential_scrub_keys() {
            cmd.env_remove(key);
        }
        crate::sandbox_shell::sanitize_shell_env(&mut cmd, &start);

        let (job, child, sandbox_handle) =
            Self::spawn_job(&ctx, &command, &mut cmd, sandbox_handle)?;

        if run_in_background {
            let kill_after = requested_timeout.map(|_| Duration::from_secs(timeout_secs));
            let job_for_task = job.clone();
            tokio::spawn(async move {
                // The handle keeps the platform sandbox alive for the child's
                // whole lifetime (a macOS profile, a Windows Job Object).
                let _sandbox_keepalive = sandbox_handle;
                let _ = supervise(child, job_for_task, kill_after).await;
            });
            return Ok(job.as_output("command running in background"));
        }

        // The supervisor is a task, not a future this call awaits directly, so
        // that a timeout can hand the still-running job to the caller instead
        // of dropping the wait with the process still alive.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let job_for_task = job.clone();
        tokio::spawn(async move {
            let _sandbox_keepalive = sandbox_handle;
            let outcome = supervise(child, job_for_task, None).await;
            let _ = tx.send(outcome);
        });

        let outcome = match tokio::time::timeout(Duration::from_secs(timeout_secs), rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => {
                return Err(ToolError::Execution {
                    name: "Bash".into(),
                    message: "the shell job ended without reporting its outcome".into(),
                });
            }
            Err(_) => {
                // Keep it: the job and its output file are still there, and the
                // model can collect them with JobOutput.
                return Ok(job.as_output(&format!(
                    "command still running after {timeout_secs}s; it continues in the background. \
                     Read it with JobOutput and stop it with JobKill"
                )));
            }
        };

        if let Some(tracker) = tracker {
            tracker.apply(&ctx);
        }

        let bytes = std::fs::read(&job.output_path).unwrap_or_default();
        let truncated = bytes.len() > MAX_OUTPUT_BYTES;
        let captured = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_OUTPUT_BYTES)]);

        let mut output = String::new();

        // Prepend security warning if dangerous
        if let Some(warning) = security_warning {
            output.push_str(&warning);
        }
        output.push_str(&captured);
        if truncated {
            output.push_str("\n\n... (truncated)");
        }

        let exit_code = outcome.exit_code.unwrap_or(-1);
        if outcome.exit_code.is_none()
            && let Some(error) = outcome.error
        {
            let _ = write!(output, "\n{error}");
        }
        let _ = write!(output, "\n\n[Exit code: {exit_code}]");

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

fn extract_number(args: &serde_json::Value, key: &str) -> Option<i64> {
    args.get(key).and_then(serde_json::Value::as_i64)
}

/// A job id that is safe as a file stem (no separators) and unique enough:
/// the start time plus a process-local sequence distinguishes two commands.
fn next_job_id(started_at_ms: i64) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let started_at_ms = started_at_ms.max(0);
    let n = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("shell-{started_at_ms}-{n}")
}

/// A bounded, human-readable description for a job record.
fn job_description(command: &str) -> String {
    const LIMIT: usize = 200;
    let mut out = String::with_capacity(command.len().min(LIMIT) + 1);
    let mut chars = command.chars();
    for _ in 0..LIMIT {
        match chars.next() {
            Some(c) => out.push(c),
            None => return out,
        }
    }
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::path::Path;

    fn test_ctx(cwd: &Path) -> ToolContext<'_> {
        ToolContext::new(ExecutionContext::Normal, cwd)
    }

    fn bash_input(command: &str) -> ToolInput {
        ToolInput {
            call_id: "1".into(),
            name: "Bash".into(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    #[tokio::test]
    async fn simple_command() {
        let output = BashTool::new()
            .execute(bash_input("echo hello"), test_ctx(Path::new("/tmp")))
            .await
            .unwrap();
        assert!(output.output.contains("hello"));
        assert!(output.output.contains("Exit code: 0"));
    }

    #[tokio::test]
    async fn command_with_stderr() {
        let output = BashTool::new()
            .execute(
                bash_input("echo stderr_out >&2"),
                test_ctx(Path::new("/tmp")),
            )
            .await
            .unwrap();
        assert!(output.output.contains("stderr_out"));
    }

    /// A foreground command that outlives its timeout is not killed and not
    /// orphaned: the call answers with a job handle and the command keeps
    /// running until it finishes.
    #[tokio::test]
    async fn foreground_timeout_moves_the_command_to_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let input = ToolInput {
            call_id: "2".into(),
            name: "Bash".into(),
            arguments: serde_json::json!({ "command": "sleep 1.5; echo late", "timeout": 1 }),
        };
        let output = BashTool::new()
            .execute(input, test_ctx(dir.path()))
            .await
            .unwrap();

        let handle: serde_json::Value = serde_json::from_str(&output.output).unwrap();
        let id = handle["job_id"].as_str().unwrap().to_string();
        assert!(
            handle["note"].as_str().unwrap().contains("background"),
            "the handle should say the command keeps running: {}",
            output.output
        );

        let done = wait_for_terminal(&JobStore::new(dir.path()), &id).await;
        assert_eq!(done.state, JobState::Done);
        let captured =
            std::fs::read_to_string(dir.path().join(".coderex/jobs").join(format!("{id}.out")))
                .unwrap();
        assert!(
            captured.contains("late"),
            "output written after promotion was lost: {captured}"
        );
    }

    #[tokio::test]
    async fn nonexistent_command_errors() {
        let output = BashTool::new()
            .execute(
                bash_input("nonexistent_command_xyz_123"),
                test_ctx(Path::new("/tmp")),
            )
            .await
            .unwrap();
        assert!(output.output.contains("Exit code:"));
        // Just verify it failed — exact exit code varies by shell
        assert!(output.output.contains("Exit code: ") && !output.output.contains("Exit code: 0"));
    }

    #[tokio::test]
    async fn exit_code_captured() {
        let output = BashTool::new()
            .execute(bash_input("exit 42"), test_ctx(Path::new("/tmp")))
            .await
            .unwrap();
        assert!(output.output.contains("Exit code: 42"));
    }

    /// `cd` moves the next call too, and the sentinel that tracks it does not
    /// disturb the exit code.
    #[tokio::test]
    async fn cwd_persists_between_calls() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        // The tracker records `pwd -P`, the *physical* path. On macOS `/var` is
        // a symlink to `/private/var`, so the tempdir this test built and the
        // path the shell reports are two spellings of one directory. Compare
        // against the canonical one, which is what the shell will report.
        let sub = std::fs::canonicalize(&sub).unwrap();
        let shared = std::sync::Arc::new(parking_lot::RwLock::new(
            std::fs::canonicalize(dir.path()).unwrap(),
        ));

        let first = BashTool::new()
            .execute(
                bash_input(&format!("cd '{}'", sub.display())),
                ctx_with_session_cwd(dir.path(), &shared),
            )
            .await
            .unwrap();
        assert!(first.output.contains("Exit code: 0"), "{}", first.output);
        assert_eq!(*shared.read(), sub);

        let second = BashTool::new()
            .execute(bash_input("pwd"), ctx_with_session_cwd(dir.path(), &shared))
            .await
            .unwrap();
        assert!(
            second.output.contains(sub.to_str().unwrap()),
            "the second call did not start in the tracked directory: {}",
            second.output
        );
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn sandboxed_echo_works() {
        use std::collections::HashMap;
        use std::path::PathBuf;

        let mut extra = HashMap::new();
        extra.insert(
            "sandbox_config".to_string(),
            Box::new(coderex_core::sandbox::SandboxConfig {
                allowed_read_paths: vec![PathBuf::from("/tmp")],
                allowed_write_paths: vec![PathBuf::from("/tmp")],
                allow_network: false,
                ..Default::default()
            }) as crate::tool::SharedState,
        );
        let ctx = ToolContext::new(ExecutionContext::Normal, Path::new("/tmp")).with_extra(extra);

        let output = BashTool::new()
            .execute(bash_input("echo sandboxed"), ctx)
            .await
            .unwrap();
        eprintln!("SANDBOX OUTPUT: {:?}", output.output);
        assert!(
            output.output.contains("sandboxed"),
            "output was: {}",
            output.output
        );
        assert!(
            output.output.contains("Exit code: 0"),
            "output was: {}",
            output.output
        );
    }

    /// The Linux counterpart of `sandboxed_echo_works`: the Bash tool's own
    /// spawn path, under the wrapper its policy produces.
    ///
    /// The macOS test above runs `sandbox-exec`; this one is what proves the
    /// Linux branch — `bwrap` for a mount namespace, `prepare` for rlimits —
    /// actually spawns a command. Skipped when bubblewrap is not in force,
    /// because without it the branch deliberately falls back to the syscall
    /// filter.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn sandboxed_echo_works_on_linux() {
        use std::collections::HashMap;
        use std::path::PathBuf;

        if !coderex_core::sandbox::bubblewrap_in_force() {
            eprintln!("skipping: bubblewrap is not in force");
            return;
        }

        let dir = std::env::temp_dir().join(format!("coderex-bash-sandbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir_buf = PathBuf::from(&dir);

        let mut extra = HashMap::new();
        extra.insert(
            "sandbox_config".to_string(),
            Box::new(coderex_core::sandbox::SandboxConfig {
                allowed_read_paths: vec![dir_buf.clone()],
                allowed_write_paths: vec![dir_buf.clone()],
                additional_working_dirs: vec![dir_buf],
                allow_network: false,
                ..Default::default()
            }) as crate::tool::SharedState,
        );
        let ctx = ToolContext::new(ExecutionContext::Normal, &dir).with_extra(extra);

        let output = BashTool::new()
            .execute(bash_input("echo sandboxed; echo written > inside.txt"), ctx)
            .await
            .unwrap();

        assert!(
            output.output.contains("sandboxed"),
            "output was: {}",
            output.output
        );
        assert!(
            output.output.contains("Exit code: 0"),
            "output was: {}",
            output.output
        );
        assert!(
            dir.join("inside.txt").exists(),
            "the sandboxed command could not write to the allowed path"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fallback a Linux machine without `bwrap` takes: `prepare` installs
    /// rlimits and the syscall table instead of a mount namespace.
    ///
    /// This is the path that used to take every command down with SIGSYS — the
    /// filter compared the syscall number with the architecture token, so it
    /// never matched and killed the process at its first syscall.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn sandboxed_echo_works_without_bubblewrap() {
        use std::collections::HashMap;
        use std::path::PathBuf;

        if coderex_core::sandbox::bubblewrap_in_force() {
            eprintln!("skipping: bubblewrap is in force; set CODEREX_BWRAP=0 for the syscall path");
            return;
        }

        let dir = std::env::temp_dir().join(format!("coderex-bash-seccomp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir_buf = PathBuf::from(&dir);

        let mut extra = HashMap::new();
        extra.insert(
            "sandbox_config".to_string(),
            Box::new(coderex_core::sandbox::SandboxConfig {
                allowed_read_paths: vec![dir_buf.clone()],
                allowed_write_paths: vec![dir_buf.clone()],
                additional_working_dirs: vec![dir_buf],
                allow_network: false,
                ..Default::default()
            }) as crate::tool::SharedState,
        );
        let ctx = ToolContext::new(ExecutionContext::Normal, &dir).with_extra(extra);

        let output = BashTool::new()
            .execute(bash_input("echo fallback"), ctx)
            .await
            .unwrap();

        assert!(
            output.output.contains("fallback"),
            "the syscall table broke a trivial command: {}",
            output.output
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A shell that can still read `PATH` and `HOME` but not the session's
    /// credentials. The variable is set in this process for the duration of
    /// the call; nothing else in the crate reads it.
    // `"${ANTHROPIC_AUTH_TOKEN:-STRIPPED}"` reads like a format argument but
    // is shell parameter expansion.
    #[allow(clippy::literal_string_with_formatting_args)]
    #[tokio::test]
    #[allow(unsafe_code)]
    async fn child_does_not_inherit_provider_credentials() {
        const KEYS: [&str; 3] = [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ];
        let previous: Vec<_> = KEYS.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for key in KEYS {
            // SAFETY: edition 2024 marks `set_var` unsafe because another
            // thread could read the environment concurrently. The only keys
            // touched are the three above, which no other test reads.
            unsafe { std::env::set_var(key, "coderex-test-secret") };
        }

        let output = BashTool::new()
            .execute(
                bash_input("printf %s \"${ANTHROPIC_AUTH_TOKEN:-STRIPPED} ${CLAUDE_CODE_OAUTH_TOKEN:-STRIPPED}\""),
                test_ctx(Path::new("/tmp")),
            )
            .await
            .unwrap();

        for (key, value) in previous {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }

        assert!(
            output.output.contains("STRIPPED STRIPPED"),
            "credentials reached the child: {}",
            output.output
        );
        assert!(!output.output.contains("coderex-test-secret"));
    }

    /// Stripping credentials must not become clearing the environment.
    // `"${HOME:+HAS_HOME}"` reads like a format argument but is shell syntax.
    #[allow(clippy::literal_string_with_formatting_args)]
    #[tokio::test]
    async fn child_keeps_ordinary_environment() {
        let output = BashTool::new()
            .execute(
                bash_input("printf %s \"${HOME:+HAS_HOME}${PATH:+HAS_PATH}\""),
                test_ctx(Path::new("/tmp")),
            )
            .await
            .unwrap();
        assert!(
            output.output.contains("HAS_HOMEHAS_PATH"),
            "ordinary variables were stripped too: {}",
            output.output
        );
    }

    #[test]
    fn program_resolving_inside_cwd_is_refused() {
        let dir = std::env::temp_dir().join(format!("coderex_bash_guard_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let planted = dir.join("coderex_planted_tool");
        std::fs::write(&planted, b"#!/bin/sh\necho planted\n").unwrap();

        let error =
            crate::sandbox_shell::guard_program_origin("./coderex_planted_tool", &dir).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("resolves inside the working directory"),
            "unexpected error: {error}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_shell_is_not_refused() {
        // The ordinary case: `sh` comes from the system, not the workspace.
        crate::sandbox_shell::guard_program_origin("sh", Path::new("/tmp")).unwrap();
    }

    fn background_input(command: &str, timeout: u64) -> ToolInput {
        ToolInput {
            call_id: "bg".into(),
            name: "Bash".into(),
            arguments: serde_json::json!({
                "command": command,
                "timeout": timeout,
                "run_in_background": true,
            }),
        }
    }

    async fn wait_for_terminal(store: &JobStore, id: &str) -> coderex_core::job_record::JobRecord {
        for _ in 0..200 {
            if let Ok(Some(record)) = store.load(id)
                && record.state != JobState::Running
            {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("background job {id} never reached a terminal state");
    }

    fn ctx_with_session_cwd<'a>(
        cwd: &'a Path,
        shared: &std::sync::Arc<parking_lot::RwLock<PathBuf>>,
    ) -> ToolContext<'a> {
        let mut extra = std::collections::HashMap::<String, crate::tool::SharedState>::new();
        extra.insert(
            crate::tool::SESSION_CWD_KEY.to_string(),
            Box::new(std::sync::Arc::clone(shared)),
        );
        ToolContext::new(ExecutionContext::Normal, cwd).with_extra(extra)
    }

    #[tokio::test]
    async fn background_job_is_recorded_running_then_done() {
        let dir = tempfile::tempdir().unwrap();
        let output = BashTool::new()
            .execute(
                background_input("sleep 0.3; echo background-done", 10),
                test_ctx(dir.path()),
            )
            .await
            .unwrap();

        let handle: serde_json::Value = serde_json::from_str(&output.output).unwrap();
        let id = handle["job_id"].as_str().unwrap().to_string();
        assert!(
            !id.contains('/') && !id.contains('\\'),
            "job id must be a safe file stem: {id}"
        );
        assert_eq!(handle["note"], "command running in background");

        let store = JobStore::new(dir.path());
        let running = store.load(&id).unwrap().expect("record written at spawn");
        assert_eq!(running.state, JobState::Running);
        assert_eq!(running.kind, JobKind::Shell);
        assert!(running.pid.is_some());
        assert!(running.ended_at_ms.is_none());
        assert!(running.description.contains("background-done"));

        let done = wait_for_terminal(&store, &id).await;
        assert_eq!(done.state, JobState::Done);
        assert_eq!(done.exit_code, Some(0));
        assert!(done.error.is_none());
        assert!(done.ended_at_ms.is_some());
        assert!(done.ended_at_ms.unwrap() >= done.started_at_ms);

        let captured =
            std::fs::read_to_string(dir.path().join(".coderex/jobs").join(format!("{id}.out")))
                .unwrap();
        assert!(captured.contains("background-done"));
    }

    /// The point of writing straight to the file: a service's output is there
    /// to read while the service is still running.
    #[tokio::test]
    async fn background_output_is_readable_while_running() {
        let dir = tempfile::tempdir().unwrap();
        // No `timeout`: a background job without one runs until it exits.
        let input = ToolInput {
            call_id: "bg".into(),
            name: "Bash".into(),
            arguments: serde_json::json!({
                "command": "echo started; sleep 30",
                "run_in_background": true,
            }),
        };
        let output = BashTool::new()
            .execute(input, test_ctx(dir.path()))
            .await
            .unwrap();
        let handle: serde_json::Value = serde_json::from_str(&output.output).unwrap();
        let id = handle["job_id"].as_str().unwrap().to_string();
        let pid = u32::try_from(handle["pid"].as_u64().unwrap()).unwrap();
        let path = dir.path().join(".coderex/jobs").join(format!("{id}.out"));

        let mut seen = String::new();
        for _ in 0..100 {
            seen = std::fs::read_to_string(&path).unwrap_or_default();
            if seen.contains("started") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            seen.contains("started"),
            "the running job's output was not in its file: {seen:?}"
        );

        job_control::terminate_group(pid).await;
        let _ = wait_for_terminal(&JobStore::new(dir.path()), &id).await;
    }

    #[tokio::test]
    async fn background_failure_records_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let output = BashTool::new()
            .execute(
                background_input("sleep 0.2; exit 7", 10),
                test_ctx(dir.path()),
            )
            .await
            .unwrap();
        let handle: serde_json::Value = serde_json::from_str(&output.output).unwrap();
        let id = handle["job_id"].as_str().unwrap().to_string();

        let failed = wait_for_terminal(&JobStore::new(dir.path()), &id).await;
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.exit_code, Some(7));
        assert!(failed.error.unwrap().contains("status 7"));
        assert!(failed.ended_at_ms.is_some());
    }

    /// A background job is only stopped by a timeout the caller asked for.
    #[tokio::test]
    async fn background_timeout_settles_failed_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let output = BashTool::new()
            .execute(background_input("sleep 30", 1), test_ctx(dir.path()))
            .await
            .unwrap();
        let handle: serde_json::Value = serde_json::from_str(&output.output).unwrap();
        let id = handle["job_id"].as_str().unwrap().to_string();

        let timed_out = wait_for_terminal(&JobStore::new(dir.path()), &id).await;
        assert_eq!(timed_out.state, JobState::Failed);
        assert!(timed_out.error.unwrap().contains("timed out"));
        assert!(timed_out.ended_at_ms.is_some());
    }
}
