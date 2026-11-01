//! Runtime supervision for plugin `monitors`.
//!
//! A monitor is a background watch script a plugin asks the host to keep alive
//! for the session — "background watch scripts the host arms as persistent
//! Monitor tasks". Monitors sit at the same trust tier as hooks: they run
//! unsandboxed, for as long as the session lasts, and this process's provider
//! credentials are removed from the environment they inherit.
//!
//! Both declared document shapes are accepted. An object maps monitor names to
//! their definitions:
//!
//! ```json
//! {
//!   "watch": { "command": "watchexec", "args": ["-w", "src"] },
//!   "lint": { "script": "scripts/lint-watch.sh", "env": { "RUST_LOG": "info" } }
//! }
//! ```
//!
//! An array carries the name on each entry instead:
//!
//! ```json
//! [{ "name": "watch", "command": "watchexec" }]
//! ```
//!
//! A relative `script` path is resolved against the supplied working
//! directory. Invalid entries are logged and skipped: one malformed monitor,
//! or one that cannot be spawned, must not keep the session or the other
//! monitors from running.
//!
//! A monitor can outlive its host: `SIGKILL` runs no signal handler and no
//! destructor. On Unix the host therefore starts a small guardian process
//! (`--internal-monitor-guardian`) and feeds it `(pid, start time)` records;
//! when the host dies the pipe reaches EOF and the guardian kills the recorded
//! processes, re-checking the start time so a reused pid is left alone. Linux
//! also records the monitor's cgroup, whose `cgroup.kill` reaches a daemonised
//! descendant. Windows needs no guardian: its Job Object is kill-on-close, so
//! the kernel does the same when the host's handles go away.

use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
use coderex_core::process_tree::tracked_descendants;
use coderex_core::process_tree::{JobLimits, ProcessTree};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use coderex_core::process_tree::{TrackedProcess, kill_tracked_process, process_start_time};
#[cfg(target_os = "linux")]
use coderex_core::sandbox::CgroupMemoryLimit;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

/// Whole-tree ownership for one monitor.
///
/// A [`ProcessTree`] is the primary owner: a Windows Job Object or a Unix
/// process group. A monitor that calls `setsid()` leaves the process group,
/// so on Linux the monitor is additionally placed in a cgroup v2 group —
/// killing the group stops the whole tree however the monitor rearranged its
/// process groups. On macOS there is no cgroup; the descendant walk in
/// [`ProcessTree::terminate`] reaches a `setsid()` child while its parent
/// chain is intact, and [`MonitorOwnership::seen`] keeps the records that
/// outlive a chain broken by an exiting intermediate parent.
#[derive(Debug)]
struct MonitorOwnership {
    tree: Arc<ProcessTree>,
    #[cfg(target_os = "linux")]
    cgroup: Option<CgroupMemoryLimit>,
    /// macOS: descendants observed while the monitor was alive.
    ///
    /// A double-forked descendant is re-parented to `launchd` when its
    /// intermediate parent exits. At cleanup the live walk starts from the
    /// monitor and no longer has a chain to it, so the sampler records it
    /// (pid plus kernel start time) while the monitor is still running.
    #[cfg(target_os = "macos")]
    seen: Mutex<Vec<TrackedProcess>>,
}

impl MonitorOwnership {
    /// Kill the monitor and everything it started. Returns whether a kill was
    /// issued.
    ///
    /// `from_signal` selects how the macOS record lock is taken: a signal
    /// handler must not block on a sampler that holds it, while `stop`/`drop`
    /// can wait.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn terminate(&self, from_signal: bool) -> bool {
        #[cfg(target_os = "linux")]
        {
            // The cgroup reaches a `setsid()` descendant that the process
            // group does not; either kill counts as one.
            let cgroup = self.cgroup.as_ref().is_some_and(CgroupMemoryLimit::kill);
            self.tree.terminate() || cgroup
        }
        #[cfg(target_os = "macos")]
        {
            // The records reach a descendant that has been re-parented to
            // launchd; the live walk in `tree.terminate()` cannot.
            let recorded = self.kill_recorded(from_signal);
            self.tree.terminate() || recorded
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            self.tree.terminate()
        }
    }

    /// `SIGKILL` every recorded descendant whose pid still names it.
    ///
    /// Reading and signalling recorded pids is bounded and allocation-free.
    /// The start-time check happens inside [`kill_tracked_process`], so a
    /// reused pid is never signalled. `try_only` is for the SIGTERM handler and
    /// the panic hook: a miss there still leaves the live walk in
    /// `tree.terminate()`.
    #[cfg(target_os = "macos")]
    fn kill_recorded(&self, try_only: bool) -> bool {
        let recorded = if try_only {
            let Ok(recorded) = self.seen.try_lock() else {
                return false;
            };
            recorded
        } else {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let mut issued = false;
        for process in recorded.iter().rev() {
            issued |= kill_tracked_process(*process);
        }
        issued
    }

    /// Stop owning the tree: the monitor exited on its own, so its remaining
    /// descendants are not ours to kill.
    fn disarm(&self) {
        self.tree.disarm();
    }
}

/// Every live monitor's ownership, so a signal handler can kill it.
///
/// `Drop` is not enough: SIGTERM takes `process::exit` and release builds use
/// `panic = "abort"`, neither of which runs destructors. The registry is the
/// only cleanup that survives those paths.
///
/// The value is a [`MonitorOwnership`], not a pid: on Windows a pid only names
/// the direct child, and `taskkill /T` is best-effort at reaching
/// grandchildren. The Job Object kills all of them without spawning a helper
/// process from a signal handler.
static MONITOR_TREES: Mutex<Vec<(u64, Arc<MonitorOwnership>)>> = Mutex::new(Vec::new());

