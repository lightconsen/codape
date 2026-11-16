//! Skill loading — bundled, user dir, repo dir, plugin, and MCP skills.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use yaml_rust2::{Yaml, YamlLoader};

/// Parsed skill frontmatter metadata.
#[derive(Debug, Clone)]
pub struct SkillMetadata {
    /// The skill name.
    pub name: String,
    /// Hooks that trigger this skill.
    pub hooks: Vec<String>,
    /// Optional agent this skill is associated with.
    pub agent: Option<String>,
    /// Tools this skill is allowed to use.
    pub allowed_tools: Vec<String>,
    /// Skill description for display.
    pub description: String,
    /// Whether the skill is enabled.
    pub enabled: bool,
}

/// A loaded skill.
#[derive(Debug, Clone)]
pub struct Skill {
    /// The skill's parsed metadata.
    pub metadata: SkillMetadata,
    /// Where this skill was loaded from.
    pub source: SkillSource,
    /// The path to the skill file.
    pub path: PathBuf,
    /// For plugin skills, the name of the owning plugin.
    pub plugin_name: Option<String>,
}

/// Where a skill was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillSource {
    /// Bundled with Codape.
    Bundled,
    /// Loaded from the user's skill directory.
    UserDir,
    /// Loaded from the repository's skill directory.
    RepoDir,
    /// Loaded from a plugin.
    Plugin,
    /// Discovered via MCP.
    Mcp,
}

impl SkillSource {
    /// Precedence for conflict resolution.
    /// Higher value = higher priority.
    #[must_use]
    pub const fn precedence(self) -> u8 {
        match self {
            Self::UserDir => 5,
            Self::RepoDir => 4,
            Self::Bundled => 3,
            Self::Plugin => 2,
            Self::Mcp => 1,
        }
    }
}

impl std::fmt::Display for SkillSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bundled => write!(f, "bundled"),
            Self::UserDir => write!(f, "user"),
            Self::RepoDir => write!(f, "repo"),
            Self::Plugin => write!(f, "plugin"),
            Self::Mcp => write!(f, "mcp"),
        }
    }
}

/// A skill script file — the actual content loaded from disk.
#[derive(Debug)]
pub struct SkillFile {
    /// The parsed frontmatter.
    pub metadata: SkillMetadata,
    /// The script content (after the frontmatter).
    pub script: String,
}

