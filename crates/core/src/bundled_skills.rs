//! Skills that ship inside the binary.
//!
//! A bundled skill is compiled in rather than written to the config home: it
//! is the product's own content, it must be present on a machine that has
//! never run Codape before, and a test run must not be able to create files in
//! a developer's `~/.codape`. The loader, the `Skill` tool and `/skills` all
//! read this list, so all three agree on what a fresh install offers.
//!
//! A bundled skill is a single Markdown file. That is the form this list can
//! carry: a skill that needs assets beside its instructions has a directory,
//! and a directory cannot be embedded as one string — such a skill belongs in
//! `~/.codape/skills/`, where the loader already finds it.

/// One skill compiled into the binary.
#[derive(Debug, Clone, Copy)]
pub struct BundledSkill {
    /// The skill's name, which is also the directory it lives in here.
    pub name: &'static str,
    /// The whole `SKILL.md`, frontmatter included.
    pub content: &'static str,
}

/// Every bundled skill, in the order the adverts and listings show them.
pub const BUNDLED_SKILLS: &[BundledSkill] = &[
    BundledSkill {
        name: "code-review",
        content: include_str!("../bundled-skills/code-review/SKILL.md"),
    },
    BundledSkill {
        name: "commit",
        content: include_str!("../bundled-skills/commit/SKILL.md"),
    },
    BundledSkill {
        name: "debug",
        content: include_str!("../bundled-skills/debug/SKILL.md"),
    },
    BundledSkill {
        name: "explain",
        content: include_str!("../bundled-skills/explain/SKILL.md"),
    },
    BundledSkill {
        name: "security-review",
        content: include_str!("../bundled-skills/security-review/SKILL.md"),
    },
    BundledSkill {
        name: "simplify",
        content: include_str!("../bundled-skills/simplify/SKILL.md"),
    },
    BundledSkill {
        name: "test-failure-triage",
        content: include_str!("../bundled-skills/test-failure-triage/SKILL.md"),
    },
];

/// The bundled skill with this name.
#[must_use]
pub fn find(name: &str) -> Option<&'static BundledSkill> {
    BUNDLED_SKILLS.iter().find(|skill| skill.name == name)
}

/// The bundled skill names, sorted as the listings print them.
#[must_use]
pub fn names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = BUNDLED_SKILLS.iter().map(|skill| skill.name).collect();
    names.sort_unstable();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_skill_parses() {
        for skill in BUNDLED_SKILLS {
            let parsed = parse_check(skill.content)
                .unwrap_or_else(|| panic!("bundled skill '{}' has no frontmatter", skill.name));
            assert_eq!(
                parsed, skill.name,
                "the frontmatter name must match the directory and the lookup key"
            );
        }
    }

    #[test]
    fn every_bundled_skill_has_a_description() {
        for skill in BUNDLED_SKILLS {
            assert!(
                !description(skill.content).is_empty(),
                "bundled skill '{}' has no description, so the advert would not say what it is for",
                skill.name
            );
        }
    }

    #[test]
    fn names_are_sorted_and_unique() {
        let names = names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted);
    }

    #[test]
    fn find_is_exact() {
        assert!(find("code-review").is_some());
        assert!(find("Code-Review").is_none());
        assert!(find("").is_none());
    }

    /// The name the frontmatter declares, or `None` when there is no
    /// frontmatter block. Deliberately tiny: the real parser lives in
    /// `codape-plugins`, which depends on this crate, so it cannot be used
    /// here. This only has to catch a bundled file that would not load.
    fn parse_check(content: &str) -> Option<String> {
        let rest = content.strip_prefix("---")?;
        let end = rest.find("\n---")?;
        for line in rest[..end].lines() {
            if let Some(name) = line.strip_prefix("name:") {
                let name = name.trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
        None
    }

    fn description(content: &str) -> String {
        let Some(rest) = content.strip_prefix("---") else {
            return String::new();
        };
        let Some(end) = rest.find("\n---") else {
            return String::new();
        };
        rest[..end]
            .lines()
            .find_map(|line| line.strip_prefix("description:"))
            .map(|value| value.trim().to_string())
            .unwrap_or_default()
    }
}
