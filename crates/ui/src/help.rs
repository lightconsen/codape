//! The help page.
//!
//! `/keybindings` already printed the bound keys, and it is the right place to
//! diagnose a `keybindings.json` that failed to parse. It is the wrong place
//! to *learn* the keys: it is a wall of text appended to the transcript, and
//! reaching it means remembering a command whose help you were looking for.
//!
//! The page is generated from the keybinding table rather than maintained
//! beside it, so a rebound key is never described by a stale paragraph. What
//! it adds over the table is grouping: a person looking for "how do I switch
//! models" does not know that lives under `chat:modelPicker`.

use crate::keybindings::{Action, Context, KeyBindings};

/// One line of the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpEntry {
    /// The section it belongs to, as drawn.
    pub section: &'static str,
    /// The action's name, from the binding table.
    pub action: &'static str,
    /// The keys bound to it, comma separated.
    pub keys: String,
}

/// The sections, in the order a newcomer needs them.
///
/// Ordered by how often a key is reached for rather than alphabetically: the
/// first section is what a session is mostly made of.
const SECTIONS: &[(&str, &[Action])] = &[
    (
        "Writing a prompt",
        &[
            Action::Cancel,
            Action::CyclePermissionMode,
            Action::ImagePaste,
            Action::AttachmentBrowser,
            Action::VoiceInput,
            Action::ExternalEditor,
            Action::StashInput,
            Action::SendNow,
            Action::HistorySearch,
            Action::HistorySearchPrevious,
            Action::HistorySearchNext,
            Action::HistorySearchAccept,
            Action::HistorySearchCancel,
            Action::AutocompleteAccept,
            Action::AutocompleteDismiss,
            Action::FileMentionAccept,
            Action::AutocompletePrevious,
            Action::AutocompleteNext,
        ],
    ),
    (
        "Choosing and inspecting",
        &[
            Action::ModelPicker,
            Action::EffortSlider,
            Action::AgentsView,
            Action::Rewind,
            Action::ThemePicker,
            Action::PluginBrowser,
            Action::SessionPicker,
            Action::SettingsMenu,
            Action::Help,
        ],
    ),
    (
        "Reading the transcript",
        &[
            Action::ToggleToolOutput,
            Action::ToggleThinking,
            Action::ToggleSplitPane,
            Action::DiffPreviousFile,
            Action::DiffNextFile,
            Action::PanePreviousTab,
            Action::PaneNextTab,
            Action::TranscriptFullscreen,
            Action::CopyTranscript,
        ],
    ),
    (
        "A list is open",
        &[Action::ListPrevious, Action::ListNext, Action::ListAccept],
    ),
    (
        "A dialog is open",
        &[
            Action::ConfirmYes,
            Action::ConfirmNo,
            Action::PermissionAllow,
            Action::PermissionAllowSession,
            Action::PermissionDeny,
        ],
    ),
];

