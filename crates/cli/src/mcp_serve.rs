//! `codape mcp serve` — this binary as a standard MCP server for another agent.
//!
//! Codape already speaks MCP in one direction: it is a client of other servers,
//! and `codape ide bridge` lets an editor drive a running daemon. Neither of
//! those is a *server* in the ordinary sense — the bridge is an adapter onto a
//! daemon's socket, and its tools are a prompt/permission protocol rather than
//! this machine's capabilities. An agent that wants Codape's file, shell and
//! search tools had no way to ask for them.
//!
//! This module is that counterpart. It speaks JSON-RPC over stdio, publishes a
//! fixed set of tools, and runs them against the same registry, dispatcher,
//! sandbox and permission rules a session uses — so an MCP client gets the
//! behaviour the session has, not a second implementation of it.
//!
//! Three things make it honest rather than merely present:
//!
//! - **The published list is a promise.** [`exposed_tools`] names the tools
//!   this server offers, and a call for anything else is refused. Every
//!   built-in tool that needs a session, a model, the client's own identity or
//!   a UI is left out: `Agent`, `SendMessage`, the team and worktree tools, the
//!   MCP resource/prompt tools, the plan-mode pair, `AskUserQuestion`, `Brief`,
//!   `Config`, the cron family (a definition written here has no turn to fire
//!   into) and the engine-internal output tools.
//! - **Permission rules still apply, and decide what is advertised.** A tool
//!   the checker would answer "yes" to is listed; a tool it would stop and ask
//!   about is not listed at all, because there is nobody to ask. That is why
//!   the default mode publishes no shell and no writes — a client that wants
//!   them runs the server with `--permissions accept-edits` (edits only) or
//!   `--dangerously-skip-permissions`.
//! - **Nothing else prints to stdout.** Every diagnostic goes through
//!   `tracing` or `eprintln`, and [`is_mcp_serve`] is checked in `main` before
//!   the startup orchestrator runs — plugin output and startup narration would
//!   otherwise land in the middle of a JSON-RPC frame.

use codape_core::permissions::{PermissionChecker, PermissionMode, PermissionResult};
// The dispatcher implements this trait; the methods it adds to the server
// (`list_tools`, `rule_subjects`, `execute`) come from it, not from the type.
use codape_core::tool_executor::ToolExecutor as _;
use codape_tools::{
    ExecutionContext, RegistryToolDispatcher, TaskStore, ToolExecutor as ToolsExecutor,
    ToolRegistry, platform_shell_tool, register_builtin_tools,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// The name this server registers as.
pub const SERVER_NAME: &str = "codape";

/// The protocol revision used when the client does not name one we speak.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

/// The MCP revisions whose framing this server implements.
///
/// All three carry the same `initialize` / `tools/list` / `tools/call` shapes
/// with newline-delimited JSON-RPC frames, which is the whole of what is spoken
/// here. A client that asks for one of them gets it echoed back.
const PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

/// JSON-RPC's "invalid params" code.
const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC's "method not found" code.
const METHOD_NOT_FOUND: i64 = -32601;

/// The tools this server publishes.
///
/// The platform's shell is named by [`platform_shell_tool`] rather than spelled
/// out, so the published surface follows the one shell the binary actually
/// runs — advertising `Bash` on a machine whose shell tool is `PowerShell`
/// would be the same mistake `register_shell_tool` exists to avoid.
///
/// Deliberately absent, and why: `Agent`, `SendMessage`, `TeamCreate`,
/// `TeamDelete`, `EnterWorktree`, `ExitWorktree`, `ListMcpResources`,
/// `ReadMcpResource`, `ListMcpPrompts` and `GetMcpPrompt` need a session that
/// owns managers this process does not have; `AskUserQuestion` needs a person;
/// `EnterPlanMode`/`ExitPlanMode` need a checker the engine would enforce and
/// nothing here would; `Brief` and `Config` read state that only a session
/// populates; the cron family would store definitions no turn ever fires; and
/// `REPL`, `RemoteTrigger`, `StructuredOutput`, `SyntheticOutput`, `Echo`,
/// `NoOp` and `ToolSearch` are engine- or test-internal.
#[must_use]
pub fn exposed_tools() -> Vec<&'static str> {
    let mut tools = vec![
        // Files and the working directory.
        "Read",
        "Write",
        "Edit",
        "NotebookEdit",
        "ListDir",
        "CurrentDir",
        "Glob",
        "Grep",
        // Search and the web.
        "WebSearch",
        "WebFetch",
        // Code intelligence.
        "LSP",
        // Work tracking that outlives the call.
        "TodoWrite",
        "TaskCreate",
        "TaskGet",
        "TaskList",
        "TaskUpdate",
        "TaskOutput",
        "TaskStop",
        // Background jobs started by the shell tool.
        "JobList",
        "JobOutput",
        "JobKill",
        // Project material the client may want to load itself.
        "Skill",
    ];
    tools.push(platform_shell_tool());
    tools
}

