//! Skill tool — load and execute skill scripts.

use crate::context::{ToolError, ToolOutput};
use crate::tool::{Tool, ToolContext, ToolDescriptor, ToolInput, ToolTier};
use async_trait::async_trait;

/// Load and execute skill scripts from .codape/skills/.
#[derive(Debug)]
pub struct SkillTool;

#[async_trait]
impl Tool for SkillTool {
    fn description(&self) -> Option<&'static str> {
        Some(
            "Loads a skill: a markdown file under .codape/skills/ or ~/.codape/skills/, or one of \
             the skills built into Codape. A skill carries instructions for a repeatable task. \
             Only call it when the adverts you were given name the skill.",
        )
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "Skill".into(),
            tier: ToolTier::Baseline,
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["skill"],
                "properties": {
                    "skill": { "type": "string", "description": "Skill name to load" },
                    "args": { "type": "string", "description": "Arguments to pass to the skill" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "result": { "type": "string" }
                }
            }),
            requires_approval: false,
            is_enabled: true,
            is_read_only: true,
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

        let skill = extract_string(&input.arguments, "skill")?;
        let args = extract_string_opt(&input.arguments, "args");

        // A bundled skill is compiled into the binary, so it is not under any
        // directory the search below can reach. Look it up first: the listing
        // the model was given names it, and a tool that cannot open what the
        // advert offered is worse than no advert at all.
        if let Some(bundled) = codape_core::bundled_skills::find(&skill) {
            let mut output = format!("Skill '{skill}' (bundled)\n\n");
            output.push_str(bundled.content);
            if let Some(a) = args {
                let _ = write!(output, "\n\nArguments: {a}");
            }
            return Ok(ToolOutput {
                output,
                is_display: true,
                images: Vec::new(),
            });
        }

        // A skill is either a single Markdown file — `pdf.md` — or a directory
        // that owns its assets and enters through `SKILL.md`. The loader
        // accepts both; this tool has to accept the same two forms or a skill
        // that loaded at startup is one the model cannot open.
        let search_dirs = [
            ctx.cwd.join(".codape/skills"),
            codape_config::settings::config_home().join("skills"),
        ];

        for dir in &search_dirs {
            for path in skill_candidates(dir, &skill) {
                if !path.exists() {
                    continue;
                }
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        let mut output =
                            format!("Skill '{skill}' loaded from {}\n\n", path.display());
                        output.push_str(&content);
                        if let Some(a) = args {
                            let _ = write!(output, "\n\nArguments: {a}");
                        }
                        return Ok(ToolOutput {
                            output,
                            is_display: true,
                            images: Vec::new(),
                        });
                    }
                    Err(e) => {
                        return Err(ToolError::InvalidInput {
                            message: format!("failed to read skill file: {e}"),
                        });
                    }
                }
            }
        }

        // Skill not found — list available skills
        let mut available: Vec<String> = codape_core::bundled_skills::names()
            .into_iter()
            .map(str::to_string)
            .collect();
        for dir in &search_dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                    {
                        available.push(
                            path.file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string(),
                        );
                    } else if path.join("SKILL.md").is_file() {
                        available.push(entry.file_name().to_string_lossy().to_string());
                    }
                }
            }
        }
        available.sort();
        available.dedup();

        let mut msg = format!("Skill '{skill}' not found.");
        if available.is_empty() {
            msg.push_str("\nNo skills found in .codape/skills/ or ~/.codape/skills/");
        } else {
            let _ = write!(msg, "\nAvailable skills: {}", available.join(", "));
        }
        Err(ToolError::InvalidInput { message: msg })
    }
}

/// The paths a skill name could resolve to, in the order to try them.
///
/// The single-file form first, so a loose `pdf.md` still wins over a
/// `pdf/` directory of the same name — the same order the loader's directory
/// walk produces.
fn skill_candidates(dir: &std::path::Path, skill: &str) -> [std::path::PathBuf; 2] {
    [
        dir.join(format!("{skill}.md")),
        dir.join(skill).join("SKILL.md"),
    ]
}