/// A human sentence for each action.
///
/// Kept here rather than derived from the variant name: `app:toggleToolOutput`
/// is a good key and a bad sentence, and the point of the page is that a
/// person reads it once.
#[must_use]
pub const fn describe(action: Action) -> &'static str {
    match action {
        Action::ToggleToolOutput => "Fold or expand tool calls and results",
        Action::ToggleThinking => "Fold or expand the model's reasoning",
        Action::ModelPicker => "Change model and reasoning effort",
        Action::EffortSlider => "Change only the reasoning effort",
        Action::ToggleSplitPane => "Switch between one pane and a split view",
        Action::CyclePermissionMode => "Cycle permission mode (default → edits → plan)",
        Action::ImagePaste => "Attach the image on the clipboard",
        Action::AttachmentBrowser => "See, show or remove the images attached to the next prompt",
        Action::VoiceInput => "Transcribe from the microphone onto the prompt",
        Action::ExternalEditor => "Write the prompt in $EDITOR and read it back",
        Action::StashInput => "Set the line aside and take it back",
        Action::SendNow => "Stop the running turn and send this line now",
        Action::Cancel => "Stop the turn, clear the line, or close a dialog",
        Action::HistorySearch => "Search previous prompts",
        Action::ConfirmYes => "Run the destructive command",
        Action::ConfirmNo => "Cancel it",
        Action::PermissionAllow => "Allow this one call",
        Action::PermissionAllowSession => "Allow this tool for the session",
        Action::PermissionDeny => "Deny the call",
        Action::AutocompletePrevious => "Previous suggestion",
        Action::AutocompleteNext => "Next suggestion",
        Action::AutocompleteAccept => "Insert the suggestion",
        Action::AutocompleteDismiss => "Close the suggestion list",
        Action::FileMentionAccept => "Insert the path",
        Action::HistorySearchPrevious => "Previous match",
        Action::HistorySearchNext => "Next match",
        Action::HistorySearchAccept => "Put the match on the line",
        Action::HistorySearchCancel => "Close history search",
        Action::AgentsView => "Open the agent roster",
        Action::Help => "This page",
        Action::Rewind => "Rewind the conversation, and the files it changed",
        Action::ThemePicker => "Choose a colour theme",
        Action::PluginBrowser => "Installed plugins and marketplaces",
        Action::SessionPicker => "Resume an earlier session of this workspace",
        Action::SettingsMenu => "Change a setting from a menu",
        Action::DiffPreviousFile => "Jump to the previous file in the diff",
        Action::DiffNextFile => "Jump to the next file in the diff",
        Action::PanePreviousTab => "Show the pane's previous view",
        Action::PaneNextTab => "Show the pane's next view",
        Action::TranscriptFullscreen => "Give the transcript the whole screen",
        Action::ListPrevious => "Previous row",
        Action::ListNext => "Next row",
        Action::ListAccept => "Choose the highlighted row",
        Action::CopyTranscript => "Copy the transcript as shown to the clipboard",
    }
}

/// Every line of the page, in order.
#[must_use]
pub fn entries(keybindings: &KeyBindings) -> Vec<HelpEntry> {
    let mut out = Vec::new();
    for (section, actions) in SECTIONS {
        for action in *actions {
            // `chords`, not `strokes`: a chord is one binding a person can
            // press, and printing only the single-keystroke half would leave
            // the page claiming a key does something it no longer does on its
            // own.
            let chords: Vec<String> = keybindings
                .chords(*action)
                .iter()
                .map(crate::keybindings::Chord::display)
                .collect();
            out.push(HelpEntry {
                section,
                action: action.as_str(),
                // An unbound action is dropped rather than shown as
                // "(unbound)": the page answers "what can I press", and a row
                // with no key does not.
                keys: chords.join(", "),
            });
        }
    }
    out.retain(|entry| !entry.keys.is_empty());
    out
}

/// Render the page.
///
/// `sources` is the list of files that were applied, in order, so the derived
/// page and the `/keybindings` diagnostic cannot disagree about which file a
/// key came from.
#[must_use]
pub fn render(keybindings: &KeyBindings, sources: &[String]) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("Keybindings\n");
    if sources.is_empty() {
        out.push_str("Using built-in defaults. Put a keybindings.json in the config home or\n");
        out.push_str(".codape/keybindings.json in the project to change one.\n");
    } else {
        out.push_str("From:\n");
        for source in sources {
            let _ = writeln!(out, "  {source}");
        }
    }

    let entries = entries(keybindings);
    let mut current = "";
    for entry in &entries {
        if entry.section != current {
            current = entry.section;
            let _ = write!(out, "\n{current}\n");
        }
        let Some(summary) = Action::parse(entry.action).map(describe) else {
            continue;
        };
        let _ = writeln!(out, "  {:<16} {summary}", entry.keys);
    }

    out.push_str("\nThe slash commands (/help) are separate from these keys.\n");
    out
}

/// Whether a context has any binding worth advertising.
///
/// Used by tests to keep `SECTIONS` honest: an action that moved to a
/// context the page never mentions would be invisible here.
#[must_use]
pub fn contexts_covered() -> Vec<Context> {
    let mut contexts: Vec<Context> = ALL_ACTIONS_CONTEXT
        .iter()
        .filter(|action| SECTIONS.iter().any(|(_, actions)| actions.contains(action)))
        .map(|action| action.context())
        .collect();
    contexts.sort_unstable();
    contexts.dedup();
    contexts
}

