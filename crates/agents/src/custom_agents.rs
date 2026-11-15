//! Custom agent definitions loaded from user and project configuration files.
//!
//! Two directories are read: the user's `~/.codape/agents/` and the project's
//! `.codape/agents/`. Each may hold either a JSON definition or a Markdown
//! file with YAML frontmatter — the Markdown form carries the system prompt as
//! its body, which is what makes a long prompt readable and reviewable in a
//! diff. A project definition shadows a user definition of the same name, so a
//! repository can pin its own reviewer without touching the person's home
//! directory.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A user- or project-defined custom agent.
///
/// The JSON form spells every field out. The Markdown form takes `name` from
/// the file stem, `system_prompt` from the body, and the rest from frontmatter
/// (`description`, `tools`, `model`, `max_tokens`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomAgentDef {
    /// Unique name for the custom agent.
    pub name: String,
    /// System prompt for the agent.
    pub system_prompt: String,
    /// Tool whitelist — only these tools are available.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Default model override (optional).
    pub model: Option<String>,
    /// Maximum tokens (optional).
    pub max_tokens: Option<u32>,
    /// JSON schema for validating agent output (optional).
    pub output_schema: Option<serde_json::Value>,
    /// One-line summary shown when listing agents (optional).
    ///
    /// The JSON form may omit it; the Markdown form reads it from
    /// frontmatter. A listing that would otherwise print the prompt's first
    /// line — which for a Markdown file is the frontmatter's neighbour, not
    /// always a sentence — prefers this when present.
    #[serde(default)]
    pub description: Option<String>,
}

/// Why one custom agent file was skipped.
///
/// Returned by [`load_custom_agents_from_dir_with_warnings`] so callers (and
/// tests) can see the decision without depending on tracing's per-callsite
/// interest cache, which makes a warning disappear when another test in the
/// same binary evaluates the callsite first with no subscriber installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLoadWarning {
    /// The file that was skipped.
    pub path: PathBuf,
    /// Why it was skipped.
    pub reason: String,
}

impl std::fmt::Display for AgentLoadWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "skipping custom agent file: {} path={}",
            self.reason,
            self.path.display()
        )
    }
}

/// Load all custom agent definitions from the user and project directories.
///
/// `~/.codape/agents/` is read first and `<cwd>/.codape/agents/` second, so a
/// definition in the project shadows the person's definition of the same name.
/// Returns a map of agent name to definition. An unreadable or malformed file
/// is skipped with a warning naming its path and the error, so a typo in a
/// definition does not make the agent vanish without explanation.
#[must_use]
pub fn load_custom_agents() -> HashMap<String, CustomAgentDef> {
    // Relative `.codape/agents` resolves against the process's working
    // directory, which is the project root in every path that calls this.
    load_custom_agents_for_project(Path::new("."))
}

/// Load the user directory and the project directory for one working
/// directory, project last so its definitions win.
///
/// This is the form the session uses: the project root is the session's own
/// `cwd`, which is what `--agent` and the Agent tool resolve against.
#[must_use]
pub fn load_custom_agents_for_project(cwd: &Path) -> HashMap<String, CustomAgentDef> {
    let mut agents = HashMap::new();
    if let Some(home) = home::home_dir() {
        agents.extend(load_custom_agents_from_dir(
            &home.join(".codape").join("agents"),
        ));
    }
    agents.extend(load_custom_agents_from_dir(
        &cwd.join(".codape").join("agents"),
    ));
    agents
}

/// Load custom agent definitions from an explicit directory.
///
/// This is the testable form of [`load_custom_agents`]: callers that manage
/// their own configuration directory (tests, embedders, project-local agent
/// sets) do not have to pretend to be `$HOME`.
///
/// Both `.json` and `.md` files are read. Files that cannot be read, do not
/// parse, or fail validation are skipped with a warning naming the path; the
/// remaining files still load.
pub fn load_custom_agents_from_dir(agents_dir: &Path) -> HashMap<String, CustomAgentDef> {
    let (agents, warnings) = load_custom_agents_from_dir_with_warnings(agents_dir);
    for warning in &warnings {
        tracing::warn!("{warning}");
    }
    agents
}