/// The flags this server's own tool surface is judged against.
///
/// The environment's flags, unchanged. `server_tools` takes `WebSearch` and
/// `WebFetch` out of the listing only when the session's engine says the
/// provider will run them (`RegistryToolDispatcher::set_provider_web_tools`),
/// and this process has no provider — it is a tool server for another agent,
/// and handing that agent a search is the point. The bit therefore stays off
/// here, and the flag alone cannot delete anything from the published surface.
fn published_surface_flags() -> codape_config::flags::FeatureFlags {
    codape_config::flags::FeatureFlags::from_env()
}

/// Whether this process was started as `codape mcp serve`.
///
/// Parsed by hand rather than through clap because the server has to run
/// before startup: the orchestrator loads plugins (whose stdout is not ours to
/// promise) and narrates the phases it completes.
#[must_use]
pub fn is_mcp_serve() -> bool {
    let mut args = std::env::args_os();
    let _program = args.next();
    args.next().as_deref() == Some(std::ffi::OsStr::new("mcp"))
        && args.next().as_deref() == Some(std::ffi::OsStr::new("serve"))
}

/// The options this subcommand accepts.
///
/// Kept to what changes what the server *publishes* or where it works. Anything
/// that configures a turn — model, effort, plugins, output format — has no
/// meaning here and is refused rather than accepted and ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServeOptions {
    /// The directory tools resolve relative paths against.
    directory: PathBuf,
    /// The permission mode to run under, when the command line names one.
    permission_mode: Option<String>,
    /// Run with every check bypassed.
    skip_permissions: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            directory: std::env::current_dir().unwrap_or_default(),
            permission_mode: None,
            skip_permissions: false,
        }
    }
}

/// Parse the arguments after `mcp serve`.
///
/// # Errors
///
/// Returns the message to print when an option is unknown, or a value-taking
/// option has no value. Both are startup failures: a server that quietly
/// ignored `--permissions` would publish a surface the caller did not ask for.
fn parse_options(args: impl Iterator<Item = std::ffi::OsString>) -> Result<ServeOptions, String> {
    let mut options = ServeOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let arg = arg.to_string_lossy().to_string();
        match arg.as_str() {
            "--directory" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--directory needs a path".to_string())?;
                options.directory = PathBuf::from(value);
            }
            "--permissions" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--permissions needs a mode".to_string())?;
                options.permission_mode = Some(value.to_string_lossy().to_string());
            }
            "--dangerously-skip-permissions" | "--allow-dangerously-skip-permissions" => {
                options.skip_permissions = true;
            }
            // `--flag=value`, the form a hand-written MCP client config or a
            // shell wrapper is likely to use. Splitting it here keeps the
            // parser from refusing a spelling the rest of the CLI accepts.
            other if other.starts_with("--directory=") => {
                options.directory = PathBuf::from(&other["--directory=".len()..]);
            }
            other if other.starts_with("--permissions=") => {
                options.permission_mode = Some(other["--permissions=".len()..].to_string());
            }
            other => {
                return Err(format!(
                    "`codape mcp serve` does not accept '{other}' — it runs a tool server, not a \
                     session"
                ));
            }
        }
    }
    Ok(options)
}