/// Every action, so the coverage check can see one the page forgot.
const ALL_ACTIONS_CONTEXT: &[Action] = &[
    Action::ToggleToolOutput,
    Action::ToggleThinking,
    Action::ModelPicker,
    Action::EffortSlider,
    Action::ToggleSplitPane,
    Action::CyclePermissionMode,
    Action::ImagePaste,
    Action::AttachmentBrowser,
    Action::VoiceInput,
    Action::Cancel,
    Action::HistorySearch,
    Action::ConfirmYes,
    Action::ConfirmNo,
    Action::PermissionAllow,
    Action::PermissionAllowSession,
    Action::PermissionDeny,
    Action::AutocompletePrevious,
    Action::AutocompleteNext,
    Action::AutocompleteAccept,
    Action::AutocompleteDismiss,
    Action::FileMentionAccept,
    Action::HistorySearchPrevious,
    Action::HistorySearchNext,
    Action::HistorySearchAccept,
    Action::HistorySearchCancel,
    Action::AgentsView,
    Action::Help,
    Action::Rewind,
    Action::ThemePicker,
    Action::PluginBrowser,
    Action::SessionPicker,
    Action::DiffPreviousFile,
    Action::DiffNextFile,
    Action::PanePreviousTab,
    Action::PaneNextTab,
    Action::TranscriptFullscreen,
    Action::ListPrevious,
    Action::ListNext,
    Action::ListAccept,
    Action::ExternalEditor,
    Action::StashInput,
    Action::SendNow,
    Action::SettingsMenu,
    Action::CopyTranscript,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_lists_the_new_surfaces_with_their_keys() {
        let rendered = render(&KeyBindings::default(), &[]);
        assert!(rendered.contains("Open the agent roster"), "{rendered}");
        assert!(rendered.contains("Choose a colour theme"), "{rendered}");
        assert!(rendered.contains("Rewind the conversation"), "{rendered}");
        assert!(rendered.contains("This page"), "{rendered}");
        assert!(rendered.contains("Resume an earlier session"), "{rendered}");
        // The default keystroke must appear beside it, not just the sentence.
        assert!(rendered.contains("ctrl+g"), "{rendered}");
        assert!(rendered.contains("alt+s"), "{rendered}");
    }

    #[test]
    fn every_action_is_described_and_some_section_covers_it() {
        for action in ALL_ACTIONS_CONTEXT {
            assert!(!describe(*action).is_empty(), "{action:?} has no sentence");
            assert!(
                SECTIONS.iter().any(|(_, actions)| actions.contains(action)),
                "{action:?} is in no section, so the page cannot show it"
            );
        }
        assert_eq!(
            ALL_ACTIONS_CONTEXT.len(),
            Action::all().len(),
            "the page's action list has drifted from the keybinding table"
        );
    }

    #[test]
    fn an_unbound_action_is_omitted_rather_than_shown_empty() {
        // `Help` has a default, so binding nothing to `AgentsView` should drop
        // its row instead of printing a blank key column impossible to press.
        let mut table = KeyBindings::default();
        let chord = table.chords(Action::AgentsView)[0].clone();
        table.unbind(Context::Chat, chord);
        let rendered = render(&table, &[]);
        assert!(!rendered.contains("Open the agent roster"), "{rendered}");
        assert!(rendered.contains("This page"), "others survive: {rendered}");
    }

    #[test]
    fn the_page_names_where_the_bindings_came_from() {
        let rendered = render(
            &KeyBindings::default(),
            &["/home/x/.codape/keybindings.json".to_string()],
        );
        assert!(rendered.contains("From:"), "{rendered}");
        assert!(
            rendered.contains("/home/x/.codape/keybindings.json"),
            "{rendered}"
        );

        let defaults = render(&KeyBindings::default(), &[]);
        assert!(defaults.contains("built-in defaults"), "{defaults}");
    }

    #[test]
    fn the_page_covers_the_contexts_it_claims_to() {
        let contexts = contexts_covered();
        assert!(contexts.contains(&Context::List), "{contexts:?}");
        assert!(contexts.contains(&Context::Chat), "{contexts:?}");
    }
}