/// Load custom agent definitions and return why any file was skipped.
///
/// This is the testable form: see [`AgentLoadWarning`]. It is also the form
/// an embedding caller should use when it wants to show the skips itself.
#[must_use]
pub fn load_custom_agents_from_dir_with_warnings(
    agents_dir: &Path,
) -> (HashMap<String, CustomAgentDef>, Vec<AgentLoadWarning>) {
    let mut agents = HashMap::new();
    let mut warnings = Vec::new();

    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return (agents, warnings);
    };

    // Sorted so a directory holding two definitions of one name and two file
    // forms loads deterministically rather than by filesystem order. A
    // subdirectory named `*.json` is deliberately not filtered out: reading it
    // fails, which is reported like any other unreadable file.
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();

    for path in paths {
        let extension = path.extension().and_then(|s| s.to_str());
        if !matches!(extension, Some("json" | "md")) {
            continue;
        }

        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(error) => {
                warnings.push(AgentLoadWarning {
                    path,
                    reason: format!("cannot read it: {error}"),
                });
                continue;
            }
        };

        let parsed = match extension {
            Some("md") => parse_markdown_agent(&path, &contents),
            _ => serde_json::from_str::<CustomAgentDef>(&contents)
                .map_err(|error| format!("not a valid custom agent definition: {error}")),
        };

        let def = match parsed {
            Ok(d) => d,
            Err(reason) => {
                warnings.push(AgentLoadWarning { path, reason });
                continue;
            }
        };

        if let Err(error) = validate_custom_agent(&def) {
            warnings.push(AgentLoadWarning {
                path,
                reason: format!("invalid definition: {error}"),
            });
            continue;
        }

        agents.insert(def.name.clone(), def);
    }

    (agents, warnings)
}

/// Parse a Markdown agent definition.
///
/// The body becomes the system prompt and the frontmatter supplies the
/// metadata: `description`, `tools` (a comma- or space-separated list),
/// `model`, and `max_tokens`. The file stem is the name, so two directories
/// holding `reviewer.md` refer to the same agent. Frontmatter is best-effort,
/// matching the custom-command reader: an unknown key is ignored.
///
/// A file with no body is rejected, because an agent with an empty system
/// prompt behaves like a generic agent under a name that promised otherwise.
fn parse_markdown_agent(path: &Path, contents: &str) -> Result<CustomAgentDef, String> {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "the file name is not a usable agent name".to_string())?;

    let (frontmatter, body) = split_frontmatter(contents);
    let mut description = None;
    let mut allowed_tools = Vec::new();
    let mut model = None;
    let mut max_tokens = None;

    for line in frontmatter.unwrap_or_default().lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        match key.trim() {
            "description" if !value.is_empty() => description = Some(value.to_string()),
            "tools" | "allowed_tools" if !value.is_empty() => {
                allowed_tools = value
                    .split([',', ' '])
                    .map(str::trim)
                    .filter(|tool| !tool.is_empty())
                    .map(String::from)
                    .collect();
            }
            "model" if !value.is_empty() => model = Some(value.to_string()),
            "max_tokens" => {
                max_tokens = Some(value.parse::<u32>().map_err(|error| {
                    format!("`max_tokens` is not a number (`{value}`): {error}")
                })?);
            }
            _ => {}
        }
    }

    Ok(CustomAgentDef {
        name: name.to_string(),
        system_prompt: body.trim().to_string(),
        allowed_tools,
        model,
        max_tokens,
        output_schema: None,
        description,
    })
}

/// Split a Markdown file into its YAML frontmatter and its body.
///
/// Returns `(None, content)` when the file does not start with a `---` block
/// closed by a later `---` line — such a file is a plain prompt with no
/// metadata, not an error.
fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let Some(rest) = content.strip_prefix("---") else {
        return (None, content);
    };
    // The opening fence must end its line, or `---foo` would be read as one.
    let Some(rest) = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
    else {
        return (None, content);
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let frontmatter = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return (Some(frontmatter), body);
        }
        offset += line.len();
    }
    (None, content)
}

