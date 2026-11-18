//! The rows behind each modal list.
//!
//! [`codape_ui::picker::Picker`] owns the interaction — filtering, the
//! highlight, accept-or-refuse. This module owns what each list is *made of*,
//! which is the part that needs the session: the agent roster, the rewind
//! targets, the themes, the plugins, the marketplaces.
//!
//! Split out of `repl.rs` for two reasons. The key handler there is already
//! over the lint's line limit, and a list-building function is not a key
//! branch. And the rows are worth testing without a terminal: every function
//! here is a pure mapping from session facts to [`PickerItem`] rows, except
//! the two that take a lock.

use codape_ui::picker::{Picker, PickerItem};

/// What an accepted row of the agent list means.
///
/// The picker's `key` is a string, so the interpretation lives here: prefix a
/// row's key so the session can tell "focus this agent" from "open its log"
/// without a parallel enum threaded through the UI crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentChoice {
    /// Point the observable subscription at this agent.
    Follow(String),
    /// Show this agent's NDJSON event log.
    Logs(String),
}

impl AgentChoice {
    /// The key [`PickerItem`] carries for this choice.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Follow(id) => format!("follow:{id}"),
            Self::Logs(id) => format!("logs:{id}"),
        }
    }

    /// Read a key back, or say why it is not one we wrote.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        let (kind, id) = key.split_once(':')?;
        if id.is_empty() {
            return None;
        }
        match kind {
            "follow" => Some(Self::Follow(id.to_string())),
            "logs" => Some(Self::Logs(id.to_string())),
            _ => None,
        }
    }
}

/// One agent as the roster needs it.
///
/// A plain struct rather than `&AgentInstance` so the row builder can be
/// tested without a live manager holding a lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    /// The task id — what `/agents logs` and `/agents follow` take.
    pub id: String,
    /// The agent type, as `Debug` spells it.
    pub kind: String,
    /// Lowercased status, as the `/agents list` text path prints it.
    pub status: String,
    /// The first line of the agent's prompt.
    pub prompt: String,
    /// Whether the agent has a transcript to continue.
    ///
    /// Drives whether the "follow" row is offered: following an agent whose
    /// conversation is over subscribes to a channel nobody will write to, and
    /// the row would look live while doing nothing.
    pub resumable: bool,
}

/// Build the agent roster.
///
/// Two rows per agent — follow and logs — because those are the two things
/// `/agents` can do with one, and a list that only did one of them would leave
/// the other reachable only by typing the id from memory.
#[must_use]
pub fn agent_rows(agents: &[AgentRow]) -> Picker {
    if agents.is_empty() {
        return Picker::new(
            "Agents",
            vec![
                PickerItem::new("", "No agents have been spawned in this session")
                    .disabled("spawn one with the Agent tool"),
            ],
        );
    }

    let mut items = Vec::with_capacity(agents.len() * 2);
    for agent in agents {
        items.push(
            PickerItem::new(
                AgentChoice::Follow(agent.id.clone()).key(),
                format!("{id} · {kind} · follow", id = agent.id, kind = agent.kind),
            )
            .with_detail(agent.status.clone()),
        );

        let logs = PickerItem::new(
            AgentChoice::Logs(agent.id.clone()).key(),
            format!("{id} · logs", id = agent.id),
        )
        .with_detail(if agent.resumable {
            "transcript available".to_string()
        } else {
            agent.prompt.clone()
        });
        items.push(logs);
    }
    Picker::new("Agents", items)
}

/// One rewritable point in the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindRow {
    /// One-based message number, the same one `/rewind <n>` takes.
    pub number: usize,
    /// The role, for the row's left half.
    pub role: String,
    /// The message's first line, truncated for the row.
    pub preview: String,
}

/// Build the rewind list.
///
/// The same one-based numbering `/rewind <n>` prints, so a person who learned
/// the number from the command finds the same row here. Newest first: the
/// reason to rewind is usually recent.
#[must_use]
pub fn rewind_rows(rows: &[RewindRow]) -> Picker {
    if rows.is_empty() {
        return Picker::new(
            "Rewind",
            vec![
                PickerItem::new("", "Nothing to rewind to").disabled("the session has no messages"),
            ],
        );
    }
    let items = rows
        .iter()
        .rev()
        .map(|row| {
            PickerItem::new(
                row.number.to_string(),
                format!(
                    "{n} · {role} · {preview}",
                    n = row.number,
                    role = row.role,
                    preview = row.preview
                ),
            )
        })
        .collect();
    Picker::new("Rewind to message", items)
}

/// Build the theme list.
///
/// `current` is annotated rather than omitted, so the list always says what the
/// session is on.
#[must_use]
pub fn theme_rows(names: &[String], current: &str) -> Picker {
    if names.is_empty() {
        return Picker::new(
            "Theme",
            vec![PickerItem::new("", "No themes available").disabled("")],
        );
    }
    let items = names
        .iter()
        .map(|name| {
            let detail = if name == current { "current" } else { "" };
            PickerItem::new(name.clone(), name.clone()).with_detail(detail)
        })
        .collect();
    Picker::new("Theme", items)
}

/// One installed plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRow {
    /// The plugin's name.
    pub name: String,
    /// What the browser says about it: version, state and where it was loaded
    /// from, when the session knows them.
    pub detail: String,
}

