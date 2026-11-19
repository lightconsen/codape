//! Turning a session's messages into the text a person would copy out of it.
//!
//! This is the one place that decides what a message reads as outside the
//! transcript. `/copy` and the transcript export both go through it, so the
//! two cannot disagree about what an image, a tool result or a reasoning block
//! contributes — and reasoning never contributes anything: handing a model's
//! notes to a recipient as if they were the answer is wrong in both places.
//!
//! It lives in `codape-core` rather than beside the export code in `codape-ui`
//! because the `/copy` handler is in `codape-commands`, which does not depend
//! on `codape-ui`. Putting it here keeps `ui`'s renderer and the command on one
//! implementation without inventing a dependency edge for it.

use crate::types::{Message, MessageContent, Role};
use std::fmt::Write as _;

/// The role label a copied transcript shows for a message.
///
/// Title case, and spelled out rather than abbreviated: the text is pasted
/// somewhere a reader sees it without the colors that carry meaning in the
/// terminal.
#[must_use]
pub const fn role_label(role: Role) -> &'static str {
    // No wildcard: `Role` is `#[non_exhaustive]`, but that only forces a
    // wildcard on other crates. Here an added role is a compile error, which
    // is what we want — a new role must get a label rather than copy as
    // "Unknown" without anyone noticing.
    match role {
        Role::User => "User",
        Role::Assistant => "Assistant",
        Role::System => "System",
        Role::Tool => "Tool",
    }
}

/// What one message contributes to copied text.
///
/// `None` means the message is provider state, not conversation, and is left
/// out entirely. That is the reasoning block: it is what the model thought,
/// not what it said.
#[must_use]
pub fn message_text(content: &MessageContent) -> Option<String> {
    Some(match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::ToolCall(call) => format!("[tool_call: {}]", call.name),
        MessageContent::ToolResult(result) => format!("[tool_result: {}]", result.output),
        // Provider state, not conversation.
        MessageContent::Thinking(_) => return None,
        // The payload would be useless in pasted text; what a reader needs is
        // that a picture was there, and what kind.
        MessageContent::Image(image) => format!("[image: {}]", image.media_type),
        // Part of the assistant turn but not prose. The summary says what
        // happened; the JSON it carries is for the provider, not a reader.
        MessageContent::ServerBlock(server) => format!("[{}]", server.summary()),
    })
}

/// Render messages as the plain-text transcript `/copy` puts on the clipboard.
///
/// The shape is one `--- Role ---` heading and a body per message, which is
/// what `codape_ui::export` already produced for `ExportFormat::PlainText`.
/// Reasoning rows are skipped rather than emitted empty, so a turn that
/// streamed its thinking does not paste as a bare heading.
#[must_use]
pub fn plain_text(messages: &[&Message]) -> String {
    let mut out = String::new();
    for message in messages {
        let Some(body) = message_text(&message.content) else {
            continue;
        };
        let _ = writeln!(out, "--- {} ---", role_label(message.role));
        out.push_str(&body);
        out.push_str("\n\n");
    }
    out
}

