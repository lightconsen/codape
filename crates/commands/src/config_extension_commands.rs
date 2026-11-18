//! Config extension commands: /theme, /hooks, /mcp, /plugin, /memory, /skills, /tasks, /plan.

use crate::registry::{CommandArgs, CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::fmt::Write as _;
use std::sync::Arc;

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

/// List items in a directory matching a pattern, returning their names.
fn list_dir_items(path: &std::path::Path) -> Vec<String> {
    if !path.is_dir() {
        return Vec::new();
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect()
}

/// The skill names a directory holds, in the form `Skill` resolves them.
///
/// A skill is `name.md` or `name/SKILL.md`, and both are named `name` — the
/// raw directory entry would print `pdf.md` and `pdf` for the same skill in
/// its two forms, and neither is what the model has to type. Directories with
/// no `SKILL.md` are skipped: not every folder under `skills/` is a skill.
fn list_skill_names(path: &std::path::Path) -> Vec<String> {
    if !path.is_dir() {
        return Vec::new();
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                return path
                    .join("SKILL.md")
                    .is_file()
                    .then(|| entry.file_name().to_string_lossy().to_string());
            }
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                .then(|| {
                    path.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
        })
        .collect()
}

/// Built-in theme names.
///
/// Kept in sync with `codape_ui::themes::ThemeManager::new`. The commands
/// crate does not depend on the UI crate, so the list is repeated here rather
/// than pulling ratatui into every command build.
const BUILTIN_THEMES: [&str; 4] = ["dark", "light", "high-contrast", "tokyo-night"];

/// Theme names installed as `~/.codape/themes/*.json`.
///
/// `ThemeManager` keys a theme by the JSON `name`, not by the file name, so a
/// file called `mine.json` may install `my-theme`. Files without a non-empty
/// `name` are ignored, matching the loader.
fn custom_theme_names(path: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("json"))
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .filter_map(|value| {
            value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Every theme `/theme` accepts: built-ins, installed files, plugin themes.
fn available_themes(path: &std::path::Path, plugin_themes: &[String]) -> Vec<String> {
    let mut names: Vec<String> = BUILTIN_THEMES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    names.extend(custom_theme_names(path));
    names.extend(plugin_themes.iter().cloned());
    names.sort();
    names.dedup();
    names
}

/// One declaration emitted under `config_home()/plugin-types`.
struct GeneratedDeclaration {
    /// The name `/plugin types <name>` accepts.
    name: String,
    /// The declaration file on disk.
    path: std::path::PathBuf,
}

/// List the declarations under `root`, sorted by name.
///
/// The generated artefacts are directories with an `index.d.ts` (named after
/// the directory) plus `tsconfig.json` (named `tsconfig`). A plugin directory
/// uses its `index.d.ts` when it has one; otherwise every `.d.ts` it contains
/// is listed as `<plugin>/<file>` so a plugin-declared name is still
/// printable.
fn generated_declarations(root: &std::path::Path) -> Vec<GeneratedDeclaration> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut entries: Vec<std::fs::DirEntry> = entries.filter_map(std::result::Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);

    let mut declarations = Vec::new();
    for entry in entries {
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();
        if path.is_file() {
            if file_name.ends_with(".d.ts") {
                declarations.push(GeneratedDeclaration {
                    name: file_name,
                    path,
                });
            } else if file_name == "tsconfig.json" {
                declarations.push(GeneratedDeclaration {
                    name: "tsconfig".to_string(),
                    path,
                });
            }
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        let index = path.join("index.d.ts");
        if index.is_file() {
            declarations.push(GeneratedDeclaration {
                name: file_name,
                path: index,
            });
            continue;
        }
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&path)
            .into_iter()
            .flatten()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"))
            })
            .collect();
        files.sort();
        for file in files {
            let Some(stem) = file.file_name() else {
                continue;
            };
            declarations.push(GeneratedDeclaration {
                name: format!("{file_name}/{}", stem.to_string_lossy()),
                path: file,
            });
        }
    }
    declarations.sort_by(|left, right| left.name.cmp(&right.name));
    declarations
}

/// Resolve `/plugin types <name>` against the listed declarations.
///
/// The canonical name is accepted, and so is a relative path
/// (`codape-tools/index.d.ts`). No path is joined here: the requested name is
/// only ever matched against an already-listed file, so `..` cannot escape
/// the type root.
fn find_declaration(
    root: &std::path::Path,
    declarations: &[GeneratedDeclaration],
    requested: &str,
) -> Option<std::path::PathBuf> {
    if let Some(found) = declarations
        .iter()
        .find(|declaration| declaration.name == requested)
    {
        return Some(found.path.clone());
    }
    declarations
        .iter()
        .find(|declaration| {
            declaration
                .path
                .strip_prefix(root)
                .is_ok_and(|relative| relative.to_string_lossy() == requested)
        })
        .map(|declaration| declaration.path.clone())
}

/// Plugin names an install directory holds.
///
/// A plugin is a directory with a `plugin.json`. An install directory also
/// holds staging (`.install-<name>`), the pin store (`.codape-pins.json`) and
/// a lock file while an install is running; none of those is a plugin, and
/// listing them made a half-finished install look like an installed one.
fn installed_plugin_names(path: &std::path::Path) -> Vec<String> {
    list_dir_items(path)
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .filter(|name| path.join(name).join("plugin.json").is_file())
        .collect()
}

/// Read memory entries from the memory file.
fn read_memory_entries(home: &std::path::Path) -> Vec<String> {
    let memory_path = home.join("memory").join("MEMORY.md");
    if memory_path.exists() {
        std::fs::read_to_string(&memory_path).map_or_else(
            |_| Vec::new(),
            |content| {
                content
                    .lines()
                    .map(std::string::ToString::to_string)
                    .take(20)
                    .collect()
            },
        )
    } else {
        Vec::new()
    }
}

/// The servers a project's `.mcp.json` configures, one line each.
///
/// What `/mcp` with no subcommand prints: the session's connection state,
/// then whatever `.mcp.json` names. A session that is not connected still has
/// a config worth showing — that file is where `codape mcp add` writes and
/// where startup reads, so a listing that only spoke about live connections
/// pointed at a file and named no servers.
fn mcp_listing(ctx: &CommandContext) -> String {
    if ctx.mcp_connected {
        let mut output = String::from("MCP: Connected");
        if !ctx.mcp_servers.is_empty() {
            output.push_str("\nServers:");
            for server in &ctx.mcp_servers {
                output.push_str("\n  ");
                output.push_str(server);
            }
        }
        if !ctx.mcp_conflicts.is_empty() {
            output.push_str("\nRefused tool names (duplicate across servers):");
            for conflict in &ctx.mcp_conflicts {
                output.push_str("\n  ");
                output.push_str(conflict);
            }
        }
        output
    } else {
        let mut output = String::from("MCP: Not connected");
        output.push_str(&configured_mcp_servers(std::path::Path::new(&ctx.cwd)));
        if ctx.mcp_servers.is_empty() && ctx.mcp_conflicts.is_empty() {
            output.push_str(
                "\nAdd a server with /mcp add <name> <command>, or write .mcp.json in the \
                 project root.",
            );
        }
        output
    }
}

/// The config crate this module can reach has no MCP parser (that lives in
/// `codape-mcp`, which the command layer does not depend on), so this reads the
/// two shapes the file accepts — `{"mcpServers": {...}}` and the older
/// `{"mcp_servers": {...}}` — and prints a name plus how it is reached. A file
/// that cannot be read or parsed says so instead of reading as "none".
fn configured_mcp_servers(cwd: &std::path::Path) -> String {
    let path = cwd.join(".mcp.json");
    if !path.exists() {
        return format!("\nNo .mcp.json in {}", cwd.display());
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => return format!("\nCould not read {}: {error}", path.display()),
    };
    let document: serde_json::Value = match serde_json::from_str(&content) {
        Ok(document) => document,
        Err(error) => return format!("\n{} is not valid JSON: {error}", path.display()),
    };
    let servers = document
        .get("mcpServers")
        .or_else(|| document.get("mcp_servers"))
        .and_then(serde_json::Value::as_object);
    let Some(servers) = servers else {
        return format!("\n{} configures no servers.", path.display());
    };
    if servers.is_empty() {
        return format!("\n{} configures no servers.", path.display());
    }

    let mut names: Vec<&String> = servers.keys().collect();
    names.sort();
    let mut output = format!("\nConfigured in {}:", path.display());
    for name in names {
        let entry = &servers[name];
        let target = entry
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .or_else(|| {
                let command = entry.get("command")?.as_str()?;
                let args: Vec<&str> = entry
                    .get("args")
                    .and_then(serde_json::Value::as_array)
                    .map(|args| {
                        args.iter()
                            .filter_map(serde_json::Value::as_str)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Some(
                    format!("{command} {}", args.join(" "))
                        .trim_end()
                        .to_string(),
                )
            })
            .unwrap_or_else(|| "(no command or url)".to_string());
        let _ = write!(output, "\n  {name}: {target}");
    }
    output
}

/// Register config extension commands.
//
// Just over the 400-line limit, and all of it is `registry.register(...)`
// blocks — one per command, each a description plus a handler. Splitting it
// would move registrations to another file, not shorten them; the function is
// a table, and a table is read in one place. Exempted until that table moves
// to data.
#[allow(clippy::too_many_lines)]
pub fn register(registry: &mut CommandRegistry) {
    // /theme — switch/view theme
    registry.register(
        SlashCommand::local(
            "theme",
            "View or change the UI theme",
            true,
            true,
            true,
            Arc::new(|ctx: &CommandContext, args: &[String]| {
                if args.is_empty() {
                    let home = ctx.config_dir.clone();
                    let themes_path = home.join("themes");
                    let available = available_themes(&themes_path, &ctx.plugin_themes);
                    let mut output = format!("Current theme: {}", ctx.theme);
                    if !available.is_empty() {
                        let _ = write!(
                            output,
                            "\nAvailable themes:\n{}",
                            available
                                .iter()
                                .take(10)
                                .map(|t| format!("  {t}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        );
                    }
                    CommandResult::success(output)
                } else {
                    let new_theme = &args[0];
                    let home = ctx.config_dir.clone();
                    let themes_path = home.join("themes");
                    if available_themes(&themes_path, &ctx.plugin_themes)
                        .iter()
                        .any(|name| name == new_theme)
                    {
                        // `statusline.theme` is the key startup reads (and a
                        // known settings field). Writing a top-level `theme`
                        // made the next startup reject the whole file as an
                        // unknown field.
                        let mut settings =
                            read_user_settings_at(&ctx.config_dir).unwrap_or_default();
                        let mut statusline = settings
                            .get("statusline")
                            .and_then(|value| value.as_object().cloned())
                            .unwrap_or_default();
                        statusline.insert(
                            "theme".to_string(),
                            serde_json::Value::String(new_theme.clone()),
                        );
                        settings.insert(
                            "statusline".to_string(),
                            serde_json::Value::Object(statusline),
                        );
                        let _ = write_user_settings_at(&ctx.config_dir, &settings);
                        CommandResult::success(format!(
                            "Theme changed to: {new_theme}\nSaved to settings.json.",
                        ))
                        .with_state_change(true)
                    } else {
                        let available = available_themes(&themes_path, &ctx.plugin_themes);
                        let hint = format!(
                            "\nAvailable themes:\n{}",
                            available
                                .iter()
                                .map(|t| format!("  {t}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        );
                        CommandResult::error(format!("Theme not found: {new_theme}{hint}"))
                    }
                }
            }),
        )
        .with_args(CommandArgs::AtMost(1)),
    );

    // /hooks — view/manage hooks
    registry.register(SlashCommand::local(
        "hooks",
        "View or manage hooks",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            // The hooks that were installed, not the contents of a directory.
            // `~/.codape/hooks/` was never written by anything, so the old
            // listing reported "no hooks" while a configured hook was running
            // on every tool call. What the session actually installed comes
            // from the registry it built.
            let details = &ctx.active_hook_details;
            let home = ctx.config_dir.clone();
            let hooks_path = home.join("hooks");
            let files = list_dir_items(&hooks_path);

            let describe = |hooks: &[String]| {
                let mut output = format!("Active hooks: {}", hooks.len());
                if hooks.is_empty() {
                    output.push_str(
                        "\nNo lifecycle hooks are configured. Add a `hooks` block to \
                         settings.json; see docs/hooks.md.",
                    );
                } else {
                    output.push_str("\nInstalled hooks:");
                    for hook in hooks {
                        let _ = write!(output, "\n  {hook}");
                    }
                }
                output
            };

            if args.is_empty() {
                let mut output = describe(details);
                if !files.is_empty() {
                    let _ = write!(
                        output,
                        "\nHook files in {}:\n{}",
                        hooks_path.display(),
                        files
                            .iter()
                            .map(|h| format!("  {h}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                }
                CommandResult::success(output)
            } else {
                match args[0].as_str() {
                    "list" => CommandResult::success(describe(details)),
                    // Editing the hook configuration from here would write a
                    // file the running executor never re-reads, so `/hooks`
                    // does not pretend to. Firing an event is something it can
                    // answer honestly: it runs the installed hooks and says
                    // what happened.
                    "test" => {
                        let Some(event) = args.get(1) else {
                            return CommandResult::error(
                                "Usage: /hooks test <event> (for example: PreToolUse)",
                            );
                        };
                        CommandResult::success(format!("Firing '{event}'...")).with_deferred(
                            crate::registry::CommandDeferred::HookFire {
                                event: event.clone(),
                            },
                        )
                    }
                    other => CommandResult::error(format!(
                        "Unknown /hooks subcommand '{other}'. Use list or test."
                    )),
                }
            }
        }),
    ));

    // /mcp — view/manage MCP services
    registry.register(SlashCommand::local(
        "mcp",
        "View or manage MCP services",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            // Subcommands come first: `add`/`remove`/`test` are about the
            // config file and the manager, not about what this session has
            // connected, so answering them with a listing would be a
            // non-answer even when the session is connected.
            if let Some(subcommand) = args.first().map(String::as_str) {
                let rest = &args[1..];
                return match subcommand {
                    "list" => CommandResult::success(mcp_listing(ctx)),
                    "add" => {
                        let Some(name) = rest.first() else {
                            return CommandResult::error(
                                "Usage: /mcp add <name> (<command> | --url <url>)",
                            );
                        };
                        // `--url` marks the target as an endpoint. Everything
                        // else is a stdio command line, which the session
                        // splits into program and args.
                        //
                        // The arguments arrive already split on whitespace, so
                        // the command is *all* of the remaining words rejoined
                        // — taking only `rest[1]` dropped the rest of
                        // `/mcp add pg npx -y @mcp/postgres` and registered a
                        // server called `npx` with no args.
                        let (remote, target) = if rest.get(1).is_some_and(|arg| arg == "--url") {
                            match rest.get(2) {
                                Some(url) => (true, url.clone()),
                                None => {
                                    return CommandResult::error(
                                        "Usage: /mcp add <name> --url <url>",
                                    );
                                }
                            }
                        } else {
                            let command =
                                rest.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
                            if command.is_empty() {
                                return CommandResult::error(
                                    "Usage: /mcp add <name> (<command> | --url <url>)",
                                );
                            }
                            (false, command)
                        };
                        CommandResult::success(format!("Adding MCP server '{name}'..."))
                            .with_deferred(crate::registry::CommandDeferred::McpAdd {
                                name: name.clone(),
                                target,
                                remote,
                            })
                    }
                    "remove" => {
                        let Some(name) = rest.first() else {
                            return CommandResult::error("Usage: /mcp remove <name>");
                        };
                        CommandResult::success(format!("Removing MCP server '{name}'..."))
                            .with_deferred(crate::registry::CommandDeferred::McpRemove {
                                name: name.clone(),
                            })
                    }
                    "test" => {
                        let Some(name) = rest.first() else {
                            return CommandResult::error("Usage: /mcp test <name>");
                        };
                        CommandResult::success(format!("Testing MCP server '{name}'..."))
                            .with_deferred(crate::registry::CommandDeferred::McpTest {
                                name: name.clone(),
                            })
                    }
                    other => CommandResult::error(format!(
                        "Unknown /mcp subcommand '{other}'. Use list, add, remove or test."
                    )),
                };
            }

            CommandResult::success(mcp_listing(ctx))
        }),
    ));

    // /plugin — view/manage plugins
    registry.register(SlashCommand::local(
        "plugin",
        "View or manage plugins",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            // Installs go to the project's `.codape/plugins` and startup loads
            // both that and the config home, so a listing that read only the
            // home showed "no plugins" straight after a successful install.
            let project_path = std::path::Path::new(&ctx.cwd).join(".codape/plugins");
            let home_path = ctx.config_dir.join("plugins");
            let mut installed = installed_plugin_names(&project_path);
            installed.extend(installed_plugin_names(&home_path));
            installed.sort();
            installed.dedup();

            if args.is_empty() {
                if installed.is_empty() {
                    CommandResult::success("No plugins installed.\nInstall one with /plugin install <source>: a directory, a git URL, a `github:owner/repo` shorthand, or a marketplace name.")
                } else {
                    CommandResult::success(format!("Installed plugins:\n{}", installed.iter().map(|p| format!("  {p}")).collect::<Vec<_>>().join("\n")))
                }
            } else {
                match args[0].as_str() {
                    "list" => {
                        if installed.is_empty() {
                            CommandResult::success("No plugins installed.")
                        } else {
                            CommandResult::success(format!("Installed plugins:\n{}", installed.join("\n")))
                        }
                    }
                    "install" => {
                        let Some(source) = args.get(1) else {
                            return CommandResult::error(
                                "Usage: /plugin install <source>\n\
                                 A source is a directory (./my-plugin), a git remote \
                                 (https://github.com/acme/plugin.git), a `github:owner/repo` \
                                 shorthand, an archive URL (.tar.gz/.tgz), or the name of a \
                                 plugin in a configured marketplace.",
                            );
                        };
                        // Installing is async and needs the plugin manager, both
                        // of which live in the CLI. The handler says what to
                        // install; `ReplLoop` carries it out and reports back.
                        CommandResult::success(format!("Installing plugin from '{source}'..."))
                            .with_state_change(true)
                            .with_deferred(crate::registry::CommandDeferred::PluginInstall {
                                source: source.clone(),
                            })
                    }
                    "components" => {
                        if ctx.plugin_components.is_empty() {
                            return CommandResult::success("No plugin components loaded.");
                        }
                        let mut by_kind: std::collections::BTreeMap<&str, Vec<&str>> =
                            std::collections::BTreeMap::new();
                        for (kind, name) in &ctx.plugin_components {
                            by_kind.entry(kind.as_str()).or_default().push(name.as_str());
                        }
                        let mut output = String::from("Plugin components:");
                        for (kind, names) in by_kind {
                            let _ = write!(output, "\n  {kind}: {}", names.join(", "));
                        }
                        CommandResult::success(output)
                    }
                    "types" => {
                        let root = ctx.config_dir.join("plugin-types");
                        let declarations = generated_declarations(&root);
                        if declarations.is_empty() {
                            return CommandResult::success(format!(
                                "No generated plugin type declarations at {}.\nThey are written at startup when the plugin type surface is exported.",
                                root.display()
                            ));
                        }
                        if let Some(requested) = args.get(1) {
                            return find_declaration(&root, &declarations, requested).map_or_else(
                                || {
                                    CommandResult::error(format!(
                                        "No generated declaration named '{requested}'. Try /plugin types."
                                    ))
                                },
                                |path| match std::fs::read_to_string(&path) {
                                    Ok(source) => CommandResult::success(source),
                                    Err(error) => CommandResult::error(format!(
                                        "Cannot read {}: {error}",
                                        path.display()
                                    )),
                                },
                            );
                        }
                        let mut output = String::from("Generated plugin type declarations:");
                        for declaration in &declarations {
                            let relative = declaration
                                .path
                                .strip_prefix(&root)
                                .unwrap_or(&declaration.path);
                            let _ = write!(
                                output,
                                "\n  {:<20} {}",
                                declaration.name,
                                relative.display()
                            );
                        }
                        CommandResult::success(output)
                    }
                    "marketplace" => match args.get(1).map(String::as_str) {
                        None | Some("list") => {
                            let settings =
                                read_user_settings_at(&ctx.config_dir).unwrap_or_default();
                            let entries =
                                settings.get("marketplaces").and_then(|v| v.as_array());
                            match entries {
                                Some(entries) if !entries.is_empty() => {
                                    let mut output = String::from("Configured marketplaces:");
                                    for entry in entries {
                                        let name = entry
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("(unnamed)");
                                        let url = entry
                                            .get("url")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("(no url)");
                                        let _ = write!(output, "\n  {name}  {url}");
                                    }
                                    CommandResult::success(output)
                                }
                                _ => CommandResult::success(
                                    "No marketplaces configured.\n\
                                     Add one with /plugin marketplace add <name> <https-url>.",
                                ),
                            }
                        }
                        Some("add") => {
                            let (Some(name), Some(url)) = (args.get(2), args.get(3)) else {
                                return CommandResult::error(
                                    "Usage: /plugin marketplace add <name> <https-url>",
                                );
                            };
                            let setting = match codape_config::MarketplaceSetting::validated(
                                name, url,
                            ) {
                                Ok(setting) => setting,
                                Err(error) => return CommandResult::error(error.to_string()),
                            };
                            let mut settings =
                                read_user_settings_at(&ctx.config_dir).unwrap_or_default();
                            let mut entries = settings
                                .get("marketplaces")
                                .and_then(|v| v.as_array())
                                .cloned()
                                .unwrap_or_default();
                            // Replacing by name rather than appending: adding
                            // the same name twice used to leave two entries,
                            // and which URL an install resolved against then
                            // depended on registration order.
                            entries.retain(|entry| {
                                entry.get("name").and_then(|v| v.as_str())
                                    != Some(setting.name.as_str())
                            });
                            entries.push(serde_json::json!({
                                "name": setting.name,
                                "url": setting.url,
                            }));
                            settings.insert(
                                "marketplaces".to_string(),
                                serde_json::Value::Array(entries),
                            );
                            match write_user_settings_at(&ctx.config_dir, &settings) {
                                Ok(()) => CommandResult::success(format!(
                                    "Marketplace '{}' saved to settings.json.\n\
                                     New sessions resolve plugin names against it.",
                                    setting.name
                                ))
                                .with_state_change(true),
                                Err(error) => CommandResult::error(format!(
                                    "Could not write settings.json: {error}"
                                )),
                            }
                        }
                        Some("remove") => {
                            let Some(name) = args.get(2) else {
                                return CommandResult::error(
                                    "Usage: /plugin marketplace remove <name>",
                                );
                            };
                            let mut settings =
                                read_user_settings_at(&ctx.config_dir).unwrap_or_default();
                            let before = settings
                                .get("marketplaces")
                                .and_then(|v| v.as_array())
                                .map_or(0, Vec::len);
                            let mut entries = settings
                                .get("marketplaces")
                                .and_then(|v| v.as_array())
                                .cloned()
                                .unwrap_or_default();
                            entries.retain(|entry| {
                                entry.get("name").and_then(|v| v.as_str()) != Some(name.as_str())
                            });
                            if entries.len() == before {
                                return CommandResult::error(format!(
                                    "No marketplace named '{name}'."
                                ));
                            }
                            settings.insert(
                                "marketplaces".to_string(),
                                serde_json::Value::Array(entries),
                            );
                            match write_user_settings_at(&ctx.config_dir, &settings) {
                                Ok(()) => CommandResult::success(format!(
                                    "Marketplace '{name}' removed from settings.json."
                                ))
                                .with_state_change(true),
                                Err(error) => CommandResult::error(format!(
                                    "Could not write settings.json: {error}"
                                )),
                            }
                        }
                        Some(other) => CommandResult::error(format!(
                            "Unknown marketplace action '{other}'.\n\
                             Usage: /plugin marketplace [list|add <name> <https-url>|remove <name>]"
                        )),
                    },
                    _ => CommandResult::error(
                        "Usage: /plugin [list|components|types [name]|install <source>|marketplace [list|add <name> <url>|remove <name>]]",
                    ),
                }
            }
        }),
    ));

    // /memory — view/search long-term memory
    registry.register(SlashCommand::local(
        "memory",
        "View or search long-term memory",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            let entries = read_memory_entries(&ctx.config_dir);
            let home = ctx.config_dir.clone();
            let memory_path = home.join("memory");
            let memory_files = list_dir_items(&memory_path);

            if args.is_empty() {
                let count = entries.len();
                if count == 0 {
                    CommandResult::success(format!(
                        "Memory: 0 entries\nMemory files found: {memory_files:?}\nUse /memory search <query> to search.\nMemory stored in ~/.codape/memory/"
                    ))
                } else {
                    CommandResult::success(format!(
                        "Memory: {count} entries\n{}",
                        entries.join("\n")
                    ))
                }
            } else if args[0] == "search" {
                let query = args[1..].join(" ");
                let matches: Vec<_> = entries.iter()
                    .filter(|l| l.to_lowercase().contains(&query.to_lowercase()))
                    .collect();
                if matches.is_empty() {
                    CommandResult::success(format!("No memory entries matching: {query}"))
                } else {
                    CommandResult::success(format!(
                        "Memory search for '{query}' — {} matches:\n{}",
                        matches.len(),
                        matches.iter().take(10).map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")
                    ))
                }
            } else {
                CommandResult::success(format!("Searching memory for: {}", args.join(" ")))
            }
        }),
    ));

    // /skills — view/load skills
    registry.register(SlashCommand::local(
        "skills",
        "View or load skills",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            let home = ctx.config_dir.clone();

            // Scan every directory the loader scans: the config home, the
            // project, and the bundled tree. Listing only the config home
            // meant a project skill was loaded into the session and missing
            // from `/skills`, which reads as "this skill does not exist".
            let skill_dirs = [
                home.join("skills"),                                   // ~/.codape/skills/
                home.join("bundled-skills"),                           // unpacked by hand
                std::path::Path::new(&ctx.cwd).join(".codape/skills"), // project
            ];

            // The skills compiled in. They are not on disk, so the directory
            // scan above cannot see them — and `/skills` listing a shorter
            // list than the advert the model received is the same bug as
            // before, one layer down.
            let mut all_skills: Vec<String> = codape_core::bundled_skills::names()
                .into_iter()
                .map(str::to_string)
                .collect();
            for dir in &skill_dirs {
                all_skills.extend(list_skill_names(dir));
            }
            all_skills.sort();
            all_skills.dedup();

            if args.is_empty() {
                if all_skills.is_empty() {
                    CommandResult::success(
                        "Skills:\n  No skills loaded.\nPlace skill scripts in ~/.codape/skills/",
                    )
                } else {
                    CommandResult::success(format!(
                        "Skills:\n{}",
                        all_skills
                            .iter()
                            .map(|s| format!("  {s}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ))
                }
            } else {
                match args[0].as_str() {
                    "list" => {
                        if all_skills.is_empty() {
                            CommandResult::success("No skills loaded.")
                        } else {
                            CommandResult::success(format!(
                                "Loaded skills:\n{}",
                                all_skills.join("\n")
                            ))
                        }
                    }
                    _ => CommandResult::error("Usage: /skills [list]"),
                }
            }
        }),
    ));

    // /tasks — view task list (command version)
    registry.register(SlashCommand::local(
        "tasks",
        "View the task list",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            // Look for task files in .codape/tasks/ and cwd
            let home = ctx.config_dir.clone();
            let tasks_path = home.join("tasks");
            let task_files = list_dir_items(&tasks_path);

            if args.is_empty() {
                if task_files.is_empty() {
                    CommandResult::success("Tasks:\n  No tasks registered.\nUse TaskCreate to create tasks.\nTask files can be placed in ~/.codape/tasks/")
                } else {
                    CommandResult::success(format!("Task files in ~/.codape/tasks/:\n{}", task_files.iter().map(|t| format!("  {t}")).collect::<Vec<_>>().join("\n")))
                }
            } else {
                match args[0].as_str() {
                    "list" => {
                        if task_files.is_empty() {
                            CommandResult::success("No task files found.")
                        } else {
                            CommandResult::success(format!("Task files:\n{}", task_files.join("\n")))
                        }
                    }
                    _ => CommandResult::error("Usage: /tasks [list]"),
                }
            }
        }),
    ));

    // /plan — view/edit current plan
    registry.register(SlashCommand::local(
        "plan",
        "View or edit the current plan",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            // Try to find and read the plan file
            let home = ctx.config_dir.clone();
            let plans_path = home.join("plans");
            let plan_files = list_dir_items(&plans_path);
            if ctx.has_plan {
                let mut output = "Plan loaded.\n\n".to_string();
                if !plan_files.is_empty() {
                    let _ = write!(
                        output,
                        "Available plans:\n{}\n",
                        plan_files
                            .iter()
                            .map(|p| format!("  {p}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                }
                output.push_str("(Use the Read tool to view plan details)");
                CommandResult::success(output)
            } else {
                // Check if any plan files exist
                if plan_files.is_empty() {
                    CommandResult::success("No active plan.\nUse /plan create to generate one.")
                } else {
                    CommandResult::success(format!(
                        "Plan files found in ~/.codape/plans/:\n{}",
                        plan_files.join("\n")
                    ))
                }
            }
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
    fn theme_shows_current() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("theme", &ctx(), &[]);
        assert!(result.output.contains("Current theme"));
    }

    #[test]
    fn theme_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("theme", &ctx_in(tmp.path()), &["dark".into()]);
        assert!(result.output.contains("dark"));
        assert!(result.state_changed);
        // Verify settings.json was written
        assert!(tmp.path().join("settings.json").exists());
    }

    /// A theme installed as `~/.codape/themes/*.json` is listed by its JSON
    /// name and accepted by `/theme` — the loader keys themes by `name`, not
    /// by the file stem.
    #[test]
    fn theme_accepts_an_installed_custom_theme() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("themes")).unwrap();
        std::fs::write(
            tmp.path().join("themes").join("mine.json"),
            r#"{"name":"my-theme"}"#,
        )
        .unwrap();

        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };

        let listed = reg.execute("theme", &ctx_in(tmp.path()), &[]);
        assert!(listed.output.contains("my-theme"), "{}", listed.output);
        assert!(!listed.output.contains("mine.json"), "{}", listed.output);
        for builtin in BUILTIN_THEMES {
            assert!(listed.output.contains(builtin), "{}", listed.output);
        }

        let switched = reg.execute("theme", &ctx_in(tmp.path()), &["my-theme".into()]);
        assert!(switched.state_changed, "{}", switched.output);
        assert!(switched.output.contains("my-theme"), "{}", switched.output);

        let settings = std::fs::read_to_string(tmp.path().join("settings.json")).unwrap();
        assert!(settings.contains("my-theme"), "{settings}");
    }

    #[test]
    fn theme_unknown_not_found() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("theme", &ctx(), &["nonexistent-theme-xyz".into()]);
        // Will show error since theme doesn't exist
        assert!(result.output.contains("nonexistent-theme-xyz"));
    }

    #[test]
    fn hooks_shows_count() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("hooks", &ctx(), &[]);
        assert!(result.output.contains("Active hooks"));
    }

    #[test]
    fn hooks_list() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("hooks", &ctx(), &["list".into()]);
        assert!(result.output.contains("hooks") || result.output.contains("Installed"));
    }

    /// `/hooks test` fires the installed executor, which the registry cannot
    /// reach, so it hands the event on; editing the configuration is refused
    /// because a rewrite would not reach a running session.
    #[test]
    fn hooks_test_defers_and_editing_is_refused() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };

        let test = reg.execute("hooks", &ctx(), &["test".into(), "PreToolUse".into()]);
        assert_eq!(
            test.deferred,
            Some(crate::registry::CommandDeferred::HookFire {
                event: "PreToolUse".into()
            })
        );

        // No event is a usage error, not a deferred no-op.
        let bare = reg.execute("hooks", &ctx(), &["test".into()]);
        assert!(bare.output.starts_with("Error: "), "{}", bare.output);
        assert!(bare.deferred.is_none());

        // An unknown subcommand names the ones that exist rather than
        // pretending `add`/`edit` are supported.
        let unknown = reg.execute("hooks", &ctx(), &["add".into()]);
        assert!(
            unknown.output.contains("list or test"),
            "{}",
            unknown.output
        );
    }

    #[test]
    fn mcp_status_not_connected() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("mcp", &ctx(), &[]);
        assert!(result.output.contains("Not connected"));
    }

    #[test]
    fn mcp_status_connected() {
        let c = CommandContext {
            mcp_connected: true,
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("mcp", &c, &[]);
        assert!(result.output.contains("Connected"));
    }

    #[test]
    fn plugin_shows_none() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("plugin", &ctx(), &[]);
        assert!(result.output.contains("No plugins") || result.output.contains("Installed"));
    }

    #[test]
    fn plugin_list() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("plugin", &ctx(), &["list".into()]);
        assert!(result.output.contains("plugin") || result.output.contains("No plugins"));
    }

    /// Adding a marketplace is the gesture that was missing: the setting
    /// existed and nothing in the CLI or the session could write it.
    #[test]
    fn plugin_marketplace_add_writes_the_setting() {
        let home = tempfile::tempdir().unwrap();
        let c = CommandContext {
            config_dir: home.path().to_path_buf(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };

        let result = reg.execute(
            "plugin",
            &c,
            &[
                "marketplace".into(),
                "add".into(),
                "internal".into(),
                "https://plugins.example".into(),
            ],
        );
        assert!(result.output.contains("internal"), "{}", result.output);
        assert!(result.state_changed);

        // The file is the contract: startup reads `marketplaces` from it, so
        // the assertion has to be against what was written, not the message.
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["marketplaces"][0]["name"], "internal");
        assert_eq!(written["marketplaces"][0]["url"], "https://plugins.example");

        let listed = reg.execute("plugin", &c, &["marketplace".into(), "list".into()]);
        assert!(
            listed.output.contains("plugins.example"),
            "{}",
            listed.output
        );
    }

    #[test]
    fn plugin_marketplace_add_replaces_an_entry_of_the_same_name() {
        let home = tempfile::tempdir().unwrap();
        let c = CommandContext {
            config_dir: home.path().to_path_buf(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        for url in ["https://one.example", "https://two.example"] {
            let result = reg.execute(
                "plugin",
                &c,
                &[
                    "marketplace".into(),
                    "add".into(),
                    "internal".into(),
                    url.into(),
                ],
            );
            assert!(result.output.contains("internal"), "{}", result.output);
        }

        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        let entries = written["marketplaces"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "the name should be registered once");
        assert_eq!(entries[0]["url"], "https://two.example");
    }

    /// A marketplace registration refuses `http://`, so accepting one into
    /// settings would write a value whose only effect is a startup warning.
    #[test]
    fn plugin_marketplace_add_refuses_a_url_registration_would_refuse() {
        let home = tempfile::tempdir().unwrap();
        let c = CommandContext {
            config_dir: home.path().to_path_buf(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute(
            "plugin",
            &c,
            &[
                "marketplace".into(),
                "add".into(),
                "insecure".into(),
                "http://plugins.example".into(),
            ],
        );
        assert!(result.output.contains("https"), "{}", result.output);
        assert!(!home.path().join("settings.json").exists());
    }

    #[test]
    fn plugin_marketplace_remove_names_what_was_not_there() {
        let home = tempfile::tempdir().unwrap();
        let c = CommandContext {
            config_dir: home.path().to_path_buf(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        reg.execute(
            "plugin",
            &c,
            &[
                "marketplace".into(),
                "add".into(),
                "internal".into(),
                "https://plugins.example".into(),
            ],
        );
        let removed = reg.execute(
            "plugin",
            &c,
            &["marketplace".into(), "remove".into(), "internal".into()],
        );
        assert!(removed.output.contains("removed"), "{}", removed.output);
        let again = reg.execute(
            "plugin",
            &c,
            &["marketplace".into(), "remove".into(), "internal".into()],
        );
        assert!(again.output.contains("No marketplace"), "{}", again.output);
    }

    /// `/hooks` names the hooks the session installed.
    ///
    /// It used to list `~/.codape/hooks/`, a directory nothing writes, and so
    /// reported "no hooks" while a configured hook ran on every tool call.
    #[test]
    fn hooks_names_the_configured_hooks() {
        let c = CommandContext {
            active_hooks: 1,
            active_hook_details: vec!["Core PreToolUse command: cargo fmt".into()],
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };

        let listed = reg.execute("hooks", &c, &[]);
        assert!(
            listed.output.contains("Active hooks: 1"),
            "{}",
            listed.output
        );
        assert!(listed.output.contains("PreToolUse"), "{}", listed.output);
        assert!(listed.output.contains("cargo fmt"), "{}", listed.output);
        assert!(
            !listed.output.contains("No lifecycle hooks"),
            "{}",
            listed.output
        );
    }

    /// `/mcp` reports what `.mcp.json` configures, not just that it is not
    /// connected.
    #[test]
    fn mcp_names_servers_configured_in_the_project() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join(".mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","@mcp/fs"]},"api":{"url":"https://example.test/mcp"}}}"#,
        )
        .unwrap();

        let c = CommandContext {
            cwd: workspace.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };

        let result = reg.execute("mcp", &c, &[]);
        assert!(result.output.contains("Not connected"), "{}", result.output);
        assert!(
            result.output.contains("fs: npx -y @mcp/fs"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("api: https://example.test/mcp"),
            "{}",
            result.output
        );
    }

    /// A `.mcp.json` that cannot be parsed says so; it must not read as "no
    /// servers".
    #[test]
    fn mcp_reports_an_unparseable_config() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join(".mcp.json"), "{not json").unwrap();

        let c = CommandContext {
            cwd: workspace.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };

        let result = reg.execute("mcp", &c, &[]);
        assert!(
            result.output.contains("not valid JSON"),
            "{}",
            result.output
        );
    }

    /// `/mcp` with a subcommand has to hand the work to the session rather
    /// than answering with a listing: the config parser and the client live
    /// in `codape-mcp`, which this crate cannot reach.
    #[test]
    fn mcp_subcommands_defer_to_the_session() {
        let c = CommandContext {
            cwd: tempfile::tempdir()
                .unwrap()
                .path()
                .to_string_lossy()
                .to_string(),
            ..ctx()
        };
        let mut reg = CommandRegistry::new();
        register(&mut reg);

        let add = reg.execute(
            "mcp",
            &c,
            &[
                "add".to_string(),
                "fs".to_string(),
                "npx".to_string(),
                "-y".to_string(),
                "@mcp/fs".to_string(),
            ],
        );
        assert_eq!(
            add.deferred,
            Some(crate::registry::CommandDeferred::McpAdd {
                name: "fs".into(),
                // The words arrive already split; the whole tail is the
                // command, not just the program.
                target: "npx -y @mcp/fs".into(),
                remote: false,
            })
        );

        // `--url` marks the target as a remote endpoint rather than a command.
        let add_url = reg.execute(
            "mcp",
            &c,
            &[
                "add".to_string(),
                "api".to_string(),
                "--url".to_string(),
                "https://example.test/mcp".to_string(),
            ],
        );
        assert_eq!(
            add_url.deferred,
            Some(crate::registry::CommandDeferred::McpAdd {
                name: "api".into(),
                target: "https://example.test/mcp".into(),
                remote: true,
            })
        );

        let remove = reg.execute("mcp", &c, &["remove".to_string(), "fs".to_string()]);
        assert_eq!(
            remove.deferred,
            Some(crate::registry::CommandDeferred::McpRemove { name: "fs".into() })
        );

        let test = reg.execute("mcp", &c, &["test".to_string(), "fs".to_string()]);
        assert_eq!(
            test.deferred,
            Some(crate::registry::CommandDeferred::McpTest { name: "fs".into() })
        );

        // A missing argument is a usage error, not a deferred no-op.
        let bare = reg.execute("mcp", &c, &["add".to_string()]);
        assert!(bare.output.starts_with("Error: "), "{}", bare.output);
        assert!(bare.deferred.is_none());

        // An unknown subcommand names the ones that exist.
        let unknown = reg.execute("mcp", &c, &["frobnicate".to_string()]);
        assert!(
            unknown.output.contains("list, add, remove or test"),
            "{}",
            unknown.output
        );
    }

    /// `/plugin` lists the project's install directory, where installs land.
    #[test]
    fn plugin_lists_a_project_install() {
        let workspace = tempfile::tempdir().unwrap();
        let plugins = workspace.path().join(".codape/plugins");
        std::fs::create_dir_all(plugins.join("acme-tools")).unwrap();
        std::fs::write(plugins.join("acme-tools/plugin.json"), "{}").unwrap();
        // Staging and the pin store are not plugins.
        std::fs::create_dir_all(plugins.join(".install-broken")).unwrap();
        std::fs::write(plugins.join(".codape-pins.json"), "{}").unwrap();

        let c = CommandContext {
            cwd: workspace.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };

        let result = reg.execute("plugin", &c, &[]);
        assert!(result.output.contains("acme-tools"), "{}", result.output);
        for not_a_plugin in [".install-broken", ".codape-pins.json"] {
            assert!(
                !result.output.contains(not_a_plugin),
                "{not_a_plugin} was listed: {}",
                result.output
            );
        }
    }

    /// `/plugin install <source>` hands the install to the session.
    ///
    /// The command layer cannot reach the installer, so the handler queues a
    /// `CommandDeferred` instead of refusing the way it used to.
    #[test]
    fn plugin_install_queues_the_install() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };

        let result = reg.execute("plugin", &ctx(), &["install".into(), "./my-plugin".into()]);
        assert!(!result.output.starts_with("Error:"), "{:?}", result.output);
        assert!(result.output.contains("./my-plugin"), "{}", result.output);
        assert_eq!(
            result.deferred,
            Some(crate::registry::CommandDeferred::PluginInstall {
                source: "./my-plugin".into()
            })
        );

        let missing = reg.execute("plugin", &ctx(), &["install".into()]);
        assert!(missing.output.starts_with("Error:"), "{:?}", missing.output);
        assert!(missing.deferred.is_none());
    }

    #[test]
    fn plugin_types_lists_and_prints_generated_declarations() {
        let home = tempfile::tempdir().unwrap();
        let types_root = home.path().join("plugin-types");
        std::fs::create_dir_all(types_root.join("codape-tools")).unwrap();
        std::fs::create_dir_all(types_root.join("acme")).unwrap();
        std::fs::write(
            types_root.join("codape-tools/index.d.ts"),
            "declare module 'codape-tools' {}",
        )
        .unwrap();
        std::fs::write(
            types_root.join("acme/index.d.ts"),
            "declare module 'acme' {}",
        )
        .unwrap();
        std::fs::write(types_root.join("tsconfig.json"), "{}").unwrap();

        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };

        let listing = reg.execute("plugin", &ctx_in(home.path()), &["types".into()]);
        assert!(!listing.output.starts_with("Error:"), "{listing:?}");
        for name in ["acme", "codape-tools", "tsconfig"] {
            assert!(listing.output.contains(name), "{listing:?}");
        }

        let printed = reg.execute(
            "plugin",
            &ctx_in(home.path()),
            &["types".into(), "codape-tools".into()],
        );
        assert_eq!(printed.output, "declare module 'codape-tools' {}");

        let by_path = reg.execute(
            "plugin",
            &ctx_in(home.path()),
            &["types".into(), "acme/index.d.ts".into()],
        );
        assert_eq!(by_path.output, "declare module 'acme' {}");

        let missing = reg.execute(
            "plugin",
            &ctx_in(home.path()),
            &["types".into(), "nope".into()],
        );
        assert!(missing.output.starts_with("Error:"), "{missing:?}");
        assert!(missing.output.contains("nope"));
    }

    #[test]
    fn plugin_types_reports_an_empty_root() {
        let home = tempfile::tempdir().unwrap();
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("plugin", &ctx_in(home.path()), &["types".into()]);
        assert!(!result.output.starts_with("Error:"), "{result:?}");
        assert!(
            result
                .output
                .contains("No generated plugin type declarations")
        );
    }

    #[test]
    fn memory_search() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute(
            "memory",
            &ctx(),
            &["search".into(), "user".into(), "preferences".into()],
        );
        assert!(
            result.output.contains("user")
                || result.output.contains("No memory")
                || result.output.contains("Memory")
        );
    }

    #[test]
    fn memory_empty_by_default() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("memory", &ctx(), &[]);
        assert!(result.output.contains("Memory"));
    }

    #[test]
    fn skills_shows_none() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("skills", &ctx(), &[]);
        assert!(result.output.contains("No skills") || result.output.contains("Skills"));
    }

    #[test]
    fn skills_list() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("skills", &ctx(), &["list".into()]);
        assert!(result.output.contains("skills") || result.output.contains("No skills"));
    }

    #[test]
    fn tasks_shows_none() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("tasks", &ctx(), &[]);
        assert!(result.output.contains("No tasks") || result.output.contains("tasks"));
    }

    #[test]
    fn tasks_list() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("tasks", &ctx(), &["list".into()]);
        assert!(result.output.to_lowercase().contains("task"));
    }

    #[test]
    fn plan_no_plan() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("plan", &ctx(), &[]);
        assert!(result.output.contains("No active plan") || result.output.contains("Plan"));
    }

    #[test]
    fn plan_loaded() {
        let c = CommandContext {
            has_plan: true,
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("plan", &c, &[]);
        assert!(result.output.contains("Plan"));
    }

    /// Both skill forms are listed by the name the model has to type, and a
    /// folder that is not a skill is left out.
    #[test]
    fn skill_listing_names_both_forms_and_skips_plain_folders() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pdf.md"), "---\nname: pdf\n---\nbody").unwrap();
        std::fs::create_dir_all(tmp.path().join("slides/assets")).unwrap();
        std::fs::write(
            tmp.path().join("slides/SKILL.md"),
            "---\nname: slides\n---\nbody",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("assets")).unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "not a skill").unwrap();

        let mut names = list_skill_names(tmp.path());
        names.sort();

        assert_eq!(names, vec!["pdf".to_string(), "slides".to_string()]);
    }
}