/// Parse a field that may be a single string or a list of strings.
///
/// `None` means the value has the wrong shape, which makes the whole
/// frontmatter unparseable.
fn string_or_list(value: Option<&Yaml>) -> Option<Vec<String>> {
    match value {
        None => Some(Vec::new()),
        Some(Yaml::String(text)) => Some(vec![text.clone()]),
        Some(Yaml::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect(),
        Some(_) => None,
    }
}

/// Intermediate representation for YAML frontmatter parsing.
struct SkillFrontmatter {
    name: String,
    hooks: Vec<String>,
    agent: Option<String>,
    allowed_tools: Vec<String>,
    description: String,
    enabled: bool,
}

/// Parse the YAML frontmatter block.
///
/// The shape mirrors the previous serde model: `name` is required; `hooks` and
/// `allowed_tools` accept a string or a list of strings; `agent` is optional;
/// `description` defaults to empty; `enabled` defaults to true. Unknown keys
/// are ignored, and a field of the wrong type makes the whole block invalid.
fn parse_frontmatter(frontmatter: &str) -> Option<SkillFrontmatter> {
    let documents = YamlLoader::load_from_str(frontmatter).ok()?;
    let mapping = documents.first()?.as_hash()?;
    let field = |name: &str| mapping.get(&Yaml::String(name.to_string()));
    Some(SkillFrontmatter {
        name: field("name")?.as_str()?.to_string(),
        hooks: string_or_list(field("hooks"))?,
        agent: match field("agent") {
            None | Some(Yaml::Null) => None,
            Some(value) => Some(value.as_str()?.to_string()),
        },
        allowed_tools: string_or_list(field("allowed_tools"))?,
        description: match field("description") {
            None => String::new(),
            Some(value) => value.as_str()?.to_string(),
        },
        enabled: match field("enabled") {
            None => true,
            Some(Yaml::Boolean(value)) => *value,
            Some(_) => return None,
        },
    })
}

/// Whether a path is a Markdown file, by extension.
fn is_markdown_file(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

/// Parse skill frontmatter from markdown file content.
/// Expected format:
/// ```markdown
/// ---
/// name: my-skill
/// hooks: [pre-query, post-tool]
/// agent: general
/// allowed_tools: [Bash, Read]
/// description: A skill that does X
/// enabled: true
/// ---
/// script content here...
/// ```
#[must_use]
pub fn parse_skill_file(content: &str) -> Option<SkillFile> {
    if !content.starts_with("---") {
        return None;
    }

    // Find the closing ---
    let closing = content[3..].find("\n---\n")?;
    let frontmatter_end = 3 + closing + 5; // "---\n" is 4 chars + newline
    let frontmatter = &content[3..3 + closing];

    let parsed = parse_frontmatter(frontmatter)?;

    if parsed.name.is_empty() {
        return None;
    }

    let script = content[frontmatter_end..].to_string();

    Some(SkillFile {
        metadata: SkillMetadata {
            name: parsed.name,
            hooks: parsed.hooks,
            agent: parsed.agent,
            allowed_tools: parsed.allowed_tools,
            description: parsed.description,
            enabled: parsed.enabled,
        },
        script,
    })
}

/// Loads skills from all configured sources.
#[derive(Debug)]
pub struct SkillLoader {
    /// Bundled skills root path.
    bundled_path: PathBuf,
    /// User skills directory.
    user_path: PathBuf,
    /// Loaded skills indexed by name.
    skills: Vec<Skill>,
}

impl SkillLoader {
    /// Create a new skill loader.
    #[must_use]
    pub const fn new(bundled_path: PathBuf, user_path: PathBuf) -> Self {
        Self {
            bundled_path,
            user_path,
            skills: Vec::new(),
        }
    }

    /// Discover and load skills from all sources.
    ///
    /// After loading, conflicts are resolved by precedence:
    /// `UserDir > RepoDir > Bundled > Plugin > Mcp`.
    pub fn load_all(&mut self, repo_path: Option<&Path>) {
        self.skills.clear();

        // 1. Bundled skills
        let bundled = self.bundled_path.clone();
        self.load_from_dir(&bundled, SkillSource::Bundled);

        // 2. User skills
        let user = self.user_path.clone();
        self.load_from_dir(&user, SkillSource::UserDir);

        // 3. Repo skills
        if let Some(repo) = repo_path {
            let repo_skills = repo.join(".codape").join("skills");
            if repo_skills.exists() {
                self.load_from_dir(&repo_skills, SkillSource::RepoDir);
            }
        }

        // 4. Resolve conflicts by precedence
        self.resolve_conflicts();
    }

    /// Add the skills compiled into the binary.
    ///
    /// These are the product's own skills, and they are added in memory
    /// rather than installed to the config home: a fresh install has them
    /// without a session ever writing to `~/.codape`, and a test run cannot
    /// leave files behind. The bundled directory is still read first, so a
    /// user who unpacks a skill of the same name there overrides this one.
    /// Call after [`load_all`](Self::load_all) so precedence can be settled
    /// with everything else.
    pub fn add_bundled_skills(&mut self) {
        for bundled in codape_core::bundled_skills::BUNDLED_SKILLS {
            let Some(parsed) = parse_skill_file(bundled.content) else {
                // A bundled file that does not parse is a build defect; the
                // core crate's own test catches it before this can run.
                tracing::warn!(skill = bundled.name, "bundled skill did not parse");
                continue;
            };
            self.skills.push(Skill {
                path: PathBuf::from(format!("<bundled>/{}", bundled.name)),
                metadata: parsed.metadata,
                source: SkillSource::Bundled,
                plugin_name: None,
            });
        }
        self.resolve_conflicts();
    }

    /// Resolve duplicate skill names by source precedence.
    ///
    /// Logs a warning for each overridden skill.
    fn resolve_conflicts(&mut self) {
        use std::collections::HashMap;

        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        for (idx, skill) in self.skills.iter().enumerate() {
            by_name.entry(&skill.metadata.name).or_default().push(idx);
        }

        let mut keep: Vec<bool> = vec![true; self.skills.len()];
        for (_name, indices) in by_name {
            if indices.len() <= 1 {
                continue;
            }
            // Find the highest-precedence skill
            let best_idx = *indices
                .iter()
                .max_by_key(|&&idx| self.skills[idx].source.precedence())
                .unwrap_or(&indices[0]);

            for &idx in &indices {
                if idx != best_idx {
                    let overridden = &self.skills[idx];
                    let winner = &self.skills[best_idx];
                    tracing::warn!(
                        skill = %overridden.metadata.name,
                        overridden_source = %overridden.source,
                        winner_source = %winner.source,
                        "skill conflict resolved: {} overridden by {}",
                        overridden.source,
                        winner.source
                    );
                    keep[idx] = false;
                }
            }
        }

        let mut retained = Vec::with_capacity(self.skills.len());
        for (idx, skill) in self.skills.drain(..).enumerate() {
            if keep[idx] {
                retained.push(skill);
            }
        }
        self.skills = retained;
    }

    /// Load skills from a directory.
    ///
    /// A skill is either a single file — `pdf.md` — or a directory holding its
    /// own assets, in which case the entry point is `<dir>/SKILL.md`. The second
    /// form is what lets a skill ship a template, a script or a reference file
    /// beside its instructions; without it a skill was one Markdown file and
    /// nothing could sit next to it.
    ///
    /// A directory with no `SKILL.md` is skipped silently: not every directory
    /// under `skills/` is a skill (a shared `assets/` folder is not), and a
    /// warning per directory would be noise.
    fn load_from_dir(&mut self, dir: &Path, source: SkillSource) {
        if !dir.exists() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                is_markdown_file(path) || path.is_dir() && path.join("SKILL.md").is_file()
            })
            .map(|path| {
                if path.is_dir() {
                    path.join("SKILL.md")
                } else {
                    path
                }
            })
            .collect();
        paths.sort();

        for path in paths {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(skill_file) = parse_skill_file(&content) else {
                continue;
            };
            self.skills.push(Skill {
                metadata: skill_file.metadata,
                source,
                path,
                plugin_name: None,
            });
        }
    }

    /// Add a skill from a plugin.
    pub fn add_plugin_skill(
        &mut self,
        metadata: SkillMetadata,
        path: PathBuf,
        plugin_name: impl Into<String>,
    ) {
        self.skills.push(Skill {
            metadata,
            source: SkillSource::Plugin,
            path,
            plugin_name: Some(plugin_name.into()),
        });
    }

    /// Add a skill from MCP discovery.
    pub fn add_mcp_skill(&mut self, metadata: SkillMetadata) {
        self.skills.push(Skill {
            metadata,
            source: SkillSource::Mcp,
            path: PathBuf::new(),
            plugin_name: None,
        });
    }

    /// Execute a skill by name.
    ///
    /// Plugin skills are forwarded to their owning plugin process.
    /// Non-plugin skills return their script path as a prompt template reference.
    pub async fn execute_skill(
        &self,
        name: &str,
        context: &serde_json::Value,
        plugin_manager: &crate::plugins::PluginManager,
    ) -> Result<String, String> {
        let skill = self
            .get(name)
            .ok_or_else(|| format!("skill '{name}' not found"))?;

        match skill.source {
            SkillSource::Plugin => {
                let plugin_name = skill
                    .plugin_name
                    .as_deref()
                    .ok_or_else(|| format!("skill '{name}' has no associated plugin"))?;
                plugin_manager
                    .execute_skill(plugin_name, name, context)
                    .await
            }
            _ => {
                // Non-plugin skills are prompt templates — return their script path
                Ok(skill.path.to_string_lossy().to_string())
            }
        }
    }

    /// Consume the loader, keeping only the skills that are enabled.
    ///
    /// The session wants to own the list — it holds it for as long as it runs
    /// and hands it to the prompt — and a disabled skill is a skill the model
    /// must not be told about.
    #[must_use]
    pub fn into_enabled(self) -> Vec<Skill> {
        self.skills
            .into_iter()
            .filter(|skill| skill.metadata.enabled)
            .collect()
    }

    /// Get a skill by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.metadata.name == name)
    }

    /// List all enabled skills.
    #[must_use]
    pub fn enabled(&self) -> Vec<&Skill> {
        self.skills.iter().filter(|s| s.metadata.enabled).collect()
    }

    /// List all skills with their source.
    #[must_use]
    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    /// Count skills by source.
    #[must_use]
    pub fn count_by_source(&self) -> Vec<(SkillSource, usize)> {
        let mut counts = std::collections::HashMap::new();
        for skill in &self.skills {
            *counts.entry(skill.source).or_insert(0) += 1;
        }
        counts.into_iter().collect()
    }
}

