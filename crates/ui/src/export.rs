//! Message export — export selected messages in various formats.

use crate::selector::MessageSelector;
use codape_core::types::Message;
use std::fmt::Write as _;

/// Export format for messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// Plain text format.
    PlainText,
    /// Markdown format.
    Markdown,
    /// JSON format.
    Json,
}

/// Export selected messages as a formatted string.
#[must_use]
pub fn export_messages(
    messages: &[Message],
    selector: &MessageSelector,
    format: ExportFormat,
) -> String {
    let indices = selector.selected_indices();
    if indices.is_empty() {
        // Export all messages if none selected
        let refs: Vec<&Message> = messages.iter().collect();
        format_messages(&refs, format)
    } else {
        // Export only selected messages
        let selected: Vec<&Message> = indices.iter().filter_map(|&i| messages.get(i)).collect();
        format_messages(&selected, format)
    }
}

const fn role_label(role: codape_core::types::Role) -> &'static str {
    // One label table, shared with the plain-text transcript: the Markdown
    // export and `/copy` name a role the same way.
    codape_core::transcript::role_label(role)
}

const fn role_json(role: codape_core::types::Role) -> &'static str {
    use codape_core::types::Role;
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
        _ => "unknown",
    }
}

fn content_text(content: &codape_core::types::MessageContent) -> String {
    // Shared with `/copy`: a tool result, an image and a provider block read
    // the same way in a JSON export as on the clipboard. A reasoning block has
    // no text of its own, which `message_text` says with `None`.
    codape_core::transcript::message_text(content).unwrap_or_default()
}

/// Whether an exported transcript should show this message at all.
const fn is_displayable(msg: &Message) -> bool {
    !matches!(msg.content, codape_core::types::MessageContent::Thinking(_))
}

fn format_messages(messages: &[&Message], format: ExportFormat) -> String {
    match format {
        ExportFormat::PlainText => format_plain(messages),
        ExportFormat::Markdown => format_markdown(messages),
        ExportFormat::Json => format_json(messages),
    }
}

fn format_plain(messages: &[&Message]) -> String {
    // One implementation, shared with `/copy`: `codape-commands` cannot depend
    // on this crate, so the plain-text shape lives in `codape-core` and both
    // callers read it from there.
    codape_core::transcript::plain_text(messages)
}

fn format_markdown(messages: &[&Message]) -> String {
    let mut out = String::new();
    for msg in messages.iter().filter(|m| is_displayable(m)) {
        let _ = write!(out, "\n### {}\n\n", role_label(msg.role));
        match &msg.content {
            codape_core::types::MessageContent::Text(text) => {
                out.push_str(text);
                out.push('\n');
            }
            codape_core::types::MessageContent::ToolCall(tc) => {
                // Serialize rather than interpolate: a tool name or id with a
                // quote or newline must not produce invalid JSON in the fence.
                let payload = serde_json::json!({"tool": tc.name, "id": tc.id});
                let json = serde_json::to_string_pretty(&payload).unwrap_or_default();
                let _ = writeln!(out, "```json\n{json}\n```");
            }
            codape_core::types::MessageContent::ToolResult(tr) => {
                let payload = serde_json::json!({"result": tr.output, "id": tr.tool_call_id});
                let json = serde_json::to_string_pretty(&payload).unwrap_or_default();
                let _ = writeln!(out, "```json\n{json}\n```");
            }
            // Provider state, not conversation.
            codape_core::types::MessageContent::Thinking(_) => {}
            codape_core::types::MessageContent::Image(image) => {
                let _ = writeln!(out, "![image]({})", image.media_type);
            }
            _ => out.push_str("[unknown]\n"),
        }
    }
    out
}

fn format_json(messages: &[&Message]) -> String {
    #[derive(serde::Serialize)]
    struct ExportMessage {
        role: String,
        content: String,
    }

    let export_msgs: Vec<ExportMessage> = messages
        .iter()
        .filter(|m| is_displayable(m))
        .map(|m| ExportMessage {
            role: role_json(m.role).into(),
            content: content_text(&m.content),
        })
        .collect();

    serde_json::to_string_pretty(&export_msgs).unwrap_or_default()
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
        let output = export_messages(&msgs, &selector, ExportFormat::PlainText);
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
        let output = export_messages(&msgs, &selector, ExportFormat::PlainText);
        assert!(output.contains("Hello"));
        assert!(output.contains("Hi there!"));
    }

    #[test]
    fn empty_messages() {
        let msgs: Vec<Message> = Vec::new();
        let selector = MessageSelector::new(0);
        let output = export_messages(&msgs, &selector, ExportFormat::PlainText);
        assert!(output.is_empty());
    }

    #[test]
    fn plain_text_format_labels() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::PlainText);
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