/// Ids are assigned monotonically so two monitors cannot unregister each
/// other when a pid is reused.
static NEXT_MONITOR_ID: AtomicU64 = AtomicU64::new(1);

fn register_monitor(ownership: Arc<MonitorOwnership>) -> u64 {
    let id = NEXT_MONITOR_ID.fetch_add(1, Ordering::Relaxed);
    MONITOR_TREES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((id, ownership));
    id
}

fn unregister_monitor(id: u64) {
    let mut trees = MONITOR_TREES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    trees.retain(|(existing, _)| *existing != id);
}

/// Kill every live plugin monitor and anything it started.
///
/// Called from the SIGTERM handler and the panic hook, where no destructor
/// will run. Idempotent; safe when no monitor is armed.
///
/// Everything here is bounded and allocation-light and spawns no process: a
/// Windows `TerminateJobObject`, a Unix `kill`, and — on Linux — a write to
/// `cgroup.kill` or a read of `cgroup.procs`. That is what a signal handler
/// can afford.
pub fn terminate_all_monitors() {
    let monitors = std::mem::take(
        &mut *MONITOR_TREES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for (_, ownership) in monitors {
        // Signal context: never block on the macOS sampler's record lock.
        ownership.terminate(true);
    }
}

/// Most monitors one session will arm.
///
/// A plugin document that names more is truncated at this count, with one
/// warning, so a runaway manifest cannot exhaust the process table.
pub const MAX_MONITORS: usize = 32;

/// Most bytes of a single monitor output line that are written to the log.
const MAX_LOG_LINE_BYTES: usize = 8 * 1024;

/// macOS: how often a monitor's descendants are sampled just after it starts.
///
/// A daemonising monitor forks almost immediately, so the first couple of
/// seconds need a fine interval to see the child before its intermediate
/// parent exits.
#[cfg(target_os = "macos")]
const DESCENDANT_SAMPLE_FAST: std::time::Duration = std::time::Duration::from_millis(25);

/// macOS: how often descendants are sampled after the fast window.
#[cfg(target_os = "macos")]
const DESCENDANT_SAMPLE_SLOW: std::time::Duration = std::time::Duration::from_millis(500);

/// macOS: how long the fast sampling interval lasts after a monitor starts.
#[cfg(target_os = "macos")]
const DESCENDANT_SAMPLE_FAST_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// macOS: most descendant records kept for one monitor.
///
/// Far more than a monitor normally starts; the cap stops a runaway monitor
/// from growing the record list without bound over a long session.
#[cfg(target_os = "macos")]
const MAX_TRACKED_DESCENDANTS: usize = 4096;

// ─── Host-death guardian ────────────────────────────────────────────────────

/// The hidden subcommand the monitor guardian runs as.
///
/// The host starts itself with this argument and keeps its stdin open. When
/// the host dies — including `SIGKILL`, where no destructor, `Drop` or signal
/// handler runs — the pipe reaches EOF and the guardian kills everything the
/// host had recorded. Windows needs no guardian: its Job Object is
/// kill-on-close, so the kernel does this when the host's handles go away.
pub const GUARDIAN_FLAG: &str = "--internal-monitor-guardian";

/// Records one guardian will hold. Far beyond the monitor cap; a bound keeps a
/// buggy host from growing the guardian without limit.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const MAX_GUARDIAN_RECORDS: usize = 8192;

/// The guardian process, once started. Its stdin is the record pipe.
#[cfg(any(target_os = "macos", target_os = "linux"))]
static MONITOR_GUARDIAN: Mutex<Option<std::process::Child>> = Mutex::new(None);

/// Run the guardian when this process was started with the hidden flag.
///
/// Returns the exit code to use, or `None` for an ordinary launch.
#[must_use]
// Outside macOS/Linux the body is only `None`, which clippy would prefer as a
// `const fn`; the real body reads the environment and cannot be one.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    allow(clippy::missing_const_for_fn)
)]
pub fn maybe_run_guardian() -> Option<i32> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut args = std::env::args_os();
        let _ = args.next();
        if args.next().as_deref() == Some(std::ffi::OsStr::new(GUARDIAN_FLAG)) {
            return Some(run_guardian(std::io::stdin().lock()));
        }
    }
    None
}

/// Read records until EOF, then kill what they name.
///
/// `pid <pid> <start>` is killed only when the pid still carries the recorded
/// start time. `cgroup <path>` (Linux) gets `cgroup.kill`, which reaches a
/// `setsid()` descendant the pid records may not. EOF is the signal that the
/// host is gone; nothing here needs the host to cooperate.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn run_guardian(reader: impl std::io::BufRead) -> i32 {
    let mut processes: Vec<TrackedProcess> = Vec::new();
    let mut cgroups: Vec<std::path::PathBuf> = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();
        if let Some(record) = line.strip_prefix("pid ") {
            let mut fields = record.split_whitespace();
            if let (Some(pid), Some(start)) = (fields.next(), fields.next())
                && let (Ok(pid), Ok(start_time)) = (pid.parse::<u32>(), start.parse::<u64>())
                && processes.len() < MAX_GUARDIAN_RECORDS
            {
                processes.push(TrackedProcess { pid, start_time });
            }
        } else if let Some(path) = line.strip_prefix("cgroup ")
            && cgroups.len() < MAX_GUARDIAN_RECORDS
        {
            cgroups.push(std::path::PathBuf::from(path));
        }
    }

    for process in &processes {
        let _ = kill_tracked_process(*process);
    }
    #[cfg(target_os = "linux")]
    for path in &cgroups {
        let _ = std::fs::write(path.join("cgroup.kill"), "1");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cgroups;
    0
}