/// Run the server to completion and return the process exit code.
pub async fn run_mcp_serve() -> i32 {
    let options = match parse_options(std::env::args_os().skip(3)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("codape mcp serve: {message}");
            return 2;
        }
    };
    let server = match Server::build(&options) {
        Ok(server) => server,
        Err(message) => {
            eprintln!("codape mcp serve: {message}");
            return 1;
        }
    };
    server.serve().await
}

/// A JSON-RPC error, as the dispatcher returns them.
type RpcResult = Result<Value, (i64, String)>;

/// The server's state across tool calls.
struct Server {
    /// Where calls execute. The registry behind it is the same one a session
    /// builds, so sandboxing, output caps and deny rules are not re-implemented.
    dispatcher: Arc<RegistryToolDispatcher>,
    /// The rules that decide what is advertised and what may run. A session
    /// normally reaches this through the engine; there is no engine here, so
    /// the checker is evaluated directly.
    checker: std::sync::Mutex<PermissionChecker>,
}

impl Server {
    /// Build the registry, dispatcher and checker the server runs with.
    fn build(options: &ServeOptions) -> Result<Self, String> {
        let cwd = options.directory.clone();
        let settings = load_settings(&cwd);

        let mode = if options.skip_permissions {
            PermissionMode::BypassPermissions
        } else {
            options
                .permission_mode
                .as_deref()
                .or(settings.permission_mode.as_deref())
                .map_or_else(|| Ok(PermissionMode::default()), str::parse)
                .map_err(|error: String| {
                    format!("{error} (valid modes: default, accept-edits, plan, dont-ask, auto, bypass-permissions)")
                })?
        };

        let mut checker = PermissionChecker::new(mode);
        // A rule that cannot be compiled is a startup failure: silently running
        // with fewer rules than the settings file asked for would publish a
        // surface the operator believes is narrower than it is.
        if let Err(error) = codape_config::permission_rules::install_settings_rules(
            &mut checker,
            settings.permissions.as_ref(),
            &cwd,
            None,
        ) {
            return Err(format!(
                "settings permission rules were not installed: {error}"
            ));
        }

        let mut registry = ToolRegistry::new();
        register_builtin_tools(&mut registry);
        let registry = Arc::new(registry);

        let mut dispatcher = RegistryToolDispatcher::new(
            registry,
            Arc::new(ToolsExecutor::new(crate::tool_sandbox::execution_config())),
            ExecutionContext::Normal,
            cwd,
        );
        dispatcher.set_feature_flags(published_surface_flags());
        // Chrome drives a browser over CDP; a tool server for another agent has
        // no tab to attach to, and none of its tools are published anyway.
        dispatcher.set_chrome_enabled(false);
        dispatcher.set_rate_limiter(Arc::new(codape_tools::RateLimiter::new()));
        dispatcher.set_session_id(format!("mcp_{}", uuid::Uuid::new_v4().simple()));

        // The task store is the one piece of session state the published tools
        // read and write, so it is attached with the same file a session uses:
        // a task an MCP client creates is one `codape` can list.
        let task_store = Arc::new(
            TaskStore::new().with_storage(codape_config::config_home().join("tasks.jsonl")),
        );
        let _ = task_store.load();
        dispatcher.set_task_store(task_store);

        Ok(Self {
            dispatcher: Arc::new(dispatcher),
            checker: std::sync::Mutex::new(checker),
        })
    }

