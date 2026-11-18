//! Integration commands: /chrome, /desktop, /mobile, /voice, /vim, /remote-env, /remote-setup.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::fmt::Write as _;
use std::sync::Arc;

thread_local! {
    static TEST_CONFIG_HOME: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_test_config_home(path: std::path::PathBuf) {
    TEST_CONFIG_HOME.with(|h| *h.borrow_mut() = Some(path));
}

fn config_home() -> std::path::PathBuf {
    TEST_CONFIG_HOME.with(|h| {
        h.borrow()
            .clone()
            .unwrap_or_else(codape_config::settings::config_home)
    })
}

/// What a surface can do, in prose, for the commands that report one.
///
/// Spelled from the descriptor rather than from a table per surface id: the
/// capabilities are the fact, and a host that registers `desktop` with no
/// mouse would otherwise be described as the terminal is.
fn describe_capabilities(capabilities: codape_core::surface::SurfaceCapabilities) -> String {
    let mut parts = Vec::new();
    parts.push(if capabilities.interactive {
        "interactive"
    } else {
        "not interactive"
    });
    parts.push(if capabilities.mouse {
        "mouse"
    } else {
        "no mouse"
    });
    parts.push(if capabilities.images {
        "renders images"
    } else {
        "no images"
    });
    parts.join(", ")
}

/// Read the user settings.json as a JSON object.
fn read_user_settings_at(
    dir: &std::path::Path,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let path = dir.join("settings.json");
    if !path.exists() {
        return None;
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
}

/// Write the user settings.json.
fn write_user_settings_at(
    dir: &std::path::Path,
    settings: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join("settings.json");
    let value = serde_json::Value::Object(settings.clone());
    let json = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}

/// Path of the dedicated remote-environment file.
fn remote_env_path() -> std::path::PathBuf {
    config_home().join("remote_env.json")
}

/// Load remote environment variables from `config_home()/remote_env.json`.
///
/// The file is a flat `{KEY: VALUE}` JSON object. A missing file yields an
/// empty map; read and parse failures are logged and also yield an empty map.
///
/// This file is deliberately separate from `settings.json`: settings files may
/// be repository-controlled, and must never be able to inject environment
/// variables into a session.
#[must_use]
pub fn load_remote_env() -> std::collections::HashMap<String, String> {
    let path = remote_env_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return std::collections::HashMap::new();
        }
        Err(e) => {
            tracing::warn!("failed to read remote env file {path:?}: {e}");
            return std::collections::HashMap::new();
        }
    };
    serde_json::from_str(&content).unwrap_or_else(|e| {
        tracing::warn!("failed to parse remote env file {path:?}: {e}");
        std::collections::HashMap::new()
    })
}

/// Write the dedicated remote-environment file.
fn write_remote_env(env: &std::collections::HashMap<String, String>) -> Result<(), String> {
    let home = config_home();
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(env).map_err(|e| e.to_string())?;
    std::fs::write(remote_env_path(), json).map_err(|e| e.to_string())
}