/// Start the guardian if it is not already running.
///
/// Returns whether records can be sent. Unit tests run inside the test-harness
/// binary, which does not handle the hidden flag, so they exercise
/// [`run_guardian`] directly instead.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn ensure_guardian() -> bool {
    if cfg!(test) {
        return false;
    }
    let mut guardian = MONITOR_GUARDIAN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(child) = guardian.as_mut() {
        // Reap a guardian that died; a later record may start a fresh one.
        if child.try_wait().ok().flatten().is_none() {
            return true;
        }
        *guardian = None;
    }

    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    match spawn_guardian(&exe) {
        Ok(child) => {
            *guardian = Some(child);
            true
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "could not start the monitor guardian; monitors may survive a host SIGKILL"
            );
            false
        }
    }
}

/// Spawn the guardian process for `exe`.
///
/// The guardian leads its **own process group**. A `killpg` on the host's
/// group — or a SIGHUP from the terminal closing — must not take the one
/// process that is supposed to outlive the host down with it. The pipe stays
/// the only channel, and EOF on it is still what triggers cleanup.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn spawn_guardian(exe: &std::path::Path) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt as _;

    let mut command = std::process::Command::new(exe);
    command
        .arg(GUARDIAN_FLAG)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // The guardian needs nothing from this session, least of all its
        // provider credentials.
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());
    command.process_group(0);
    command.spawn()
}

/// Send one record to the guardian, if it is running.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn guardian_send(line: &str) {
    use std::io::Write as _;

    let mut guardian = MONITOR_GUARDIAN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(child) = guardian.as_mut() else {
        return;
    };
    let Some(stdin) = child.stdin.as_mut() else {
        return;
    };
    let _ = stdin.write_all(line.as_bytes());
    let _ = stdin.flush();
}

/// Record a monitor the guardian should kill if this host disappears.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn guardian_record_monitor(pid: u32, ownership: &MonitorOwnership) {
    if !ensure_guardian() {
        return;
    }
    if let Some(start) = process_start_time(pid) {
        guardian_send(&format!("pid {pid} {start}\n"));
    }
    #[cfg(target_os = "linux")]
    if let Some(cgroup) = &ownership.cgroup {
        guardian_send(&format!("cgroup {}\n", cgroup.path().display()));
    }
    let _ = ownership;
}

/// Owns the running tasks of a session's plugin monitors.
///
/// Dropping the supervisor aborts every task. Each task owns its child with
/// `kill_on_drop(true)`, so aborting the task also kills the process; [`stop`]
/// further waits for the aborts to finish, which is what a graceful shutdown
/// should call.
///
/// [`stop`]: PluginMonitorSupervisor::stop
#[derive(Debug)]
pub struct PluginMonitorSupervisor {
    monitors: Vec<MonitorTask>,
}

/// One armed monitor: its name, for logging, the task that owns the child, and
/// the whole-tree ownership that outlives it.
#[derive(Debug)]
struct MonitorTask {
    name: String,
    task: JoinHandle<()>,
    ownership: Arc<MonitorOwnership>,
    /// macOS: the task that samples descendants while the monitor is alive.
    #[cfg(target_os = "macos")]
    tracker: JoinHandle<()>,
}

/// One validated monitor definition, before it is spawned.
#[derive(Debug)]
struct MonitorSpec {
    name: String,
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl PluginMonitorSupervisor {
    /// Arm every valid monitor in `monitors`, resolving relative `script`
    /// paths against `cwd`.
    ///
    /// Monitors are spawned with a null stdin and piped stdout/stderr. Their
    /// output is drained line by line into the log under the monitor's name:
    /// stdout at `info`, stderr at `warn`. Individual lines longer than
    /// 8 KiB are truncated.
    ///
    /// # Errors
    ///
    /// Returns `Err` when `monitors` is neither an object nor an array, or
    /// when called outside a Tokio runtime. A single monitor that fails to
    /// spawn is logged and skipped instead of failing the whole call.
    pub fn start(monitors: &Value, cwd: &Path) -> Result<Self, String> {
        if tokio::runtime::Handle::try_current().is_err() {
            return Err("plugin monitors must be started from within a Tokio runtime".to_string());
        }

        let mut supervisor = Self {
            monitors: Vec::new(),
        };
        let mut over_cap = 0usize;

        for spec in monitor_specs(monitors, cwd)? {
            if supervisor.monitors.len() >= MAX_MONITORS {
                over_cap += 1;
                continue;
            }
            let name = spec.name.clone();
            match spawn_monitor(spec, cwd) {
                Ok(monitor) => supervisor.monitors.push(monitor),
                Err(error) => {
                    tracing::warn!(
                        monitor = %name,
                        %error,
                        "plugin monitor failed to start; skipping it"
                    );
                }
            }
        }

        if over_cap > 0 {
            tracing::warn!(
                max = MAX_MONITORS,
                skipped = over_cap,
                "plugin monitor limit reached; extra monitors were not started"
            );
        }

        Ok(supervisor)
    }

    /// Number of monitors whose task has not finished.
    ///
    /// A monitor that exited on its own no longer counts as running; its exit
    /// has already been logged.
    #[must_use]
    pub fn running(&self) -> usize {
        self.monitors
            .iter()
            .filter(|monitor| !monitor.task.is_finished())
            .count()
    }

    /// Kill, abort and wait for every monitor.
    ///
    /// Idempotent: a second call, or a call after the monitors exited on
    /// their own, does nothing. Aborting a task drops its child, which
    /// `kill_on_drop(true)` turns into a `SIGKILL` on Unix; that only reaches
    /// the direct child, so the whole tree is terminated *first*, while the
    /// parent still exists to hold the libproc child chain together and the
    /// descendants are still in the cgroup.
    pub async fn stop(&mut self) {
        let monitors = std::mem::take(&mut self.monitors);
        for monitor in &monitors {
            monitor.ownership.terminate(false);
            monitor.task.abort();
            #[cfg(target_os = "macos")]
            monitor.tracker.abort();
        }
        for monitor in monitors {
            match monitor.task.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    tracing::warn!(
                        monitor = %monitor.name,
                        %error,
                        "plugin monitor task failed"
                    );
                }
            }
        }
    }
}