fn extract_string(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
        .ok_or_else(|| ToolError::InvalidInput {
            message: format!("missing required field: {key}"),
        })
}

fn extract_string_opt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecutionContext;
    use crate::tool::ToolContext;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    fn test_ctx() -> ToolContext<'static> {
        ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: Path::new("/tmp"),
            extra: Arc::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn skill_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".codape/skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("pdf.md"),
            "# PDF Skill\nExtract text from PDF files.",
        )
        .unwrap();

        let ctx = ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: tmp.path(),
            extra: Arc::new(HashMap::new()),
        };

        let output = SkillTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Skill".into(),
                    arguments: serde_json::json!({ "skill": "pdf" }),
                },
                ctx,
            )
            .await
            .unwrap();
        assert!(output.output.contains("pdf"));
        assert!(output.output.contains("Extract text"));
    }

    /// A bundled skill is compiled in, so it is not under any directory the
    /// tool searches. If the adverts name it, the tool has to be able to open
    /// it — and it must open without the config home being touched.
    #[tokio::test]
    async fn a_bundled_skill_loads_from_the_binary() {
        let output = SkillTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Skill".into(),
                    arguments: serde_json::json!({ "skill": "code-review" }),
                },
                test_ctx(),
            )
            .await
            .unwrap();
        assert!(output.output.contains("(bundled)"), "{}", output.output);
        assert!(
            output.output.contains("name: code-review"),
            "{}",
            output.output
        );
    }

    /// A miss names the bundled skills, or a model that guessed wrong is told
    /// only about what happens to be on disk.
    #[tokio::test]
    async fn a_missed_name_lists_bundled_skills() {
        let error = SkillTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Skill".into(),
                    arguments: serde_json::json!({ "skill": "nope" }),
                },
                test_ctx(),
            )
            .await
            .expect_err("a missing skill is an error");
        let message = format!("{error:?}");
        assert!(message.contains("code-review"), "{message}");
    }

    #[tokio::test]
    async fn skill_missing_name() {
        let result = SkillTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Skill".into(),
                    arguments: serde_json::json!({}),
                },
                test_ctx(),
            )
            .await;
        assert!(result.is_err());
    }

    /// A directory skill is reached through its `SKILL.md`, like the loader.
    #[tokio::test]
    async fn a_directory_skill_loads_from_its_skill_markdown() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join(".codape/skills/pdf");
        std::fs::create_dir_all(skill_dir.join("templates")).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "# PDF Skill\nExtract text from PDF files.",
        )
        .unwrap();
        std::fs::write(skill_dir.join("templates/invoice.html"), "<html>").unwrap();

        let ctx = ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: tmp.path(),
            extra: Arc::new(HashMap::new()),
        };

        let output = SkillTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Skill".into(),
                    arguments: serde_json::json!({ "skill": "pdf" }),
                },
                ctx,
            )
            .await
            .unwrap();
        assert!(output.output.contains("pdf/SKILL.md"), "{}", output.output);
        assert!(output.output.contains("Extract text"), "{}", output.output);
    }

    /// The miss message lists directory skills too, or a model that guessed
    /// wrong is told only about the single-file ones.
    #[tokio::test]
    async fn a_missed_name_lists_directory_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".codape/skills");
        std::fs::create_dir_all(skills_dir.join("pdf")).unwrap();
        std::fs::write(skills_dir.join("pdf/SKILL.md"), "---\nname: pdf\n---\nbody").unwrap();

        let ctx = ToolContext {
            elicit_tx: None,
            path_verifier: None,
            execution_context: ExecutionContext::Normal,
            cwd: tmp.path(),
            extra: Arc::new(HashMap::new()),
        };

        let error = SkillTool
            .execute(
                ToolInput {
                    call_id: "1".into(),
                    name: "Skill".into(),
                    arguments: serde_json::json!({ "skill": "nope" }),
                },
                ctx,
            )
            .await
            .expect_err("a missing skill is an error");
        let message = format!("{error:?}");
        assert!(message.contains("pdf"), "{message}");
    }
}