/// Reject a definition that parses but cannot be used.
///
/// A definition with no name cannot be selected, and one with no system
/// prompt silently behaves like a generic agent. Both are reported rather
/// than dropped so the author can fix the file.
fn validate_custom_agent(def: &CustomAgentDef) -> Result<(), String> {
    if def.name.trim().is_empty() {
        return Err("the definition has an empty `name`".into());
    }
    if def.system_prompt.trim().is_empty() {
        return Err(format!("agent '{}' has an empty `system_prompt`", def.name));
    }
    Ok(())
}

/// Resolve a custom agent definition by name.
#[must_use]
pub fn get_custom_agent<'a, S: std::hash::BuildHasher>(
    name: &str,
    agents: &'a HashMap<String, CustomAgentDef, S>,
) -> Option<&'a CustomAgentDef> {
    agents.get(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn load_custom_agents_reads_json_files() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        let def = CustomAgentDef {
            name: "test_agent".into(),
            system_prompt: "You are a test agent.".into(),
            allowed_tools: vec!["Read".into()],
            model: Some("claude-sonnet".into()),
            max_tokens: Some(1024),
            output_schema: None,
            description: None,
        };

        let mut file = std::fs::File::create(agents_dir.join("test.json")).unwrap();
        file.write_all(serde_json::to_string(&def).unwrap().as_bytes())
            .unwrap();

        let loaded = load_custom_agents_from_dir(&agents_dir);
        let loaded_def = loaded.get("test_agent").expect("agent was not loaded");
        assert_eq!(loaded_def.system_prompt, "You are a test agent.");
        assert_eq!(loaded_def.allowed_tools, vec!["Read"]);
        assert_eq!(loaded_def.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(loaded_def.max_tokens, Some(1024));
    }

    #[test]
    fn missing_directory_loads_nothing() {
        let loaded =
            load_custom_agents_from_dir(std::path::Path::new("/definitely/not/a/real/dir"));
        assert!(loaded.is_empty());
    }

    #[test]
    fn malformed_files_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("broken.json"), "{ not json").unwrap();
        std::fs::write(tmp.path().join("ignored.txt"), "not an agent").unwrap();
        assert!(load_custom_agents_from_dir(tmp.path()).is_empty());
    }

    /// A file that cannot be read or parsed, and a definition that parses but
    /// is invalid, are each reported by path; the valid file still loads.
    #[test]
    fn bad_files_warn_and_valid_definitions_still_load() {
        let tmp = tempfile::tempdir().unwrap();

        let valid = CustomAgentDef {
            name: "valid_agent".into(),
            system_prompt: "prompt".into(),
            allowed_tools: vec![],
            model: None,
            max_tokens: None,
            output_schema: None,
            description: None,
        };
        std::fs::write(
            tmp.path().join("valid.json"),
            serde_json::to_string(&valid).unwrap(),
        )
        .unwrap();
        std::fs::write(tmp.path().join("broken.json"), "{ not json").unwrap();
        std::fs::write(
            tmp.path().join("empty-name.json"),
            serde_json::json!({"name": "  ", "system_prompt": "prompt"}).to_string(),
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("empty-prompt.json"),
            serde_json::json!({"name": "no_prompt", "system_prompt": ""}).to_string(),
        )
        .unwrap();
        // Reading a directory as a file fails, which exercises the read path.
        std::fs::create_dir(tmp.path().join("unreadable.json")).unwrap();

        let (loaded, warnings) = load_custom_agents_from_dir_with_warnings(tmp.path());

        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key("valid_agent"));

        assert_eq!(warnings.len(), 4, "{warnings:?}");
        for file in [
            "broken.json",
            "empty-name.json",
            "empty-prompt.json",
            "unreadable.json",
        ] {
            assert!(
                warnings
                    .iter()
                    .any(|warning| warning.path.to_string_lossy().ends_with(file)),
                "no warning named {file}: {warnings:?}"
            );
        }
    }

    /// A Markdown definition is the readable form: the body is the prompt,
    /// the file stem is the name, and the frontmatter carries the metadata.
    #[test]
    fn markdown_definitions_are_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("reviewer.md"),
            "---\n\
             description: Reviews diffs for correctness\n\
             tools: Read, Grep, Bash\n\
             model: claude-opus-5-5\n\
             max_tokens: 4096\n\
             ---\n\
             You review code carefully.\n\
             \n\
             Report only defects.\n",
        )
        .unwrap();

        let loaded = load_custom_agents_from_dir(tmp.path());
        let def = loaded
            .get("reviewer")
            .expect("markdown agent was not loaded");
        assert_eq!(def.name, "reviewer");
        assert_eq!(
            def.system_prompt,
            "You review code carefully.\n\nReport only defects."
        );
        assert_eq!(def.allowed_tools, vec!["Read", "Grep", "Bash"]);
        assert_eq!(def.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(def.max_tokens, Some(4096));
        assert_eq!(
            def.description.as_deref(),
            Some("Reviews diffs for correctness")
        );
    }

    /// The body is what the agent is told; trailing whitespace and a missing
    /// frontmatter block both have to leave a usable prompt.
    #[test]
    fn markdown_without_frontmatter_is_still_a_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("plain.md"), "You are a plain agent.\n").unwrap();

        let loaded = load_custom_agents_from_dir(tmp.path());
        let def = loaded.get("plain").expect("plain agent was not loaded");
        assert_eq!(def.system_prompt, "You are a plain agent.");
        assert!(def.allowed_tools.is_empty());
        assert!(def.description.is_none());
    }

    /// An agent whose body is empty behaves like a generic agent under a name
    /// that promised a persona, so it is rejected with the path named.
    #[test]
    fn markdown_with_an_empty_body_warns() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("hollow.md"),
            "---\ndescription: nothing here\n---\n\n",
        )
        .unwrap();

        let (loaded, warnings) = load_custom_agents_from_dir_with_warnings(tmp.path());
        assert!(loaded.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].path.to_string_lossy().ends_with("hollow.md"),
            "{warnings:?}"
        );
    }

    /// A project definition of a name that also exists in the user directory
    /// replaces it, which is the point of checking the project directory
    /// second.
    #[test]
    fn the_project_directory_shadows_the_user_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("home/.codape/agents");
        let project = tmp.path().join("repo/.codape/agents");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();

        std::fs::write(
            user.join("reviewer.json"),
            serde_json::json!({"name": "reviewer", "system_prompt": "user prompt"}).to_string(),
        )
        .unwrap();
        std::fs::write(project.join("reviewer.md"), "Project prompt.\n").unwrap();
        std::fs::write(project.join("tester.md"), "Test prompt.\n").unwrap();

        // `load_custom_agents_for_project` reads the real home directory plus
        // `<cwd>/.codape/agents`; assert the shadowing rule on the merged maps
        // directly so the test never depends on what the machine has
        // configured.
        let mut merged = load_custom_agents_from_dir(&user);
        merged.extend(load_custom_agents_from_dir(&project));

        assert_eq!(merged["reviewer"].system_prompt, "Project prompt.");
        assert!(merged.contains_key("tester"));
    }

    #[test]
    fn get_custom_agent_finds_existing() {
        let mut agents = HashMap::new();
        agents.insert(
            "my_agent".into(),
            CustomAgentDef {
                name: "my_agent".into(),
                system_prompt: "prompt".into(),
                allowed_tools: vec![],
                model: None,
                max_tokens: None,
                output_schema: None,
                description: None,
            },
        );

        assert!(get_custom_agent("my_agent", &agents).is_some());
        assert!(get_custom_agent("missing", &agents).is_none());
    }
}