    /// The stdio JSON-RPC loop.
    async fn serve(self) -> i32 {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(request) = serde_json::from_str::<Value>(line) else {
                // A malformed frame carries no id to answer; skipping it is the
                // only move that keeps the stream usable.
                continue;
            };
            let Some(method) = request.get("method").and_then(Value::as_str) else {
                // A response to a request this server never sends.
                continue;
            };
            // A notification has no id and takes no answer.
            let Some(id) = request.get("id").cloned() else {
                continue;
            };

            let params = request.get("params").cloned();
            let response = match self.dispatch(method, params.as_ref()).await {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err((code, message)) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": code, "message": message}
                }),
            };
            if write_frame(&response).await.is_err() {
                break;
            }
        }

        // Leaving drops the dispatcher. A background job a tool started keeps
        // running — that is what the job store is for, and `JobKill` is how the
        // next connection ends it.
        0
    }

    /// Answer one JSON-RPC request.
    async fn dispatch(&self, method: &str, params: Option<&Value>) -> RpcResult {
        match method {
            "initialize" => Ok(Self::initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tool_definitions()})),
            "tools/call" => self.call_tool(params).await,
            other => Err((
                METHOD_NOT_FOUND,
                format!("this server does not implement '{other}'"),
            )),
        }
    }

    /// The `initialize` result: protocol, capabilities, identity.
    fn initialize(params: Option<&Value>) -> Value {
        let requested = params.and_then(|params| params.get("protocolVersion"));
        let protocol = requested
            .and_then(Value::as_str)
            .filter(|version| PROTOCOL_VERSIONS.contains(version))
            .unwrap_or(DEFAULT_PROTOCOL_VERSION);
        json!({
            "protocolVersion": protocol,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Codape's file, shell and search tools, running in the working \
                             directory this server was started in. A tool that is missing was \
                             withheld by a permission rule: restart the server with \
                             `--permissions accept-edits` or `--dangerously-skip-permissions`.",
        })
    }

    /// What `tools/list` reports: the published surface, minus what the
    /// permission rules would stop to ask about.
    fn tool_definitions(&self) -> Vec<Value> {
        let published = exposed_tools();
        self.dispatcher
            .list_tools()
            .into_iter()
            .filter(|info| published.contains(&info.name.as_str()))
            .filter(|info| self.listing_allows(&info.name))
            .map(|info| {
                json!({
                    "name": info.name,
                    "description": info.description,
                    "inputSchema": info.input_schema,
                })
            })
            .collect()
    }

    /// Whether a call with no rule subjects would be allowed.
    ///
    /// This is the listing question, not the call question: a tool whose rule
    /// matches only a specific path cannot be judged without one, and offering
    /// it would be the "invitation to waste a turn" the dispatcher's own
    /// listing avoids.
    fn listing_allows(&self, tool_name: &str) -> bool {
        let exemptions = self.dispatcher.registry().rule_exemptions_for(
            tool_name,
            &serde_json::Value::Object(serde_json::Map::new()),
        );
        let Ok(checker) = self.checker.lock() else {
            return false;
        };
        is_allowed(checker.check_with_subjects(tool_name, &[], Some(exemptions)))
    }

    /// Run one tool call.
    async fn call_tool(&self, params: Option<&Value>) -> RpcResult {
        let params = params.ok_or_else(|| (INVALID_PARAMS, "missing params".to_string()))?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| (INVALID_PARAMS, "missing tool name".to_string()))?
            .to_string();
        let arguments = params
            .get("arguments")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));

        let published = exposed_tools();
        if !published.contains(&name.as_str()) {
            return Err((
                INVALID_PARAMS,
                format!(
                    "unknown tool '{name}'; this server publishes {}",
                    published.join(", ")
                ),
            ));
        }

        // The rules are judged against the call's own subjects, exactly as the
        // engine does it: a rule written against a path can only be decided
        // once the path is known.
        let subjects = self.dispatcher.rule_subjects(&name, &arguments);
        let exemptions = self
            .dispatcher
            .registry()
            .rule_exemptions_for(&name, &arguments);
        let decision = {
            let Ok(checker) = self.checker.lock() else {
                return Ok(error_content("the permission checker is unavailable"));
            };
            checker.check_with_subjects(&name, &subjects, Some(exemptions))
        };
        if !is_allowed(decision.clone()) {
            return Ok(error_content(&refusal(&name, &decision)));
        }

        let call_id = format!("mcp_{}", uuid::Uuid::new_v4().simple());
        let outcome = self.dispatcher.execute(&call_id, &name, &arguments).await;
        let result = outcome.result;
        let mut content = content_blocks(&result.output, &result.images);
        if content.is_empty() {
            content.push(json!({"type": "text", "text": "(no output)"}));
        }
        Ok(json!({"content": content, "isError": result.is_error}))
    }
}