/// Register integration commands.
pub fn register(registry: &mut CommandRegistry) {
    // /chrome — Chrome DevTools integration
    registry.register(SlashCommand::local(
        "chrome",
        "Chrome DevTools integration",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            let connected = std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], 9222)),
                std::time::Duration::from_millis(50),
            ).is_ok();

            if let Some(arg) = args.first()
                && arg == "--tools" {
                    return CommandResult::success(
                        "Chrome Tools\n───────────\n\
chrome_list_tabs      — List open Chrome tabs\n\
chrome_navigate       — Open a URL in a new tab\n\
chrome_execute_js     — Execute JavaScript in a tab\n\
chrome_screenshot     — Capture a screenshot (base64 PNG)\n\
chrome_get_content    — Extract page HTML as markdown"
                    );
                }

            let status = if connected {
                "Connected\nDebugger listening on port 9222."
            } else {
                "Not connected\n\nStart Chrome with:\n  google-chrome --remote-debugging-port=9222\n  # or\n  /Applications/Google\\ Chrome.app/Contents/MacOS/Google\\ Chrome --remote-debugging-port=9222"
            };
            CommandResult::success(format!(
                "Chrome DevTools\n───────────────\nStatus: {status}\n\nUse /chrome --tools to see available tools."
            ))
        }),
    ));

    // /desktop — desktop app integration
    //
    // Codape ships no desktop host of its own: the surface abstraction has one
    // implementation, the terminal (`crates/ui/src/surface.rs`). This used to
    // report "Connected" whenever `CODAPE_DESKTOP` was set, and then, once
    // that was called out, "none" no matter what was actually presenting the
    // session. The honest answer is the surface the context was built from: a
    // host that really presents sessions in a desktop window registers that
    // surface and this command reports it.
    registry.register(SlashCommand::local(
        "desktop",
        "Desktop app integration status",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let surface = &ctx.surface;
            if surface.id.as_str() == codape_core::surface::SurfaceId::DESKTOP {
                return CommandResult::success(format!(
                    "Desktop integration: active. This session is presented in the '{}' \
                     surface ({}).",
                    surface.id,
                    describe_capabilities(surface.capabilities)
                ));
            }
            CommandResult::success(format!(
                "Desktop integration: none. This session is presented in the '{}' surface \
                 ({}), not a desktop window.",
                surface.id,
                describe_capabilities(surface.capabilities)
            ))
        }),
    ));

    // /mobile — mobile remote connection
    //
    // A phone is not a surface Codape presents a session in; it reaches one
    // through `codape bridge`. So the answer has two halves, and only the
    // first is the same question `/desktop` asks: which surface is this, and
    // can a remote client attach to it at all.
    registry.register(SlashCommand::local(
        "mobile",
        "Mobile remote connection status",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let surface = &ctx.surface;
            // Whether this is a terminal is the id's question, not the
            // picture capability's: a truecolor terminal draws images and is
            // still a terminal, and a desktop host that draws none is not.
            let local_only = surface.id.as_str() == codape_core::surface::SurfaceId::TERMINAL;
            let surface_line = format!(
                "This session is presented in the '{}' surface ({}).",
                surface.id,
                describe_capabilities(surface.capabilities)
            );
            if local_only {
                CommandResult::success(format!(
                    "Mobile: no client attached. {surface_line}\n\
                     A remote client attaches through `codape bridge`, which is served by this \
                     host; nothing here can observe whether one has."
                ))
            } else {
                CommandResult::success(format!(
                    "Mobile: no client attached. {surface_line}\n\
                     A non-terminal surface is presenting this session, so a remote client may \
                     already be reaching it through that host rather than through `codape bridge`."
                ))
            }
        }),
    ));

    // /voice — report the voice input state
    //
    // Not a toggle: the microphone belongs to a command the person configures,
    // not to a flag this command can flip. `alt+m` runs that command and puts
    // its output on the prompt, so what this command can usefully say is
    // whether one is configured — which is the difference between the key
    // working and the key explaining itself.
    registry.register(SlashCommand::local(
        "voice",
        "Show voice input status",
        false,
        false,
        false,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if !ctx.is_feature_enabled("voice_mode") {
                return CommandResult::error(
                    "Voice mode is not available. Enable the voice_mode feature flag.",
                );
            }
            let settings = read_user_settings_at(&ctx.config_dir);
            let command = settings
                .as_ref()
                .and_then(|settings| settings.get("voice"))
                .and_then(|voice| voice.get("command"))
                .and_then(|command| command.as_str())
                .map(str::trim)
                .filter(|command| !command.is_empty());
            match command {
                Some(command) => CommandResult::success(format!(
                    "Voice Mode: on. `alt+m` runs this and puts its output on the prompt:\n\
                     {command}"
                )),
                None => CommandResult::success(
                    "Voice Mode: no transcription command configured.\n\
                     Set voice.command in settings.json to a command that prints \
                     the transcript on stdout, then press alt+m. The microphone \
                     is that command's; this program only reads what it prints.",
                ),
            }
        }),
    ));

    // /vim — vim mode toggle
    registry.register(SlashCommand::local(
        "vim",
        "Toggle vim mode",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let new_state = !ctx.vim_mode;
            let mut settings = read_user_settings_at(&ctx.config_dir).unwrap_or_default();
            settings.insert("vim_mode".to_string(), serde_json::Value::Bool(new_state));
            let persist_msg = match write_user_settings_at(&ctx.config_dir, &settings) {
                Ok(()) => "Saved to settings.json.".to_string(),
                Err(e) => {
                    return CommandResult::error(format!("Failed to save vim mode: {e}"));
                }
            };
            CommandResult::success(format!(
                "Vim mode: {}\n{persist_msg}",
                if new_state { "enabled" } else { "disabled" }
            ))
            .with_state_change(true)
        }),
    ));

    // /remote-env — view/set remote env vars
    registry.register(SlashCommand::local(
        "remote-env",
        "View or set remote environment variables",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                let env = load_remote_env();
                let mut output = String::from("Environment Variables:\n");
                if env.is_empty() {
                    output.push_str("  (none set)");
                } else {
                    let mut entries: Vec<_> = env.iter().collect();
                    entries.sort_by_key(|(key, _)| key.as_str());
                    for (k, v) in entries {
                        let _ = writeln!(output, "  {k}={v}");
                    }
                }
                CommandResult::success(output)
            } else {
                // Parse KEY=VALUE from the first argument
                let arg = &args[0];
                let (key, value) = if let Some(eq) = arg.find('=') {
                    (arg[..eq].to_string(), arg[eq + 1..].to_string())
                } else if args.len() >= 2 {
                    // Support: /remote-env KEY VALUE
                    (arg.clone(), args[1..].join(" "))
                } else {
                    return CommandResult::error(
                        "Usage: /remote-env KEY=VALUE or /remote-env KEY VALUE",
                    );
                };

                // Remote environment variables live in a dedicated file that
                // never participates in settings merging: settings may come
                // from a repository, which must not be able to inject env vars.
                let mut env = load_remote_env();
                env.insert(key.clone(), value.clone());

                let persist_msg = match write_remote_env(&env) {
                    Ok(()) => "Saved to remote_env.json.".to_string(),
                    Err(e) => {
                        return CommandResult::error(format!(
                            "Failed to save remote environment: {e}"
                        ));
                    }
                };
                CommandResult::success(format!(
                    "Environment variable set: {key}={value}\n{persist_msg}"
                ))
                .with_state_change(true)
            }
        }),
    ));

    // /remote-setup — remote environment setup
    registry.register(SlashCommand::local(
        "remote-setup",
        "Initialize remote environment",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            let bridge_url = std::env::var("CODAPE_BRIDGE_URL").ok();
            let api_key_set = std::env::var("ANTHROPIC_API_KEY").is_ok_and(|s| !s.is_empty())
                || std::env::var("OPENAI_API_KEY").is_ok_and(|s| !s.is_empty());
            let mut lines = vec!["Remote Setup".to_string(), "───────────".to_string()];
            if api_key_set {
                lines.push("API key: configured".to_string());
            } else {
                lines.push("API key: not set".to_string());
                lines.push("  export ANTHROPIC_API_KEY=...  # or OPENAI_API_KEY".to_string());
            }
            if let Some(url) = bridge_url.filter(|s| !s.is_empty()) {
                lines.push(format!("Bridge URL: {url}"));
                lines.push("Bridge is configured. Use /bridge to check status.".to_string());
            } else {
                lines.push("Bridge: not configured".to_string());
                lines.push("  export CODAPE_BRIDGE_URL=ws://host:port".to_string());
            }
            lines.push(String::new());
            lines.push("Run: codape serve".to_string());
            lines.push("Use Bridge for full remote session support.".to_string());
            CommandResult::success(lines.join("\n"))
        }),
    ));

    // /statusline-setup — statusline configuration wizard
    registry.register(SlashCommand::local(
        "statusline-setup",
        "Configure statusline preferences",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let format = if ctx.vim_mode {
                "[model] [tokens] [cost] [permissions] [vim]"
            } else {
                "[model] [tokens] [cost] [permissions]"
            };
            let mut settings = read_user_settings_at(&ctx.config_dir).unwrap_or_default();
            let mut statusline = settings
                .get("statusline")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            statusline.insert(
                "format".to_string(),
                serde_json::Value::String(format.to_string()),
            );
            statusline.insert(
                "theme".to_string(),
                serde_json::Value::String(ctx.theme.clone()),
            );
            settings.insert(
                "statusline".to_string(),
                serde_json::Value::Object(statusline),
            );
            let persist_msg = match write_user_settings_at(&ctx.config_dir, &settings) {
                Ok(()) => "Saved to settings.json.".to_string(),
                Err(e) => {
                    return CommandResult::error(format!(
                        "Failed to save statusline settings: {e}"
                    ));
                }
            };
            CommandResult::success(format!(
                "Statusline Setup\n────────────────\nFormat: {format}\nTheme: {}\n\n{persist_msg}",
                ctx.theme,
            ))
            .with_state_change(true)
        }),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use codape_core::types::ModelId;

    fn ctx() -> CommandContext {
        CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
    }

    /// A context whose config home is `dir`.
    ///
    /// The persisting commands (`/theme`, `/vim`, `/statusline-setup`) write
    /// under `ctx.config_dir`; leaving it at the default writes the real
    /// `~/.codape/settings.json` when the test suite runs.
    fn ctx_in(dir: &std::path::Path) -> CommandContext {
        let mut ctx = ctx();
        ctx.config_dir = dir.to_path_buf();
        ctx
    }

    #[test]
    fn chrome_not_connected() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("chrome", &ctx(), &[]);
        assert!(result.output.contains("Not connected"));
    }

    #[test]
    fn desktop_reports_the_surface_that_is_actually_presenting() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        // The default context is the terminal, so the answer is "none" — but
        // it now says which surface it *is* rather than asserting there is
        // only ever one.
        let result = reg.execute("desktop", &ctx(), &[]);
        assert!(
            result.output.contains("'terminal' surface"),
            "{}",
            result.output
        );
        assert!(
            result.output.starts_with("Desktop integration: none"),
            "{}",
            result.output
        );

        // A host that presents sessions in a desktop window registers that
        // surface; the command follows the descriptor instead of a constant.
        let mut desktop = ctx();
        desktop.surface = codape_core::surface::SurfaceDescriptor::new(
            codape_core::surface::SurfaceId::DESKTOP,
            codape_core::surface::SurfaceCapabilities {
                interactive: true,
                mouse: true,
                images: true,
            },
        )
        .expect("desktop is a valid surface id");
        let result = reg.execute("desktop", &desktop, &[]);
        assert!(
            result.output.contains("Desktop integration: active"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("renders images"),
            "{}",
            result.output
        );
    }

    #[test]
    fn mobile_no_connections() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("mobile", &ctx(), &[]);
        assert!(result.output.contains("codape bridge"), "{}", result.output);
    }

    /// A truecolor terminal draws pictures and is still a terminal.
    ///
    /// The command used to infer "some other surface is presenting this" from
    /// `images: true`, so turning the picture capability on would have had it
    /// announce a non-terminal host that does not exist.
    #[test]
    fn mobile_does_not_mistake_a_picture_capable_terminal_for_another_surface() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let mut c = ctx();
        c.surface.capabilities.images = true;
        let result = reg.execute("mobile", &c, &[]);
        assert!(result.output.contains("codape bridge"), "{}", result.output);
        assert!(
            !result.output.contains("non-terminal surface"),
            "a terminal that draws images was called another surface: {}",
            result.output
        );
    }

    #[test]
    fn voice_feature_gated() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("voice", &ctx(), &[]);
        assert!(result.output.contains("not available"));
    }

    /// With the flag on but no command configured, the command says how to
    /// configure one. It used to answer "Voice Mode: Toggled" while nothing
    /// ever listened.
    #[test]
    fn voice_without_a_configured_command_explains_how_to_set_one() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let mut c = ctx_in(tmp.path());
        c.feature_flags.insert("voice_mode".to_string(), true);
        let result = reg.execute("voice", &c, &[]);
        assert!(
            result.output.contains("voice.command"),
            "{}",
            result.output
        );
        assert!(
            !result.output.contains("Toggled"),
            "the command still claims to toggle: {}",
            result.output
        );
    }

    /// A configured command is named, because that is the thing `alt+m` runs.
    #[test]
    fn voice_names_the_configured_command() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("settings.json"),
            r#"{"voice": {"command": "stt.sh --lang en"}}"#,
        )
        .unwrap();
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let mut c = ctx_in(tmp.path());
        c.feature_flags.insert("voice_mode".to_string(), true);
        let result = reg.execute("voice", &c, &[]);
        assert!(
            result.output.contains("stt.sh --lang en"),
            "the command does not name what alt+m would run: {}",
            result.output
        );
    }

    #[test]
    fn vim_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        // Default is false
        let result = reg.execute("vim", &ctx_in(tmp.path()), &[]);
        assert!(result.output.contains("enabled"));
        assert!(result.state_changed);
        // Verify settings.json was written and is a valid Settings document
        let settings_path = tmp.path().join("settings.json");
        assert!(settings_path.exists());
        let content = std::fs::read_to_string(&settings_path).unwrap();
        let settings: codape_config::Settings = serde_json::from_str(&content).unwrap();
        assert_eq!(settings.vim_mode, Some(true));
    }

    #[test]
    fn vim_already_on() {
        let tmp = tempfile::tempdir().unwrap();
        let c = CommandContext {
            vim_mode: true,
            ..ctx_in(tmp.path())
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("vim", &c, &[]);
        assert!(result.output.contains("disabled"));
        assert!(result.state_changed);
        // Verify settings.json was written with false
        let settings_path = tmp.path().join("settings.json");
        assert!(settings_path.exists());
        let content = std::fs::read_to_string(&settings_path).unwrap();
        let settings: codape_config::Settings = serde_json::from_str(&content).unwrap();
        assert_eq!(settings.vim_mode, Some(false));
    }

    #[test]
    fn remote_env_empty() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("remote-env", &ctx(), &[]);
        assert!(result.output.contains("none set"));
    }

    #[test]
    fn remote_env_with_vars() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("remote_env.json"), r#"{"PATH":"/usr/bin"}"#).unwrap();
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        // The listing comes from the dedicated file, not from ctx.env.
        let result = reg.execute("remote-env", &ctx(), &[]);
        assert!(result.output.contains("PATH=/usr/bin"));
    }

    #[test]
    fn remote_env_set_persists_to_dedicated_file() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("remote-env", &ctx(), &["FOO=bar".into()]);
        assert!(result.output.contains("FOO=bar"));
        assert!(result.state_changed);

        // Values go to remote_env.json...
        let env_path = tmp.path().join("remote_env.json");
        assert!(env_path.exists());
        let content = std::fs::read_to_string(&env_path).unwrap();
        assert_eq!(
            serde_json::from_str::<std::collections::HashMap<String, String>>(&content).unwrap(),
            std::collections::HashMap::from([("FOO".to_string(), "bar".to_string())])
        );
        assert_eq!(
            load_remote_env().get("FOO").map(String::as_str),
            Some("bar")
        );

        // ...and never into settings.json, which may be repository-controlled.
        assert!(!tmp.path().join("settings.json").exists());
    }

    #[test]
    fn remote_env_set_merges_existing_entries() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        std::fs::write(tmp.path().join("remote_env.json"), r#"{"EXISTING":"keep"}"#).unwrap();
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("remote-env", &ctx(), &["NEW=value".into()]);
        assert!(result.state_changed);

        let env = load_remote_env();
        assert_eq!(env.get("EXISTING").map(String::as_str), Some("keep"));
        assert_eq!(env.get("NEW").map(String::as_str), Some("value"));
    }

    #[test]
    fn remote_env_supports_key_value_form() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("remote-env", &ctx(), &["KEY".into(), "some value".into()]);
        assert!(result.state_changed);
        assert_eq!(
            load_remote_env().get("KEY").map(String::as_str),
            Some("some value")
        );
    }

    #[test]
    fn load_remote_env_missing_or_invalid_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        assert!(load_remote_env().is_empty());

        std::fs::write(tmp.path().join("remote_env.json"), "not json").unwrap();
        assert!(load_remote_env().is_empty());
    }

    #[test]
    fn remote_setup_instructions() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("remote-setup", &ctx(), &[]);
        assert!(result.output.contains("API key"));
        assert!(result.output.contains("Bridge"));
    }

    #[test]
    fn statusline_setup_opens_wizard() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("statusline-setup", &ctx_in(tmp.path()), &[]);
        assert!(result.output.contains("Statusline Setup"));
        assert!(result.state_changed);
        // Verify settings.json was written with a real statusline section
        let settings_path = tmp.path().join("settings.json");
        assert!(settings_path.exists());
        let content = std::fs::read_to_string(&settings_path).unwrap();
        let settings: codape_config::Settings = serde_json::from_str(&content).unwrap();
        let statusline = settings.statusline.as_ref().unwrap();
        assert_eq!(
            statusline.format.as_deref(),
            Some("[model] [tokens] [cost] [permissions]")
        );
        assert_eq!(statusline.theme.as_deref(), Some(""));
    }

    #[test]
    fn statusline_includes_vim_segment_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let c = CommandContext {
            vim_mode: true,
            theme: "dark".into(),
            ..ctx_in(tmp.path())
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("statusline-setup", &c, &[]);
        assert!(result.state_changed);

        let content = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
        let settings: codape_config::Settings = serde_json::from_str(&content).unwrap();
        let statusline = settings.statusline.as_ref().unwrap();
        assert!(statusline.format.as_deref().unwrap().contains("[vim]"));
        assert_eq!(statusline.theme.as_deref(), Some("dark"));
    }
}