/// Render the block that tells the model which skills exist.
///
/// A skill is only useful if the model knows to reach for it. Before this the
/// loader ran and nothing was ever said about what it found, so `Skill` was a
/// tool the model would only call by guessing a name. The block names each
/// skill and its description, which is exactly what the tool's own error
/// message would have had to supply after a miss.
///
/// Descriptions are truncated: a description is a hint, not the skill, and a
/// directory of verbose skills must not crowd out the conversation.
#[must_use]
pub fn render_skill_adverts(skills: &[Skill]) -> String {
    const MAX_DESCRIPTION: usize = 200;

    let enabled: Vec<&Skill> = skills
        .iter()
        .filter(|skill| skill.metadata.enabled)
        .collect();
    if enabled.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "Skills available to you. Call the Skill tool with the name when one applies; do not \
         guess a name that is not listed.\n",
    );
    for skill in enabled {
        let description = skill
            .metadata
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if description.is_empty() {
            let _ = write!(out, "\n- {}", skill.metadata.name);
        } else if description.chars().count() > MAX_DESCRIPTION {
            let head: String = description.chars().take(MAX_DESCRIPTION).collect();
            let _ = write!(out, "\n- {}: {head}…", skill.metadata.name);
        } else {
            let _ = write!(out, "\n- {}: {description}", skill.metadata.name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_content() -> String {
        r"---
name: test-skill
hooks: [pre-query]
agent: general
allowed_tools: [Bash, Read]
description: A test skill
enabled: true
---
This is the script content.
"
        .to_string()
    }

    #[test]
    fn parse_valid_skill() {
        let content = skill_content();
        let parsed = parse_skill_file(&content).unwrap();
        assert_eq!(parsed.metadata.name, "test-skill");
        assert_eq!(parsed.metadata.hooks, vec!["pre-query"]);
        assert_eq!(parsed.metadata.agent, Some("general".into()));
        assert_eq!(parsed.metadata.allowed_tools, vec!["Bash", "Read"]);
        assert!(parsed.metadata.enabled);
        assert_ne!(parsed.script, "");
    }

    #[test]
    fn parse_no_frontmatter_returns_none() {
        assert!(parse_skill_file("just some text").is_none());
    }

    #[test]
    fn parse_empty_name_returns_none() {
        let content = "---\nname: \n---\nscript";
        assert!(parse_skill_file(content).is_none());
    }

    #[test]
    fn parse_disabled_skill() {
        let content = "---\nname: disabled\nenabled: false\n---\nscript";
        let parsed = parse_skill_file(content).unwrap();
        assert!(!parsed.metadata.enabled);
    }

    #[test]
    fn parse_single_hook() {
        let content = "---\nname: x\nhooks: hook1\n---\ns";
        let parsed = parse_skill_file(content).unwrap();
        assert_eq!(parsed.metadata.hooks, vec!["hook1"]);
    }

    #[test]
    fn loader_load_from_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("test.md");
        std::fs::write(&skill_path, skill_content()).unwrap();

        let mut loader = SkillLoader::new(dir.path().to_path_buf(), PathBuf::from("/nonexistent"));
        loader.load_all(None);

        assert_eq!(loader.list().len(), 1);
        assert_eq!(loader.enabled().len(), 1);
    }

    #[test]
    fn loader_nonexistent_dir_is_silent() {
        let mut loader = SkillLoader::new(
            PathBuf::from("/nonexistent"),
            PathBuf::from("/also-nonexistent"),
        );
        loader.load_all(None);
        assert!(loader.list().is_empty());
    }

    #[test]
    fn loader_add_plugin_skill() {
        let mut loader = SkillLoader::new(PathBuf::new(), PathBuf::new());
        loader.add_plugin_skill(
            SkillMetadata {
                name: "plugin-skill".into(),
                hooks: vec!["pre-query".into()],
                agent: None,
                allowed_tools: vec![],
                description: "From plugin".into(),
                enabled: true,
            },
            PathBuf::from("/plugin/test.md"),
            "test-plugin",
        );

        let skill = loader.get("plugin-skill").unwrap();
        assert_eq!(skill.source, SkillSource::Plugin);
    }

    #[test]
    fn loader_add_mcp_skill() {
        let mut loader = SkillLoader::new(PathBuf::new(), PathBuf::new());
        loader.add_mcp_skill(SkillMetadata {
            name: "mcp-skill".into(),
            hooks: vec![],
            agent: None,
            allowed_tools: vec![],
            description: "From MCP".into(),
            enabled: true,
        });

        assert_eq!(loader.get("mcp-skill").unwrap().source, SkillSource::Mcp);
    }

    #[test]
    fn count_by_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), skill_content()).unwrap();

        let mut loader = SkillLoader::new(dir.path().to_path_buf(), PathBuf::new());
        loader.load_all(None);
        loader.add_mcp_skill(SkillMetadata {
            name: "m".into(),
            hooks: vec![],
            agent: None,
            allowed_tools: vec![],
            description: String::new(),
            enabled: true,
        });

        let counts = loader.count_by_source();
        assert!(
            counts
                .iter()
                .any(|(s, c)| matches!(s, SkillSource::Bundled) && *c == 1)
        );
        assert!(
            counts
                .iter()
                .any(|(s, c)| matches!(s, SkillSource::Mcp) && *c == 1)
        );
    }

    #[test]
    fn repo_dir_skills_loaded() {
        let repo = tempfile::tempdir().unwrap();
        let skills_dir = repo.path().join(".codape").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(skills_dir.join("repo.md"), skill_content()).unwrap();

        let mut loader =
            SkillLoader::new(PathBuf::from("/nonexistent"), PathBuf::from("/nonexistent"));
        loader.load_all(Some(repo.path()));

        assert_eq!(loader.enabled().len(), 1);
        assert_eq!(loader.enabled()[0].source, SkillSource::RepoDir);
    }

    /// A skill directory carries its own assets, so it is `name/SKILL.md`.
    ///
    /// The single-file form made shipping a template or a script beside a
    /// skill impossible: everything had to be inline. A skill that ships
    /// anything now owns a directory.
    #[test]
    fn a_skill_directory_is_loaded_from_its_skill_markdown() {
        let repo = tempfile::tempdir().unwrap();
        let skills_dir = repo.path().join(".codape").join("skills");
        let skill_dir = skills_dir.join("pdf");
        std::fs::create_dir_all(skill_dir.join("templates")).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), skill_content()).unwrap();
        std::fs::write(skill_dir.join("templates/invoice.html"), "<html>").unwrap();

        let mut loader =
            SkillLoader::new(PathBuf::from("/nonexistent"), PathBuf::from("/nonexistent"));
        loader.load_all(Some(repo.path()));

        let skill = loader.get("test-skill").expect("the directory skill loads");
        assert!(
            skill.path.ends_with("pdf/SKILL.md"),
            "the entry point is SKILL.md: {}",
            skill.path.display()
        );
    }

    /// A directory without `SKILL.md` is not a skill and says nothing.
    #[test]
    fn a_plain_directory_is_not_a_skill() {
        let repo = tempfile::tempdir().unwrap();
        let skills_dir = repo.path().join(".codape").join("skills");
        std::fs::create_dir_all(skills_dir.join("shared")).unwrap();
        std::fs::write(skills_dir.join("shared/notes.md"), "not a skill").unwrap();

        let mut loader =
            SkillLoader::new(PathBuf::from("/nonexistent"), PathBuf::from("/nonexistent"));
        loader.load_all(Some(repo.path()));
        assert!(loader.list().is_empty(), "{:?}", loader.list());
    }

    /// Both forms in one directory are loaded, in a stable order.
    #[test]
    fn single_file_and_directory_skills_coexist() {
        let repo = tempfile::tempdir().unwrap();
        let skills_dir = repo.path().join(".codape").join("skills");
        std::fs::create_dir_all(skills_dir.join("bundled")).unwrap();
        std::fs::write(skills_dir.join("bundled/SKILL.md"), skill_content()).unwrap();
        std::fs::write(
            skills_dir.join("loose.md"),
            "---\nname: loose\ndescription: A loose skill\n---\nBody",
        )
        .unwrap();

        let mut loader =
            SkillLoader::new(PathBuf::from("/nonexistent"), PathBuf::from("/nonexistent"));
        loader.load_all(Some(repo.path()));

        let mut names: Vec<&str> = loader
            .list()
            .iter()
            .map(|s| s.metadata.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, vec!["loose", "test-skill"]);
    }

    #[test]
    fn conflict_resolution_prefers_user_over_bundled() {
        let dir = tempfile::tempdir().unwrap();
        // Write a bundled skill
        std::fs::write(dir.path().join("bundled.md"), skill_content()).unwrap();

        // Write a user skill with same name
        let user_dir = tempfile::tempdir().unwrap();
        std::fs::write(user_dir.path().join("user.md"), skill_content()).unwrap();

        let mut loader = SkillLoader::new(dir.path().to_path_buf(), user_dir.path().to_path_buf());
        loader.load_all(None);

        // Should only have 1 skill (duplicate resolved)
        assert_eq!(loader.list().len(), 1);
        // UserDir takes precedence over Bundled
        assert_eq!(loader.list()[0].source, SkillSource::UserDir);
    }

    #[test]
    fn source_precedence_order() {
        assert!(SkillSource::UserDir.precedence() > SkillSource::RepoDir.precedence());
        assert!(SkillSource::RepoDir.precedence() > SkillSource::Bundled.precedence());
        assert!(SkillSource::Bundled.precedence() > SkillSource::Plugin.precedence());
        assert!(SkillSource::Plugin.precedence() > SkillSource::Mcp.precedence());
    }

    #[test]
    fn frontmatter_parses_quoted_colons() {
        let content = r#"---
name: quoted-skill
description: "This: has a colon"
hooks: [pre-query]
enabled: true
---
script"#;
        let parsed = parse_skill_file(content).unwrap();
        assert_eq!(parsed.metadata.description, "This: has a colon");
    }

    /// The model has to be told what it can reach for.
    ///
    /// A loaded skill that nobody mentions is a tool the model will only call
    /// by guessing. The advert names it and says what it is for.
    #[test]
    fn adverts_name_each_enabled_skill_and_its_description() {
        let mut loader = SkillLoader::new(PathBuf::new(), PathBuf::new());
        loader.add_mcp_skill(SkillMetadata {
            name: "pdf".into(),
            hooks: Vec::new(),
            agent: None,
            allowed_tools: Vec::new(),
            description: "Extract text from a PDF".into(),
            enabled: true,
        });
        loader.add_mcp_skill(SkillMetadata {
            name: "off".into(),
            hooks: Vec::new(),
            agent: None,
            allowed_tools: Vec::new(),
            description: "Should not be mentioned".into(),
            enabled: false,
        });

        let adverts = render_skill_adverts(loader.list());
        assert!(adverts.contains("pdf"), "{adverts}");
        assert!(adverts.contains("Extract text from a PDF"), "{adverts}");
        assert!(
            !adverts.contains("off"),
            "a disabled skill must not be advertised: {adverts}"
        );
    }

    #[test]
    fn no_skills_produce_no_advert_block() {
        assert_eq!(render_skill_adverts(&[]), "");
        let mut loader = SkillLoader::new(PathBuf::new(), PathBuf::new());
        loader.add_mcp_skill(SkillMetadata {
            name: "off".into(),
            hooks: Vec::new(),
            agent: None,
            allowed_tools: Vec::new(),
            description: String::new(),
            enabled: false,
        });
        assert_eq!(render_skill_adverts(loader.list()), "");
    }

    #[test]
    fn a_directory_skill_is_handed_to_the_session_as_an_enabled_skill() {
        let repo = tempfile::tempdir().unwrap();
        let skills_dir = repo.path().join(".codape").join("skills");
        std::fs::create_dir_all(skills_dir.join("pdf")).unwrap();
        std::fs::write(skills_dir.join("pdf/SKILL.md"), skill_content()).unwrap();

        let mut loader =
            SkillLoader::new(PathBuf::from("/nonexistent"), PathBuf::from("/nonexistent"));
        loader.load_all(Some(repo.path()));
        let skills = loader.into_enabled();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.name, "test-skill");
    }

    /// The skills compiled into the binary reach the session without anything
    /// being written to the config home.
    #[test]
    fn bundled_skills_load_without_touching_the_disk() {
        let mut loader =
            SkillLoader::new(PathBuf::from("/nonexistent"), PathBuf::from("/nonexistent"));
        loader.load_all(None);
        loader.add_bundled_skills();

        assert_eq!(
            loader.list().len(),
            codape_core::bundled_skills::BUNDLED_SKILLS.len()
        );
        let skill = loader.get("code-review").expect("the bundled skill loads");
        assert_eq!(skill.source, SkillSource::Bundled);
        assert!(!skill.metadata.description.is_empty());
        assert!(skill.metadata.enabled, "a bundled skill is on by default");
    }

    /// A bundled skill is announced to the model, which is the whole point:
    /// a skill nobody names is a tool call the model has to guess.
    #[test]
    fn bundled_skills_appear_in_the_adverts() {
        let mut loader = SkillLoader::new(PathBuf::new(), PathBuf::new());
        loader.add_bundled_skills();
        let adverts = render_skill_adverts(loader.list());
        for name in codape_core::bundled_skills::names() {
            assert!(adverts.contains(name), "{name} missing from:\n{adverts}");
        }
    }

    /// A user skill of the same name overrides the bundled one, and the
    /// bundled copy does not appear twice.
    #[test]
    fn a_user_skill_overrides_a_bundled_one_of_the_same_name() {
        let user = tempfile::tempdir().unwrap();
        std::fs::write(
            user.path().join("code-review.md"),
            "---\nname: code-review\ndescription: Mine\n---\nmy version\n",
        )
        .unwrap();

        let mut loader = SkillLoader::new(PathBuf::from("/nonexistent"), user.path().to_path_buf());
        loader.load_all(None);
        loader.add_bundled_skills();

        let matches: Vec<&Skill> = loader
            .list()
            .iter()
            .filter(|skill| skill.metadata.name == "code-review")
            .collect();
        assert_eq!(matches.len(), 1, "the name must resolve once");
        assert_eq!(matches[0].source, SkillSource::UserDir);
    }
}
