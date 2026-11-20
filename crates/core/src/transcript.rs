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

// ─── Export ─────────────────────────────────────────────────────────────────

/// A format `/export` can write a transcript in.
///
/// The three live here rather than in `codape_ui::export` for the same reason
/// [`plain_text`] does: the handler is in `codape-commands`, which cannot see
/// `codape-ui`, and a second renderer there would drift from the first. The
/// variants are named by what a person would do with the file — paste it into
/// a document, read it in a diff, or feed it to a program — not by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// `--- Role ---` headings: the same shape `/copy` puts on the clipboard.
    Text,
    /// One `### Role` heading per message, with tool traffic fenced as JSON.
    Markdown,
    /// An array of `{role, content}` objects.
    Json,
}

impl ExportFormat {
    /// Every format `/export` accepts, in the order its usage line lists them.
    pub const ALL: [Self; 3] = [Self::Text, Self::Markdown, Self::Json];

    /// The word a person types for this format.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Markdown => "markdown",
            Self::Json => "json",
        }
    }

    /// The extension a written file gets, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Text => "txt",
            Self::Markdown => "md",
            Self::Json => "json",
        }
    }

    /// Parse the word a person types.
    ///
    /// Case-insensitive, and `md` is accepted for `markdown` because it is the
    /// extension a person already has in their fingers.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "text" | "txt" => Some(Self::Text),
            "markdown" | "md" => Some(Self::Markdown),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    /// The formats this string could have meant, for an error that helps.
    #[must_use]
    pub fn names() -> String {
        Self::ALL.map(Self::name).join(", ")
    }
}

/// Render messages in one export format.
///
/// Reasoning rows are dropped by every format, including JSON: they are
/// provider state, and a file handed to a reader that carried the model's
/// private notes as if they were its answer would be wrong in the same way a
/// copied transcript would be.
#[must_use]
pub fn export(messages: &[&Message], format: ExportFormat) -> String {
    match format {
        ExportFormat::Text => plain_text(messages),
        ExportFormat::Markdown => markdown(messages),
        ExportFormat::Json => json(messages),
    }
}

fn markdown(messages: &[&Message]) -> String {
    let mut out = String::new();
    for message in messages {
        // A heading with nothing under it is what a reasoning row would
        // produce, so the filter happens before the heading, not after.
        if matches!(message.content, MessageContent::Thinking(_)) {
            continue;
        }
        let _ = writeln!(out, "\n### {}\n", role_label(message.role));
        match &message.content {
            MessageContent::Text(text) => {
                out.push_str(text);
                out.push('\n');
            }
            // Serialize rather than interpolate: a tool name or id holding a
            // quote or a newline must not break the fence it sits in.
            MessageContent::ToolCall(call) => {
                let payload = serde_json::json!({"tool": call.name, "id": call.id});
                let rendered = serde_json::to_string_pretty(&payload).unwrap_or_default();
                let _ = writeln!(out, "```json\n{rendered}\n```");
            }
            MessageContent::ToolResult(result) => {
                let payload =
                    serde_json::json!({"result": result.output, "id": result.tool_call_id});
                let rendered = serde_json::to_string_pretty(&payload).unwrap_or_default();
                let _ = writeln!(out, "```json\n{rendered}\n```");
            }
            MessageContent::Image(image) => {
                let _ = writeln!(out, "![image]({})", image.media_type);
            }
            MessageContent::Thinking(_) => {}
            MessageContent::ServerBlock(server) => {
                let _ = writeln!(out, "{}", server.summary());
            }
        }
    }
    out
}

fn json(messages: &[&Message]) -> String {
    #[derive(serde::Serialize)]
    struct Exported<'a> {
        role: &'a str,
        content: String,
    }

    let rows: Vec<Exported<'_>> = messages
        .iter()
        .filter_map(|message| {
            let content = message_text(&message.content)?;
            Some(Exported {
                role: role_name(message.role),
                content,
            })
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_default()
}

/// The lowercase role name a JSON export carries.
///
/// Separate from [`role_label`], which is prose for a reader: `"User"` in a
/// heading and `"user"` in a field are both right for their own consumer, and
/// one string cannot be.
#[must_use]
pub const fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}

