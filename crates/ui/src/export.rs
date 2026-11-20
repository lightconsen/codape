//! Message export — export selected messages in various formats.
//!
//! The rendering itself lives in [`codape_core::transcript`], because `/export`
//! in `codape-commands` has to reach it and this crate is not on its dependency
//! path. What stays here is the piece that is about *selection*: an interactive
//! `MessageSelector` decides which messages by focus, where the command names
//! them by number, and both then render through the same functions. A second
//! renderer here would drift from the command's the way the two clipboard
//! implementations did.

use crate::selector::MessageSelector;
pub use codape_core::transcript::ExportFormat;
use codape_core::types::Message;

/// Export the messages a [`MessageSelector`] has selected.
///
/// An empty selection means "all of them": in the selector an empty selection
/// is the state you start in, not a deliberate request for nothing, and an
/// export of nothing is never what the person meant.
#[must_use]
pub fn export_messages(
    messages: &[Message],
    selector: &MessageSelector,
    format: ExportFormat,
) -> String {
    let indices = selector.selected_indices();
    let selected: Vec<&Message> = if indices.is_empty() {
        messages.iter().collect()
    } else {
        indices.iter().filter_map(|&i| messages.get(i)).collect()
    };
    codape_core::transcript::export(&selected, format)
}

/// Copy text to the system clipboard.
///
/// The platform helpers live in `codape_core::clipboard`, shared with `/copy`.
/// This used to be a second implementation that knew only macOS and Linux, so
/// `ctrl+shift+c` would have refused to work on Windows while the command did.
///
/// # Errors
///
/// What the helper reported, by name.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    codape_core::clipboard::copy_to_clipboard(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::MessageSelector;

    fn test_messages() -> Vec<Message> {
        vec![Message::user("Hello"), Message::assistant("Hi there!")]
    }

    #[test]
    fn export_all_as_plain_text() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::Text);
        assert!(output.contains("User"));
        assert!(output.contains("Hello"));
        assert!(output.contains("Assistant"));
        assert!(output.contains("Hi there!"));
    }

    #[test]
    fn export_selected_as_markdown() {
        let msgs = test_messages();
        let mut selector = MessageSelector::new(msgs.len());
        // Navigate to first message and select it
        while selector.focus_index() > 0 {
            selector.focus_up();
        }
        selector.toggle_focus(); // Select first message only

        let output = export_messages(&msgs, &selector, ExportFormat::Markdown);
        assert!(output.contains("### User"));
        assert!(output.contains("Hello"));
        // Second message should not be present since only first is selected
        assert!(!output.contains("Hi there!"));
    }

    #[test]
    fn export_as_json() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::Json);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["role"], "user");
        assert_eq!(parsed[0]["content"], "Hello");
    }

    #[test]
    fn empty_selection_exports_all() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::Text);
        assert!(output.contains("Hello"));
        assert!(output.contains("Hi there!"));
    }

    #[test]
    fn empty_messages() {
        let msgs: Vec<Message> = Vec::new();
        let selector = MessageSelector::new(0);
        let output = export_messages(&msgs, &selector, ExportFormat::Text);
        assert!(output.is_empty());
    }

    #[test]
    fn plain_text_format_labels() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::Text);
        assert!(output.contains("--- User ---"));
        assert!(output.contains("--- Assistant ---"));
    }

    #[test]
    fn markdown_format_headings() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::Markdown);
        assert!(output.contains("### User"));
        assert!(output.contains("### Assistant"));
    }

    #[test]
    fn json_is_valid() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::Json);
        // Should not panic
        let _: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
    }

    /// Tool output is embedded in a Markdown JSON fence; quotes, newlines and
    /// backslashes must be escaped so the fence stays parseable. Ported from
    /// kimi-code's export-markdown contract cases.
    #[test]
    fn markdown_tool_fences_are_valid_json() {
        use codape_core::types::{MessageContent, Role, ToolCall, ToolResult};

        let output = "first\nsecond \"quoted\" \\ backslash";
        let msgs = vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::ToolCall(ToolCall {
                    id: "call\"1".into(),
                    name: "Write".into(),
                    input: serde_json::json!({}),
                }),
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    tool_call_id: "call\"1".into(),
                    output: output.into(),
                    is_error: false,
                    images: Vec::new(),
                }),
            },
        ];
        let selector = MessageSelector::new(msgs.len());
        let markdown = export_messages(&msgs, &selector, ExportFormat::Markdown);

        let mut fences = markdown
            .split("```json")
            .skip(1)
            .map(|rest| rest.split("```").next().expect("closing fence"));
        let call: serde_json::Value = serde_json::from_str(fences.next().expect("call fence"))
            .expect("call fence is valid JSON");
        assert_eq!(call["tool"], "Write");
        assert_eq!(call["id"], "call\"1");

        let result: serde_json::Value = serde_json::from_str(fences.next().expect("result fence"))
            .expect("result fence is valid JSON");
        assert_eq!(result["result"], output);
        assert_eq!(result["id"], "call\"1");
    }

    #[test]
    fn json_export_preserves_quotes_newlines_and_unicode() {
        let msgs = vec![Message::assistant("quote \" line\n汉字")];
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::Json);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed[0]["content"], "quote \" line\n汉字");
    }
}