impl Drop for PluginMonitorSupervisor {
    /// Kill and abort the monitor tasks that [`stop`](Self::stop) did not
    /// already take.
    ///
    /// The whole tree is terminated before the abort, so a monitor's
    /// descendants die too — not just the direct child that `kill_on_drop`
    /// reaches.
    fn drop(&mut self) {
        for monitor in &self.monitors {
            monitor.ownership.terminate(false);
            monitor.task.abort();
            #[cfg(target_os = "macos")]
            monitor.tracker.abort();
        }
    }
}

/// Normalise either document shape into validated specs.
///
/// An entry that does not validate is warned about and skipped; only the
/// document's top-level shape can make this fail.
fn monitor_specs(monitors: &Value, cwd: &Path) -> Result<Vec<MonitorSpec>, String> {
    let mut specs = Vec::new();
    match monitors {
        Value::Null => {}
        Value::Object(entries) => {
            for (name, value) in entries {
                if name.is_empty() {
                    tracing::warn!("skipping plugin monitor with an empty name");
                    continue;
                }
                if let Some(spec) = parse_monitor(name, value, cwd) {
                    specs.push(spec);
                }
            }
        }
        Value::Array(entries) => {
            for (index, value) in entries.iter().enumerate() {
                let Some(name) = value.get("name").and_then(Value::as_str) else {
                    tracing::warn!(index, "skipping plugin monitor entry with no string `name`");
                    continue;
                };
                if name.is_empty() {
                    tracing::warn!(index, "skipping plugin monitor entry with an empty `name`");
                    continue;
                }
                if let Some(spec) = parse_monitor(name, value, cwd) {
                    specs.push(spec);
                }
            }
        }
        other => {
            return Err(format!(
                "plugin monitors must be a JSON object or array, found {}",
                json_kind(other)
            ));
        }
    }
    Ok(specs)
}

/// Validate one monitor definition.
///
/// Returns `None`, after logging why, for anything malformed. The caller keeps
/// going: a plugin's bad entry is its own monitor's problem.
fn parse_monitor(name: &str, value: &Value, cwd: &Path) -> Option<MonitorSpec> {
    let Some(entry) = value.as_object() else {
        tracing::warn!(
            monitor = %name,
            "skipping plugin monitor: definition must be an object"
        );
        return None;
    };

    let command = entry
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty());
    let script = entry
        .get("script")
        .and_then(Value::as_str)
        .filter(|script| !script.is_empty());

    let program = if let Some(command) = command {
        if script.is_some() {
            tracing::warn!(
                monitor = %name,
                "plugin monitor declares both `command` and `script`; using `command`"
            );
        }
        command.to_string()
    } else if let Some(script) = script {
        // A script is a path, not a name to look up on `PATH`.
        let script = Path::new(script);
        if script.is_absolute() {
            script.to_string_lossy().into_owned()
        } else {
            cwd.join(script).to_string_lossy().into_owned()
        }
    } else {
        tracing::warn!(
            monitor = %name,
            "skipping plugin monitor: no non-empty `command` or `script`"
        );
        return None;
    };

    let args = match entry.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(values)) => {
            let mut args = Vec::with_capacity(values.len());
            for value in values {
                let Some(arg) = value.as_str() else {
                    tracing::warn!(
                        monitor = %name,
                        "skipping plugin monitor: every `args` entry must be a string"
                    );
                    return None;
                };
                args.push(arg.to_string());
            }
            args
        }
        Some(_) => {
            tracing::warn!(
                monitor = %name,
                "skipping plugin monitor: `args` must be an array of strings"
            );
            return None;
        }
    };

    let env = match entry.get("env") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(values)) => {
            let mut env = Vec::with_capacity(values.len());
            for (key, value) in values {
                if key.is_empty() {
                    tracing::warn!(
                        monitor = %name,
                        "skipping plugin monitor: environment variable name is empty"
                    );
                    return None;
                }
                let Some(value) = value.as_str() else {
                    tracing::warn!(
                        monitor = %name,
                        variable = %key,
                        "skipping plugin monitor: environment values must be strings"
                    );
                    return None;
                };
                env.push((key.clone(), value.to_string()));
            }
            env
        }
        Some(_) => {
            tracing::warn!(
                monitor = %name,
                "skipping plugin monitor: `env` must be an object of strings"
            );
            return None;
        }
    };

    Some(MonitorSpec {
        name: name.to_string(),
        program,
        args,
        env,
    })
}

