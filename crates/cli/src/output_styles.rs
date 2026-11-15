//! Built-in output styles, alongside the ones plugins contribute.
//!
//! `/style <name>` used to work only when a plugin happened to publish an
//! `outputStyles` map, so a fresh install answered "No output styles
//! available." An output style is a chunk of system-prompt text, and the three
//! that carry their weight ship with the binary: a plain neutral answer, one
//! that explains its reasoning, and one that teaches as it goes.
//!
//! A plugin style of the same name wins: a plugin is a deliberate local
//! choice, and shadowing is the only way to revise a built-in without removing
//! it.

/// One output style: a name, a one-line summary, and the prompt text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputStyle {
    /// The name `/style` takes.
    pub name: &'static str,
    /// One line for `/style` with no arguments.
    pub description: &'static str,
    /// The text appended to the system prompt when the style is applied.
    pub prompt: &'static str,
}

/// Styles that ship with Codape.
pub const BUILT_IN: &[OutputStyle] = &[
    OutputStyle {
        name: "default",
        description: "Direct answers, no meta-commentary",
        prompt: "Answer directly and stay on the task. Do not narrate your \
                 process, explain what you are about to do, or restate the \
                 question. Say what changed and what the person should know \
                 next, and nothing else.",
    },
    OutputStyle {
        name: "explanatory",
        description: "Explain the reasoning behind each decision",
        prompt: "Alongside the work itself, explain the reasoning behind \
                 non-obvious decisions: why this approach over the alternative, \
                 what constraint shaped it, and what it costs. Keep the \
                 explanation next to the thing it explains rather than in a \
                 preamble, and skip it for changes that need no justification.",
    },
    OutputStyle {
        name: "learning",
        description: "Teach the concept while doing the work",
        prompt: "Treat the person as someone learning the codebase. When you \
                 make a non-obvious change, name the pattern or concept it \
                 uses in a sentence and say where else in this codebase it \
                 appears, so the answer teaches the surrounding shape and not \
                 just the fix. Ask a brief question at the end of a turn only \
                 when the answer would change what you do next.",
    },
];

/// Look up a built-in style by name, case-insensitively.
#[must_use]
pub fn built_in(name: &str) -> Option<&'static OutputStyle> {
    BUILT_IN
        .iter()
        .find(|style| style.name.eq_ignore_ascii_case(name))
}

/// Every style name a session can offer, plugin styles first.
///
/// A name a plugin also defines is listed once — under the plugin's copy —
/// because `handle_output_style_command` resolves plugin before built-in.
#[must_use]
pub fn available_names(plugin_names: &[String]) -> Vec<String> {
    let mut names: Vec<String> = plugin_names.to_vec();
    for style in BUILT_IN {
        if !names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(style.name))
        {
            names.push(style.name.to_string());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_has_a_non_empty_prompt() {
        for style in BUILT_IN {
            assert!(!style.name.is_empty(), "a style has no name");
            assert!(
                !style.description.is_empty(),
                "{} has no description",
                style.name
            );
            assert!(!style.prompt.is_empty(), "{} has no prompt", style.name);
        }
    }

    #[test]
    fn the_three_documented_styles_exist() {
        for name in ["default", "explanatory", "learning"] {
            assert!(built_in(name).is_some(), "{name} is missing");
        }
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert_eq!(
            built_in("EXPLANATORY").map(|style| style.name),
            Some("explanatory")
        );
        assert!(built_in("nope").is_none());
    }

    #[test]
    fn listing_puts_plugin_styles_first_and_does_not_duplicate() {
        let plugin = vec!["project-style".to_string(), "default".to_string()];
        let names = available_names(&plugin);
        assert_eq!(&names[0], "project-style");
        assert_eq!(
            names.iter().filter(|name| *name == "default").count(),
            1,
            "the built-in default shadowed nothing but was listed twice"
        );
        assert!(names.iter().any(|name| name == "explanatory"));
    }
}