/// Build the plugin browser.
///
/// Lists what is installed and the marketplaces that are configured, with the
/// URL of each marketplace in its detail line. It does not install anything:
/// installing fetches and runs code from a remote, which is a decision that
/// wants its arguments typed out and confirmed, not a keystroke on a
/// highlighted row. The empty row says which command does it.
#[must_use]
pub fn plugin_rows(plugins: &[PluginRow], marketplaces: &[(String, String)]) -> Picker {
    let mut items: Vec<PickerItem> = plugins
        .iter()
        .map(|plugin| {
            PickerItem::new(format!("plugin:{}", plugin.name), plugin.name.clone()).with_detail(
                if plugin.detail.is_empty() {
                    "installed".to_string()
                } else {
                    plugin.detail.clone()
                },
            )
        })
        .collect();
    for (name, url) in marketplaces {
        items.push(
            PickerItem::new(format!("marketplace:{name}"), name.clone())
                .with_detail(format!("marketplace · {url}")),
        );
    }
    if items.is_empty() {
        items.push(
            PickerItem::new("", "No plugins installed")
                .disabled("add one with /plugin install <source>"),
        );
    }
    Picker::new("Plugins", items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, status: &str) -> AgentRow {
        AgentRow {
            id: id.to_string(),
            kind: "Explore".into(),
            status: status.into(),
            prompt: "find the auth flow".into(),
            resumable: true,
        }
    }

    #[test]
    fn the_roster_offers_follow_and_logs_per_agent() {
        let picker = agent_rows(&[agent("a1", "running"), agent("a2", "completed")]);
        assert_eq!(picker.items.len(), 4, "two rows each");
        let keys: Vec<&str> = picker.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["follow:a1", "logs:a1", "follow:a2", "logs:a2"]);
        assert_eq!(picker.accept(), Some("follow:a1"));
    }

    #[test]
    fn a_row_key_round_trips_through_the_choice() {
        let choice = AgentChoice::Follow("a1".into());
        assert_eq!(AgentChoice::parse(&choice.key()), Some(choice));
        let choice = AgentChoice::Logs("weird:id".into());
        assert_eq!(
            AgentChoice::parse(&choice.key()),
            Some(AgentChoice::Logs("weird:id".into())),
            "an id containing a colon survives, because only the first is split"
        );
        assert_eq!(AgentChoice::parse("nonsense"), None);
        assert_eq!(
            AgentChoice::parse("follow:"),
            None,
            "an empty id is not a row"
        );
    }

    #[test]
    fn an_empty_roster_says_how_to_fill_it() {
        let picker = agent_rows(&[]);
        assert_eq!(picker.accept(), None, "nothing to choose");
        assert!(
            picker.refusal().is_some_and(|r| r.contains("unavailable")),
            "{:?}",
            picker.refusal()
        );
    }

    #[test]
    fn the_rewind_list_is_newest_first_and_keeps_command_numbering() {
        let rows = vec![
            RewindRow {
                number: 1,
                role: "User".into(),
                preview: "first".into(),
            },
            RewindRow {
                number: 2,
                role: "Assistant".into(),
                preview: "second".into(),
            },
        ];
        let picker = rewind_rows(&rows);
        assert_eq!(picker.accept(), Some("2"), "newest first");
        assert!(picker.items[0].label.contains("Assistant"));
    }

    #[test]
    fn an_empty_rewind_list_is_disabled_rather_than_absent() {
        let picker = rewind_rows(&[]);
        assert_eq!(picker.accept(), None);
        assert_eq!(picker.items.len(), 1);
    }

    #[test]
    fn the_theme_list_marks_the_current_one() {
        let picker = theme_rows(&["dark".to_string(), "light".to_string()], "light");
        let light = picker
            .items
            .iter()
            .find(|i| i.key == "light")
            .expect("light");
        assert_eq!(light.detail, "current");
        let dark = picker.items.iter().find(|i| i.key == "dark").expect("dark");
        assert!(dark.detail.is_empty());
    }

    #[test]
    fn the_plugin_list_covers_plugins_and_marketplaces() {
        let picker = plugin_rows(
            &[PluginRow {
                name: "fmt".into(),
                detail: "1.2.3 · active".into(),
            }],
            &[("acme".to_string(), "https://example.com/m.json".to_string())],
        );
        assert_eq!(picker.items.len(), 2);
        assert!(picker.items.iter().any(|i| i.key == "plugin:fmt"));
        assert!(picker.items.iter().any(|i| i.key == "marketplace:acme"));
        let marketplace = picker
            .items
            .iter()
            .find(|i| i.key == "marketplace:acme")
            .expect("marketplace row");
        assert!(
            marketplace.detail.contains("https://example.com/m.json"),
            "the row must name the URL it resolves against: {:?}",
            marketplace.detail
        );
    }

    #[test]
    fn an_empty_plugin_list_names_the_install_command() {
        let picker = plugin_rows(&[], &[]);
        assert_eq!(picker.accept(), None);
        assert!(
            picker
                .refusal()
                .is_some_and(|r| r.contains("/plugin install")),
            "{:?}",
            picker.refusal()
        );
    }
}