/// Spawn one monitor and the task that owns it.
fn spawn_monitor(spec: MonitorSpec, cwd: &Path) -> std::io::Result<MonitorTask> {
    let tree = Arc::new(ProcessTree::create(JobLimits::default())?);
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // A task aborted at shutdown drops this child, and the drop kills it.
        .kill_on_drop(true);
    // Unix: own group. Windows: create suspended, so `attach_pid` runs before
    // the monitor executes and nothing it starts can escape the job.
    tree.prepare(command.as_std_mut());

    // The monitor is not a provider client. Remove the session's credentials
    // before layering the monitor's own environment on: an explicit entry is
    // the plugin asking for a variable, and wins over the removal.
    scrub_credentials(&mut command, &spec.env);
    command.envs(spec.env.iter().map(|(key, value)| (key, value)));

    // Linux: a cgroup owns the whole monitor tree, so a monitor that calls
    // `setsid()` is still stopped as a unit. Created before the spawn so the
    // group exists by the time the pid does; only the attach needs the pid.
    // Failure is not fatal — the process tree is still the primary owner, and
    // a monitor the host cannot put in a cgroup must still run.
    #[cfg(target_os = "linux")]
    let cgroup = match CgroupMemoryLimit::create_owning() {
        Ok(cgroup) => Some(cgroup),
        Err(error) => {
            tracing::debug!(
                monitor = %spec.name,
                %error,
                "no cgroup v2 group for the plugin monitor; its process tree is the only owner"
            );
            None
        }
    };

    let mut child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| std::io::Error::other("the monitor exited before it could be registered"))?;
    if let Err(error) = tree.attach_pid(pid) {
        let _ = child.start_kill();
        return Err(std::io::Error::other(format!(
            "could not place the monitor in its process tree: {error}"
        )));
    }
    #[cfg(target_os = "linux")]
    if let Some(cgroup) = &cgroup
        && let Err(error) = cgroup.attach(pid)
    {
        tracing::debug!(
            monitor = %spec.name,
            %error,
            "could not add the plugin monitor to its cgroup; a `setsid()` escape \
             would not be caught"
        );
    }

    let ownership = Arc::new(MonitorOwnership {
        tree,
        #[cfg(target_os = "linux")]
        cgroup,
        #[cfg(target_os = "macos")]
        seen: Mutex::new(Vec::new()),
    });
    let id = register_monitor(Arc::clone(&ownership));
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    guardian_record_monitor(pid, &ownership);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let name = spec.name;
    #[cfg(target_os = "macos")]
    let tracker = track_descendants(pid, process_start_time(pid), Arc::clone(&ownership));
    let task = tokio::spawn(pump(
        child,
        id,
        Arc::clone(&ownership),
        stdout,
        stderr,
        name.clone(),
    ));
    Ok(MonitorTask {
        name,
        task,
        ownership,
        #[cfg(target_os = "macos")]
        tracker,
    })
}

/// Record a monitor's descendants while it is alive (macOS).
///
/// The process group cannot reach a descendant that moved to its own group,
/// and the live libproc walk cannot reach one whose intermediate parent exited
/// and re-parented it to `launchd`. Sampling while the monitor runs is the
/// only way to know that pid at all. The interval is fine for the first two
/// seconds — a daemonising monitor forks immediately — then backs off so a
/// long-lived monitor costs one libproc walk every half second.
///
/// The task stops on its own when the monitor exits; the supervisor also
/// aborts it on `stop`/`drop` so it never outlives the ownership it holds.
#[cfg(target_os = "macos")]
fn track_descendants(
    root: u32,
    root_start_time: Option<u64>,
    ownership: Arc<MonitorOwnership>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // No start time means the monitor was already gone; and comparing the
        // start time (not just the pid) keeps a reused pid from being sampled.
        let Some(root_start_time) = root_start_time else {
            return;
        };
        let started = tokio::time::Instant::now();
        while process_start_time(root) == Some(root_start_time) {
            let descendants = tracked_descendants(root);
            if !descendants.is_empty() {
                let mut added = Vec::new();
                {
                    let mut seen = ownership
                        .seen
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    for process in descendants {
                        if seen.len() >= MAX_TRACKED_DESCENDANTS {
                            break;
                        }
                        if !seen.contains(&process) {
                            seen.push(process);
                            added.push(process);
                        }
                    }
                }
                // Tell the guardian too, so a host SIGKILL is covered as well
                // as an orderly stop.
                for process in added {
                    guardian_send(&format!("pid {} {}\n", process.pid, process.start_time));
                }
            }
            let interval = if started.elapsed() < DESCENDANT_SAMPLE_FAST_WINDOW {
                DESCENDANT_SAMPLE_FAST
            } else {
                DESCENDANT_SAMPLE_SLOW
            };
            tokio::time::sleep(interval).await;
        }
    })
}