/// Whether a decision lets the call through.
fn is_allowed(decision: PermissionResult) -> bool {
    matches!(
        decision,
        PermissionResult::Allow | PermissionResult::AllowForPath { .. }
    )
}

/// Why a call was refused, in terms the calling model can act on.
fn refusal(tool_name: &str, decision: &PermissionResult) -> String {
    let reason = match decision {
        PermissionResult::Deny { reason }
        | PermissionResult::Ask { reason }
        | PermissionResult::Elevated { reason } => reason.clone(),
        PermissionResult::Allow | PermissionResult::AllowForPath { .. } => {
            return format!("tool '{tool_name}' was refused");
        }
    };
    format!(
        "tool '{tool_name}' was refused: {reason}. There is nobody to ask here — the client's \
         user can restart this server with `--permissions accept-edits` (edits only) or \
         `--dangerously-skip-permissions` (everything), or add an allow rule to their settings."
    )
}

/// One text block, for a failure the model should read rather than a crash.
fn error_content(message: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": message}],
        "isError": true
    })
}

/// The MCP content array for a tool result.
///
/// Images travel as MCP image content so a client that renders them sees the
/// picture instead of the base64 the model was given.
fn content_blocks(output: &str, images: &[codape_core::types::ImageBlock]) -> Vec<Value> {
    let mut blocks = Vec::new();
    if !output.is_empty() {
        blocks.push(json!({"type": "text", "text": output}));
    }
    for image in images {
        blocks.push(json!({
            "type": "image",
            "data": image.data,
            "mimeType": image.media_type,
        }));
    }
    blocks
}

/// Write one JSON-RPC frame to stdout and flush it.
async fn write_frame(frame: &Value) -> std::io::Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut text = frame.to_string();
    text.push('\n');
    stdout.write_all(text.as_bytes()).await?;
    stdout.flush().await
}