/// The messages `/copy` and `/export` should act on.
///
/// With no arguments, all of them. With message numbers, those messages —
/// numbered the way `/rewind` numbers them, from 1, so `/copy 3` and
/// `/rewind 3` name the same message. A number past the end is an error rather
/// than a skipped argument: silently taking `1, 2` when `1, 2, 9` was asked for
/// gives a person the wrong text with no way to notice.
///
/// Shared by both commands rather than written twice, because the two would
/// otherwise disagree about what message 3 means.
///
/// # Errors
///
/// An empty transcript, a non-numeric argument, `0`, or a number past the end.
pub fn selected_messages<'a>(
    transcript: &'a [Message],
    args: &[String],
) -> Result<Vec<&'a Message>, String> {
    if transcript.is_empty() {
        return Err("The session has no messages yet.".to_string());
    }
    if args.is_empty() {
        return Ok(transcript.iter().collect());
    }
    let mut picked: Vec<&Message> = Vec::with_capacity(args.len());
    for arg in args {
        let number = arg.parse::<usize>().map_err(|_| {
            format!("'{arg}' is not a message number. Usage: <n>… from 1, or none for all.")
        })?;
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
    Ok(picked)
}

/// The text `/copy` should put on the clipboard.
///
/// # Errors
///
/// An empty transcript — there is no text to copy when nothing has been said —
/// plus whatever [`selected_messages`] refuses about the numbers.
pub fn copied_text(transcript: &[Message], args: &[String]) -> Result<String, String> {
    if transcript.is_empty() {
        return Err("The session has no messages to copy yet.".to_string());
    }
    Ok(plain_text(&selected_messages(transcript, args)?))
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

    // ── /export ──────────────────────────────────────────────────────────────

    #[test]
    fn every_format_is_reachable_by_the_word_its_usage_line_prints() {
        // The name a usage line shows and the word `parse` accepts are written
        // separately, so this pins them against each other: a format whose
        // printed name is not a name you can type would be a usage line that
        // teaches something the command refuses.
        for format in ExportFormat::ALL {
            assert_eq!(ExportFormat::parse(format.name()), Some(format));
            assert!(
                ExportFormat::names().contains(format.name()),
                "{} is missing from the usage line",
                format.name()
            );
        }
        assert_eq!(ExportFormat::parse("md"), Some(ExportFormat::Markdown));
        assert_eq!(ExportFormat::parse("TEXT"), Some(ExportFormat::Text));
        assert_eq!(ExportFormat::parse("yaml"), None);
    }

    #[test]
    fn every_format_leaves_reasoning_and_its_heading_out() {
        let transcript = conversation();
        let all: Vec<&Message> = transcript.iter().collect();
        // A reasoning row must not become a bare heading in Markdown, nor a
        // row with an empty `content` in JSON, nor an empty `--- ---` in text.
        for format in ExportFormat::ALL {
            let out = export(&all, format);
            assert!(!out.contains("let me think"), "{format:?} leaked reasoning");
        }
        let markdown = export(&all, ExportFormat::Markdown);
        assert_eq!(markdown.matches("### ").count(), 2, "{markdown}");
    }

    #[test]
    fn the_json_export_is_an_array_of_role_and_content() {
        let transcript = conversation();
        let all: Vec<&Message> = transcript.iter().collect();
        let out = export(&all, ExportFormat::Json);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["role"], "user");
        assert_eq!(parsed[0]["content"], "first question");
        assert_eq!(parsed[1]["role"], "assistant");
    }

    #[test]
    fn a_selection_is_shared_with_copy_and_counted_from_one() {
        let transcript = conversation();
        // Message 3 is the answer; the same number must mean the same message
        // in `/copy` and `/export`, or a person who learned one would get the
        // wrong text from the other.
        let picked = selected_messages(&transcript, &["3".into()]).expect("selects");
        assert_eq!(picked.len(), 1);
        assert!(matches!(&picked[0].content, MessageContent::Text(t) if t == "the answer"));
        assert_eq!(
            copied_text(&transcript, &["3".into()]).expect("copies"),
            "--- Assistant ---\nthe answer\n\n"
        );
    }

    #[test]
    fn a_selection_says_what_is_wrong_rather_than_skipping_it() {
        let transcript = conversation();
        assert!(
            selected_messages(&transcript, &["0".into()])
                .unwrap_err()
                .contains("start at 1")
        );
        assert!(
            selected_messages(&transcript, &["9".into()])
                .unwrap_err()
                .contains("not found")
        );
        assert!(
            selected_messages(&transcript, &["x".into()])
                .unwrap_err()
                .contains("not a message number")
        );
        // No argument means all of them, which is how `/export` with a bare
        // `| /next` behaves; an empty transcript is the only "nothing" case.
        assert_eq!(selected_messages(&transcript, &[]).expect("all").len(), 3);
        assert!(selected_messages(&[], &[]).is_err());
    }
}