/// Remove this session's credential variables from `command`'s environment.
///
/// The exact names come from the same helper the shell and MCP tools use. The
/// reference also treats the whole `ANTHROPIC_*`, `CLAUDE_*`, and `OTEL_*`
/// families as host credentials, so those prefixes are added on top.
/// A name the monitor names explicitly in its own `env` is not removed.
fn scrub_credentials(command: &mut Command, explicit_env: &[(String, String)]) {
    let explicit: HashSet<&str> = explicit_env.iter().map(|(key, _)| key.as_str()).collect();

    let mut keys = coderex_core::sandbox::credential_scrub_keys();
    keys.extend(
        std::env::vars_os()
            .filter_map(|(key, _)| key.into_string().ok())
            .filter(|key| is_credential_prefix(key)),
    );
    keys.sort_unstable();
    keys.dedup();

    for key in keys {
        if !explicit.contains(key.as_str()) {
            command.env_remove(key);
        }
    }
}

/// Whether `key` belongs to one of the credential families the host owns.
fn is_credential_prefix(key: &str) -> bool {
    ["ANTHROPIC_", "CLAUDE_", "OTEL_"]
        .iter()
        .any(|prefix| key.starts_with(prefix))
}

/// Own the child for its whole lifetime, draining both output streams.
///
/// Waiting for the streams to reach EOF before `wait()` means the exit is
/// observed even when the monitor is chatty. Aborting this task drops the
/// child, and `kill_on_drop` kills (and the runtime reaps) the process.
async fn pump(
    mut child: Child,
    id: u64,
    ownership: Arc<MonitorOwnership>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    name: String,
) {
    let stdout = drain_lines(stdout, &name, false);
    let stderr = drain_lines(stderr, &name, true);
    tokio::join!(stdout, stderr);

    match child.wait().await {
        Ok(status) => tracing::debug!(monitor = %name, %status, "plugin monitor exited"),
        Err(error) => {
            tracing::warn!(monitor = %name, %error, "could not wait for plugin monitor");
        }
    }
    // The monitor exited on its own. Its remaining descendants (if any) are
    // not ours to kill, so disarm before the tree is dropped.
    ownership.disarm();
    unregister_monitor(id);
}

/// Drain one child stream, logging each line with the monitor's name.
///
/// Reading bytes rather than `lines()` keeps a monitor that emits invalid
/// UTF-8 from stranding the pipe; undecodable bytes are replaced.
async fn drain_lines<R>(stream: Option<R>, name: &str, is_stderr: bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(stream) = stream else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();

    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) => return,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buffer);
                let line = line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    continue;
                }
                let line = bounded_line(line);
                if is_stderr {
                    tracing::warn!(monitor = %name, line = %line, "plugin monitor stderr");
                } else {
                    tracing::info!(monitor = %name, line = %line, "plugin monitor stdout");
                }
            }
            Err(error) => {
                tracing::warn!(
                    monitor = %name,
                    %error,
                    "could not read plugin monitor output"
                );
                return;
            }
        }
    }
}

/// Truncate an over-long line at a character boundary for logging.
fn bounded_line(line: &str) -> Cow<'_, str> {
    if line.len() <= MAX_LOG_LINE_BYTES {
        return Cow::Borrowed(line);
    }
    let mut end = MAX_LOG_LINE_BYTES;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!(
        "{}... ({} bytes truncated)",
        &line[..end],
        line.len() - end
    ))
}