/// The settings the server's permission rules come from.
///
/// Resolved without asking about workspace trust: the repository's own settings
/// are a document a stranger wrote, and a server nobody is watching is the last
/// place to grant one the `permissions.allow` it asks for. Everything the user
/// configured still applies.
fn load_settings(cwd: &Path) -> codape_config::Settings {
    let user_path = codape_config::config_home().join("settings.json");
    let project =
        crate::orchestrator::StartupOrchestrator::find_project_config(&cwd.to_string_lossy());
    let service = codape_config::ConfigService::new(user_path, project)
        .with_local_config_path(cwd.join(".codape/settings.local.json"));
    match service.resolve_with_workspace_trust(false) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("codape mcp serve: settings could not be resolved ({error}); using defaults");
            codape_config::Settings::defaults()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(args: &[&str]) -> Result<ServeOptions, String> {
        parse_options(args.iter().map(std::ffi::OsString::from))
    }

    /// A server built the way [`Server::build`] builds one, minus settings.
    ///
    /// The registry's flags have to be resolved the same way the real
    /// constructor resolves them, or a gated tool is missing from the listing
    /// here for a reason the running server does not share. The sandbox is
    /// off because a test's temporary directory is not the session's working
    /// directory and the write policy is judged against the latter.
    fn server(cwd: &Path, mode: PermissionMode) -> Server {
        let mut registry = ToolRegistry::new();
        register_builtin_tools(&mut registry);
        let dispatcher = RegistryToolDispatcher::new(
            Arc::new(registry),
            Arc::new(ToolsExecutor::new(codape_tools::ExecutionConfig {
                sandbox_enabled: false,
                ..codape_tools::ExecutionConfig::default()
            })),
            ExecutionContext::Normal,
            cwd.to_path_buf(),
        );
        dispatcher.set_feature_flags(published_surface_flags());
        dispatcher.set_chrome_enabled(false);
        Server {
            dispatcher: Arc::new(dispatcher),
            checker: std::sync::Mutex::new(PermissionChecker::new(mode)),
        }
    }

    fn listed(server: &Server) -> Vec<String> {
        server
            .tool_definitions()
            .into_iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str).map(String::from))
            .collect()
    }

    #[test]
    fn the_published_surface_has_no_duplicates_and_no_engine_tools() {
        let tools = exposed_tools();
        let unique: std::collections::HashSet<&str> = tools.iter().copied().collect();
        assert_eq!(
            unique.len(),
            tools.len(),
            "a tool is published twice: {tools:?}"
        );
        // These all need state this process does not have; publishing one
        // would advertise a capability that fails on first use.
        for absent in [
            "Agent",
            "SendMessage",
            "AskUserQuestion",
            "EnterPlanMode",
            "ExitPlanMode",
            "CronCreate",
            "Brief",
            "Config",
        ] {
            assert!(!tools.contains(&absent), "{absent} must not be published");
        }
    }

    #[test]
    fn the_published_surface_is_registered() {
        // A name in `exposed_tools` that no tool answers to would be a
        // documented capability the server cannot keep.
        let mut registry = ToolRegistry::new();
        register_builtin_tools(&mut registry);
        let names: std::collections::HashSet<String> = registry
            .all_descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect();
        for tool in exposed_tools() {
            assert!(
                names.contains(tool),
                "{tool} is published but not registered"
            );
        }
    }

    #[test]
    fn the_options_are_parsed_and_unknown_ones_refused() {
        let parsed = options(&["--directory", "/tmp/work", "--permissions", "accept-edits"])
            .expect("options parse");
        assert_eq!(parsed.directory, PathBuf::from("/tmp/work"));
        assert_eq!(parsed.permission_mode.as_deref(), Some("accept-edits"));
        assert!(!parsed.skip_permissions);

        let parsed = options(&["--dangerously-skip-permissions"]).unwrap();
        assert!(parsed.skip_permissions);

        let parsed = options(&["--directory=/srv/repo", "--permissions=accept-edits"]).unwrap();
        assert_eq!(parsed.directory, PathBuf::from("/srv/repo"));
        assert_eq!(parsed.permission_mode.as_deref(), Some("accept-edits"));

        assert!(options(&["--model", "x"]).is_err());
        assert!(options(&["--permissions"]).is_err());
        assert!(options(&["--directory"]).is_err());
    }

    /// The server has no provider to run a search for it, so `server_tools`
    /// must not take the web tools out of the surface it publishes.
    #[test]
    fn server_tools_does_not_shrink_the_published_surface() {
        use codape_config::flags::FeatureFlags;

        // The environment's flags are whatever they are; what decides the
        // retirement is the dispatcher's own bit, which only a session engine
        // sets. A tool server has no engine.
        let dir = tempfile::tempdir().unwrap();
        let server = server(dir.path(), PermissionMode::Default);
        server.dispatcher.set_feature_flags(FeatureFlags {
            server_tools: true,
            web_browser_tool: true,
            ..FeatureFlags::default()
        });
        assert!(!server.dispatcher.provider_web_tools());
        let names = listed(&server);
        assert!(names.contains(&"WebSearch".to_string()), "{names:?}");
        assert!(names.contains(&"WebFetch".to_string()), "{names:?}");
    }

    #[tokio::test]
    async fn initialize_echoes_a_protocol_it_speaks() {
        let dir = tempfile::tempdir().unwrap();
        let server = server(dir.path(), PermissionMode::Default);
        let result = server
            .dispatch(
                "initialize",
                Some(&json!({"protocolVersion": "2025-06-18", "capabilities": {}})),
            )
            .await
            .expect("initialize answers");
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert!(result["capabilities"]["tools"].is_object());

        let unknown = server
            .dispatch(
                "initialize",
                Some(&json!({"protocolVersion": "1999-01-01"})),
            )
            .await
            .unwrap();
        assert_eq!(unknown["protocolVersion"], DEFAULT_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn the_default_mode_publishes_no_shell_and_no_writes() {
        let dir = tempfile::tempdir().unwrap();
        let names = listed(&server(dir.path(), PermissionMode::Default));
        for write in ["Write", "Edit", "NotebookEdit", platform_shell_tool()] {
            assert!(
                !names.contains(&write.to_string()),
                "{write} was published without an opt-in: {names:?}"
            );
        }
        // `WebFetch` is behind the `web_browser_tool` flag, which this
        // environment may or may not have on; only tools with no gate are
        // asserted here.
        for read in ["Read", "Glob", "Grep", "ListDir"] {
            assert!(names.contains(&read.to_string()), "{read} is missing");
        }
    }

    #[tokio::test]
    async fn bypassing_permissions_publishes_the_whole_ungated_surface() {
        let dir = tempfile::tempdir().unwrap();
        let names = listed(&server(dir.path(), PermissionMode::BypassPermissions));
        // A flag-gated tool stays out until its flag is on, whatever the
        // permission mode says: those are two different gates.
        for tool in exposed_tools() {
            if codape_tools::required_feature_flag(tool).is_some() {
                continue;
            }
            assert!(names.contains(&tool.to_string()), "{tool} is missing");
        }
        assert!(names.contains(&platform_shell_tool().to_string()));
    }

    #[tokio::test]
    async fn a_published_tool_runs_and_an_unpublished_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "hello from the server").unwrap();

        let server = server(dir.path(), PermissionMode::BypassPermissions);
        let result = server
            .dispatch(
                "tools/call",
                Some(&json!({
                    "name": "Read",
                    "arguments": {"file_path": file.to_string_lossy()}
                })),
            )
            .await
            .expect("Read runs");
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("hello from the server"), "{text}");

        // `Agent` is registered but deliberately not published: this process
        // has no agent manager, so a call would report a failure pretending to
        // be a tool.
        let refused = server
            .dispatch(
                "tools/call",
                Some(&json!({"name": "Agent", "arguments": {}})),
            )
            .await;
        assert!(refused.is_err(), "an unpublished tool was run");
    }

    #[tokio::test]
    async fn a_write_is_refused_with_a_way_forward() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("written.txt");
        let server = server(dir.path(), PermissionMode::Default);
        let result = server
            .dispatch(
                "tools/call",
                Some(&json!({
                    "name": "Write",
                    "arguments": {"file_path": target.to_string_lossy(), "content": "no"}
                })),
            )
            .await
            .expect("a refusal is a tool result, not a protocol error");
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        assert!(text.contains("--permissions accept-edits"), "{text}");
        assert!(!target.exists(), "the write ran despite the refusal");
    }

    #[tokio::test]
    async fn accept_edits_publishes_writes_but_not_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("written.txt");
        let server = server(dir.path(), PermissionMode::AcceptEdits);
        let names = listed(&server);
        assert!(names.contains(&"Write".to_string()));
        assert!(!names.contains(&platform_shell_tool().to_string()));

        let result = server
            .dispatch(
                "tools/call",
                Some(&json!({
                    "name": "Write",
                    "arguments": {"file_path": target.to_string_lossy(), "content": "yes"}
                })),
            )
            .await
            .expect("Write runs under accept-edits");
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "yes");
    }

    #[tokio::test]
    async fn an_unknown_method_is_a_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let server = server(dir.path(), PermissionMode::Default);
        let error = server.dispatch("resources/list", None).await.unwrap_err();
        assert_eq!(error.0, METHOD_NOT_FOUND);
        assert!(server.dispatch("ping", None).await.is_ok());
    }
}