/// The text `/copy` should put on the clipboard.
///
/// With no arguments, the whole transcript. With message numbers, those
/// messages — numbered the way `/rewind` numbers them, from 1, so `/copy 3`
/// and `/rewind 3` name the same message. A number past the end is an error
/// rather than a skipped argument: silently copying `1, 2` when `1, 2, 9` was
/// asked for gives a person the wrong text with no way to notice.
///
/// # Errors
///
/// A non-numeric argument, `0`, or a number past the end of the transcript.
pub fn copied_text(transcript: &[Message], args: &[String]) -> Result<String, String> {
    if transcript.is_empty() {
        return Err("The session has no messages to copy yet.".to_string());
    }
    if args.is_empty() {
        let all: Vec<&Message> = transcript.iter().collect();
        return Ok(plain_text(&all));
    }
    let mut picked: Vec<&Message> = Vec::with_capacity(args.len());
    for arg in args {
        let number = arg
            .parse::<usize>()
            .map_err(|_| format!("'{arg}' is not a message number. Usage: /copy [<n>…]"))?;
        if number == 0 {
            return Err(format!(
                "Message numbers start at 1, not 0. The session has {} message(s).",
                transcript.len()
            ));
        }
        // Numbering from 1 is `/rewind`'s numbering: `perform_rewind` keeps the
        // first `n` messages, so message 1 is the opening one.
        let Some(message) = transcript.get(number - 1) else {
            return Err(format!(
                "Message {number} not found. The session has {} message(s).",
                transcript.len()
            ));
        };
        picked.push(message);
    }
    Ok(plain_text(&picked))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ImageBlock, Role, ServerBlock, ThinkingBlock, ToolCall, ToolResult};

    fn message(role: Role, content: MessageContent) -> Message {
        Message { role, content }
    }

    fn thinking() -> MessageContent {
        MessageContent::Thinking(ThinkingBlock {
            thinking: "let me think".into(),
            signature: "sig".into(),
        })
    }

    fn conversation() -> Vec<Message> {
        vec![
            message(Role::User, MessageContent::Text("first question".into())),
            message(Role::Assistant, thinking()),
            message(Role::Assistant, MessageContent::Text("the answer".into())),
        ]
    }

    #[test]
    fn the_whole_transcript_copies_in_order() {
        let transcript = conversation();
        let text = copied_text(&transcript, &[]).expect("copies");
        assert_eq!(
            text,
            "--- User ---\nfirst question\n\n--- Assistant ---\nthe answer\n\n"
        );
    }

    #[test]
    fn reasoning_is_left_out_rather_than_emitted_empty() {
        let transcript = vec![message(Role::Assistant, thinking())];
        // The only message is reasoning, so there is nothing to copy — but the
        // transcript is not empty, so this is an empty string, not an error.
        assert_eq!(copied_text(&transcript, &[]).expect("copies"), "");
    }

    #[test]
    fn numbers_pick_one_message_and_follow_rewinds_numbering() {
        let transcript = conversation();
        let text = copied_text(&transcript, &["1".to_string()]).expect("copies");
        assert_eq!(text, "--- User ---\nfirst question\n\n");
        // Number 3 is the third message in the session even though message 2
        // contributed no text: the numbers address the session, not the copy.
        let text = copied_text(&transcript, &["3".to_string()]).expect("copies");
        assert_eq!(text, "--- Assistant ---\nthe answer\n\n");
    }

    #[test]
    fn several_numbers_copy_in_the_order_asked_for() {
        let transcript = conversation();
        let text = copied_text(&transcript, &["3".to_string(), "1".to_string()]).expect("copies");
        assert_eq!(
            text,
            "--- Assistant ---\nthe answer\n\n--- User ---\nfirst question\n\n"
        );
    }

    #[test]
    fn a_number_past_the_end_is_an_error_naming_the_size() {
        let transcript = conversation();
        let error = copied_text(&transcript, &["9".to_string()]).expect_err("out of range");
        assert!(error.contains("Message 9 not found"), "{error}");
        assert!(error.contains("3 message(s)"), "{error}");
    }

    #[test]
    fn zero_and_non_numbers_are_errors_not_silent_skips() {
        let transcript = conversation();
        let error = copied_text(&transcript, &["0".to_string()]).expect_err("zero");
        assert!(error.contains("start at 1"), "{error}");
        let error = copied_text(&transcript, &["three".to_string()]).expect_err("not a number");
        assert!(error.contains("'three' is not a message number"), "{error}");
    }

    #[test]
    fn an_empty_session_says_so() {
        let error = copied_text(&[], &[]).expect_err("nothing to copy");
        assert!(error.contains("no messages"), "{error}");
    }

    #[test]
    fn a_tool_result_keeps_its_output_and_an_image_keeps_its_kind() {
        let transcript = vec![
            message(
                Role::Tool,
                MessageContent::ToolResult(ToolResult {
                    tool_call_id: "call-1".into(),
                    output: "test failed on line 3".into(),
                    is_error: false,
                    images: Vec::new(),
                }),
            ),
            message(
                Role::User,
                MessageContent::Image(ImageBlock {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                }),
            ),
        ];
        let text = copied_text(&transcript, &[]).expect("copies");
        assert!(text.contains("--- Tool ---\n[tool_result: test failed on line 3]"));
        // The base64 payload must not reach the clipboard.
        assert!(!text.contains("AAAA"));
        assert!(text.contains("[image: image/png]"));
    }

    #[test]
    fn a_tool_call_copies_as_its_name_not_its_arguments() {
        let transcript = vec![message(
            Role::Assistant,
            MessageContent::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "rm -rf /tmp/secret"}),
            }),
        )];
        let text = copied_text(&transcript, &[]).expect("copies");
        assert_eq!(text, "--- Assistant ---\n[tool_call: Bash]\n\n");
        assert!(!text.contains("secret"));
    }

    #[test]
    fn a_server_block_copies_as_its_summary() {
        let transcript = vec![message(
            Role::Assistant,
            MessageContent::ServerBlock(ServerBlock::new(serde_json::json!({
                "type": "server_tool_use",
                "name": "web_search",
            }))),
        )];
        let text = copied_text(&transcript, &[]).expect("copies");
        assert_eq!(text, "--- Assistant ---\n[provider ran web_search]\n\n");
    }
}