/// Name a JSON value for the top-level shape error.
const fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Mutex, MutexGuard};
    // The waits that use it are Unix-only (they watch a shell monitor).
    #[cfg(unix)]
    use std::time::Duration;

    /// Tests share one global PID registry, so a test that kills every
    /// monitor must not run concurrently with the others.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Wait until `path` exists.
    ///
    /// Generous enough to survive a loaded CI machine: the assertion is about
    /// the monitor starting, not about scheduler latency.
    #[cfg(unix)]
    async fn wait_for_file(path: &Path) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !path.exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait until `path` contains `needle`, and return its contents.
    #[cfg(unix)]
    async fn wait_for_contents(path: &Path, needle: &str) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(contents) = std::fs::read_to_string(path)
                && contents.contains(needle)
            {
                return contents;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {needle:?} in {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)] // TEST_LOCK serialises monitor tests; held across awaits on purpose.
    async fn starts_an_echoing_monitor_and_reports_it_running() {
        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let monitors = json!({
            "echoer": {
                // Absolute paths: another test in this binary replaces PATH.
                "command": "/bin/sh",
                "args": ["-c", "echo monitor-ready; exec /bin/sleep 60"]
            }
        });

        let mut supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        assert_eq!(supervisor.running(), 1);

        supervisor.stop().await;
        assert_eq!(supervisor.running(), 0);
    }

    #[tokio::test]
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)] // TEST_LOCK serialises monitor tests; held across awaits on purpose.
    async fn resolves_a_relative_script_against_the_working_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        let _guard = serial();

        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("monitor.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'script-ran\\n' > marker\nexec sleep 60\n",
        )
        .expect("write monitor script");
        let mut permissions = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("make monitor executable");

        let monitors = json!({ "scripted": { "script": "monitor.sh" } });
        let mut supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        assert_eq!(supervisor.running(), 1);

        wait_for_file(&dir.path().join("marker")).await;
        assert_eq!(supervisor.running(), 1);
        supervisor.stop().await;
    }

    #[tokio::test]
    async fn skips_invalid_entries() {
        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let monitors = json!({
            "not-an-object": "nope",
            "no-program": { "args": ["-x"] },
            "empty-command": { "command": "" },
            "bad-args": { "command": "true", "args": "not-an-array" },
            "bad-arg-entry": { "command": "true", "args": [7] },
            "bad-env": { "command": "true", "env": ["nope"] },
            "bad-env-value": { "command": "true", "env": { "KEY": 7 } },
            "": { "command": "true" }
        });

        let supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        assert_eq!(supervisor.running(), 0);
    }

    #[tokio::test]
    async fn array_entries_must_carry_a_name() {
        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let monitors = json!([
            { "command": "true" },
            { "name": "", "command": "true" },
            { "name": 7, "command": "true" }
        ]);

        let supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        assert_eq!(supervisor.running(), 0);
    }

    #[tokio::test]
    async fn rejects_a_document_that_is_not_an_object_or_array() {
        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let error = PluginMonitorSupervisor::start(&json!("nope"), dir.path())
            .expect_err("a string is not a monitor document");
        assert!(
            error.contains("object or array"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)] // TEST_LOCK serialises monitor tests; held across awaits on purpose.
    async fn stop_kills_the_child() {
        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        let monitors = json!({
            "sleeper": {
                "command": "/bin/sh",
                "args": [
                    "-c",
                    "echo $$ > \"$1\"; exec /bin/sleep 60",
                    "sh",
                    pid_file.to_string_lossy()
                ]
            }
        });

        let mut supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        wait_for_file(&pid_file).await;
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("read pid file")
            .trim()
            .parse()
            .expect("parse pid");

        supervisor.stop().await;
        assert_eq!(supervisor.running(), 0);

        // The child was killed. `kill -0` also succeeds for a zombie, so poll
        // while the runtime's reaper finishes with it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let alive = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("kill -0 {pid} 2>/dev/null"))
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "child {pid} survived stop"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    #[allow(unsafe_code)] // SAFETY: the environment mutation is serialised by ENV_LOCK.
    #[allow(clippy::await_holding_lock)] // TEST_LOCK serialises monitor tests; held across awaits on purpose.
    async fn removes_credential_variables_from_the_child_environment() {
        const SCRUBBED: [(&str, &str); 4] = [
            ("ANTHROPIC_API_KEY", "test-anthropic-key"),
            ("ANTHROPIC_CUSTOM_SECRET", "test-anthropic-custom"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "test-claude-token"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "Authorization=test-otel"),
        ];
        const KEPT: (&str, &str) = ("CODEREX_MONITOR_TEST_KEEP", "kept");

        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join("env.txt");

        let guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Remember the previous values so the test leaves the process as it
        // found it.
        let previous: Vec<(&str, Option<std::ffi::OsString>)> = SCRUBBED
            .iter()
            .chain(std::iter::once(&KEPT))
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();

        // SAFETY: `ENV_LOCK` is held for the whole test, and this module is
        // the only one that mutates these names.
        unsafe {
            for (key, value) in SCRUBBED {
                std::env::set_var(key, value);
            }
            std::env::set_var(KEPT.0, KEPT.1);
        }

        let monitors = json!({
            "env-dumper": {
                "command": "/bin/sh",
                "args": [
                    "-c",
                    "env > \"$1\"",
                    "sh",
                    env_file.to_string_lossy()
                ],
                "env": {
                    "CODEREX_MONITOR_TEST_EXPLICIT": "explicit",
                    "ANTHROPIC_EXPLICIT_OVERRIDE": "explicit"
                }
            }
        });

        // Spawning snapshots the environment; restore the parent as soon as
        // the child exists.
        let mut supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        // SAFETY: as above; `ENV_LOCK` is still held.
        unsafe {
            for (key, value) in previous {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }

        // The child's environment was snapshotted at spawn, so the lock is no
        // longer needed and must not be held across the waits below.
        drop(guard);

        let contents = wait_for_contents(&env_file, "CODEREX_MONITOR_TEST_EXPLICIT=explicit").await;

        for (key, _) in SCRUBBED {
            let leaked = contents
                .lines()
                .any(|line| line.starts_with(&format!("{key}=")));
            assert!(!leaked, "{key} leaked into the monitor environment");
        }
        assert!(
            contents.contains("CODEREX_MONITOR_TEST_KEEP=kept"),
            "non-credential variables must be inherited"
        );
        assert!(
            contents.contains("ANTHROPIC_EXPLICIT_OVERRIDE=explicit"),
            "an explicitly named environment variable must win over scrubbing"
        );

        supervisor.stop().await;
    }

    /// A monitor that moves its work into a new process group — the shell's
    /// `setsid()`, via `set -m` and a background job — leaves the group a plain
    /// group kill signals. Whole-tree ownership must still take it down: the
    /// macOS descendant walk reaches it through libproc, and the Linux cgroup
    /// holds it regardless of process group.
    #[tokio::test]
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)] // TEST_LOCK serialises monitor tests; held across awaits on purpose.
    async fn a_monitor_that_leaves_its_process_group_is_still_killed() {
        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("escaped.pid");
        // `set -m` turns on job control, so the background job is its own
        // process group. `wait` keeps the monitor alive so the libproc
        // parent/child chain still names the escaped child. The path reaches
        // the inner shell through the environment, so no shell quoting is
        // needed for it.
        let script = "set -m\n/bin/sh -c 'echo $$ > \"$CODEREX_MONITOR_ESCAPE_PID\"; exec /bin/sleep 60' &\nwait\n";
        let monitors = json!({
            "escapes": {
                "command": "/bin/sh",
                "args": ["-c", script],
                "env": { "CODEREX_MONITOR_ESCAPE_PID": pid_file.to_string_lossy() }
            }
        });

        let mut supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        wait_for_file(&pid_file).await;
        let escaped: i32 = std::fs::read_to_string(&pid_file)
            .expect("read pid file")
            .trim()
            .parse()
            .expect("parse pid");

        supervisor.stop().await;
        assert_eq!(supervisor.running(), 0);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let alive = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("kill -0 {escaped} 2>/dev/null"))
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the monitor's escaped descendant {escaped} survived stop"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// A monitor whose intermediate child exits leaves the descendant
    /// re-parented to `launchd`: it is outside the process group, and the live
    /// libproc walk from the monitor no longer has a chain to it. The sampler
    /// must have recorded it while the chain still existed.
    ///
    /// This is the macOS-specific half of the daemonize escape; Linux has the
    /// cgroup and Windows has the Job Object for the same case.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // TEST_LOCK serialises monitor tests; held across awaits on purpose.
    async fn a_reparented_descendant_is_still_killed() {
        let _guard = serial();
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("orphan.pid");
        // `set -m` gives the background job its own process group. The
        // subshell that starts it exits after a second, so the sleep is
        // re-parented to launchd before `stop()` runs; only the sampler can
        // still know its pid.
        let script = "set -m\n\
             ( /bin/sh -c 'echo $$ > \"$CODEREX_MONITOR_ORPHAN_PID\"; exec /bin/sleep 60' & \
             sleep 1 )\n\
             sleep 60\n";
        let monitors = json!({
            "reparents": {
                "command": "/bin/sh",
                "args": ["-c", script],
                "env": { "CODEREX_MONITOR_ORPHAN_PID": pid_file.to_string_lossy() }
            }
        });

        let mut supervisor =
            PluginMonitorSupervisor::start(&monitors, dir.path()).expect("start monitors");
        wait_for_file(&pid_file).await;
        let orphan: i32 = std::fs::read_to_string(&pid_file)
            .expect("read pid file")
            .trim()
            .parse()
            .expect("parse pid");

        // Let the intermediate subshell exit, so cleanup cannot reach the
        // orphan through libproc any more.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        supervisor.stop().await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let alive = std::process::Command::new("kill")
                .args(["-0", &orphan.to_string()])
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the re-parented descendant {orphan} survived stop"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Wait for a `std` child to exit, bounded.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn wait_for_child_exit(child: &mut std::process::Child, budget: Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if child.try_wait().unwrap().is_some() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The guardian must not share the host's process group: a `killpg` on
    /// that group (or a terminal SIGHUP) would otherwise kill the one process
    /// meant to outlive the host.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    #[allow(unsafe_code)] // `getpgid` is the only way to read a process group.
    fn the_guardian_leads_its_own_process_group() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("guardian.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 60\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut child = spawn_guardian(&script).expect("spawn the guardian");
        let pid = libc::pid_t::try_from(child.id()).expect("pid");
        // SAFETY: `getpgid` takes plain integers; the pid belongs to the child
        // we just spawned and have not waited on.
        let (child_group, own_group) = unsafe { (libc::getpgid(pid), libc::getpgid(0)) };
        let _ = child.kill();
        let _ = child.wait();

        assert!(child_group > 0, "getpgid failed for the guardian");
        assert_ne!(
            child_group, own_group,
            "the guardian shares the host's process group"
        );
        assert_eq!(
            child_group, pid,
            "the guardian should lead its own process group"
        );
    }

    /// The guardian's whole contract: EOF means the host is gone, and what it
    /// recorded dies with it.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_guardian_kills_a_recorded_process_at_eof() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let start = coderex_core::process_tree::process_start_time(child.id())
            .expect("the child has a start time");
        let records = format!("pid {} {}\n", child.id(), start);

        assert_eq!(run_guardian(std::io::Cursor::new(records.into_bytes())), 0);
        assert!(
            wait_for_child_exit(&mut child, Duration::from_secs(5)),
            "the recorded process survived the guardian"
        );
    }

    /// A reused pid must be left alone: the start time is the whole guard
    /// against killing an unrelated process after the host is gone.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_guardian_does_not_kill_a_reused_pid() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let start = coderex_core::process_tree::process_start_time(child.id())
            .expect("the child has a start time");
        let records = format!("pid {} {}\n", child.id(), start.saturating_add(1));

        assert_eq!(run_guardian(std::io::Cursor::new(records.into_bytes())), 0);
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            child.try_wait().unwrap().is_none(),
            "a start-time mismatch must not kill the process"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    /// A signal handler cannot run destructors, so the registry is the only
    /// cleanup. `terminate_all_monitors` must kill a child without `stop()`.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // TEST_LOCK serialises monitor tests; held across awaits on purpose.
    async fn terminate_all_kills_monitors_without_drop() {
        let _guard = serial();
        let dir = tempfile::tempdir().unwrap();
        let document = serde_json::json!({
            // Absolute path: another test in this binary replaces `PATH`, and a
            // PATH lookup here would make this test depend on test order.
            "sleeper": { "command": "/bin/sh", "args": ["-c", "sleep 60"] }
        });
        let supervisor = PluginMonitorSupervisor::start(&document, dir.path()).unwrap();
        assert_eq!(supervisor.running(), 1);

        terminate_all_monitors();

        let mut running = supervisor.running();
        for _ in 0..100 {
            if running == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            running = supervisor.running();
        }
        assert_eq!(running, 0, "the monitor survived terminate_all_monitors");
    }
}
