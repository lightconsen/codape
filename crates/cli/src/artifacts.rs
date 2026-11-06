//! Which output paths a request asks for.
//!
//! A headless run has no person watching for a missing deliverable, and the
//! caller cannot always annotate the request: benchmark and batch harnesses
//! pass the instruction as plain text. This module reads the instruction for
//! the paths it asks to be produced, so the headless loop can check them
//! before accepting a stop.
//!
//! The reading is deliberately conservative. A miss here costs nothing — the
//! caller can always name the path with `--require-artifact`. A false positive
//! costs the model a turn and can send a finished run back to work, so the
//! rules refuse anything ambiguous: a path counts only when an output verb
//! introduces it and no input verb does.

use std::path::PathBuf;

/// Words that introduce an input rather than an output.
const INPUT_WORDS: [&str; 8] = [
    "read", "from", "input", "source", "load", "given", "provided", "using",
];

/// Words that introduce an output.
const OUTPUT_WORDS: [&str; 12] = [
    "create", "write", "save", "output", "produce", "generate", "store", "place", "put", "dump",
    "export", "build",
];

/// Directories whose contents belong to the image, not to the request.
const SYSTEM_PREFIXES: [&str; 8] = [
    "/bin", "/sbin", "/usr", "/etc", "/dev", "/proc", "/sys", "/lib",
];

/// How far before a path an output verb may sit, in characters.
const OUTPUT_LOOKBACK: usize = 160;

/// How close an input verb must be to mark the path as an input, in characters.
const INPUT_WINDOW: usize = 24;

/// Paths the instruction asks to be produced, in the order they appear.
///
/// Each entry is absolute, without surrounding punctuation, and de-duplicated.
#[must_use]
pub fn required_from_instruction(instruction: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for (start, raw) in candidate_tokens(instruction) {
        let token = trim_token(raw);
        if !token.starts_with('/') || token.len() < 2 {
            continue;
        }
        if token.contains('*') || token.contains('?') || token.contains("://") {
            continue;
        }
        if SYSTEM_PREFIXES
            .iter()
            .any(|prefix| token == *prefix || token.starts_with(&format!("{prefix}/")))
        {
            continue;
        }
        let before = &instruction[..start];
        if mentions_near_end(before, &INPUT_WORDS, INPUT_WINDOW)
            && !mentions_near_end(before, &OUTPUT_WORDS, INPUT_WINDOW)
        {
            continue;
        }
        if !mentions_near_end(before, &OUTPUT_WORDS, OUTPUT_LOOKBACK) {
            continue;
        }
        let path = PathBuf::from(token);
        if !found.contains(&path) {
            found.push(path);
        }
    }
    found
}

/// Split the text into (byte start, token) pairs, keeping offsets so the words
/// around a path can be inspected. Commas and semicolons separate tokens too:
/// `a.txt,b.txt` is two paths, not one.
fn candidate_tokens(text: &str) -> Vec<(usize, &str)> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        let separator = ch.is_whitespace() || matches!(ch, ',' | ';');
        match (separator, start) {
            (false, None) => start = Some(index),
            (true, Some(begin)) => {
                tokens.push((begin, &text[begin..index]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        tokens.push((begin, &text[begin..]));
    }
    tokens
}

/// Strip the punctuation a sentence puts around a path.
fn trim_token(token: &str) -> &str {
    token.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '<' | '>' | '.' | ':' | '!' | '?'
        )
    })
}

/// Whether one of `words` appears in the last `window` characters of `text`.
fn mentions_near_end(text: &str, words: &[&str], window: usize) -> bool {
    let tail: String = text
        .chars()
        .rev()
        .take(window)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let lowered = tail.to_lowercase();
    words
        .iter()
        .any(|word| lowered.contains(&format!(" {word}")) || lowered.trim_start().starts_with(word))
}

#[cfg(test)]
mod tests {
    use super::required_from_instruction;

    #[test]
    fn finds_a_named_output_file() {
        let instruction = "Download this video. Then transcribe the text, and create a file \
                           /app/solution.txt that has all the moves, one per line.";
        let paths = required_from_instruction(instruction);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].to_str(), Some("/app/solution.txt"));
    }

    #[test]
    fn finds_the_artifact_of_the_video_transcription_task() {
        // The real benchmark instruction: one long sentence naming the command
        // format before and after the path.
        let instruction = "Download this video of someone playing zork. \
                           https://www.youtube.com/watch?v=ZCbvyPbhRfA. Then transcribe the \
                           entire contents of the text, and create a file /app/solution.txt that \
                           has all the moves they input, one per line, in the format 'n' or \
                           'get bag' etc.";
        let paths = required_from_instruction(instruction);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].to_str(), Some("/app/solution.txt"));
    }

    #[test]
    fn ignores_inputs_read_from_disk() {
        let instruction = "Read /app/data.csv and report the row count.";
        assert!(required_from_instruction(instruction).is_empty());
    }

    #[test]
    fn ignores_system_binaries() {
        let instruction = "Use /usr/bin/ffmpeg and /app/junk to build /app/out/clip.mp4";
        let paths = required_from_instruction(instruction);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].to_str(), Some("/app/out/clip.mp4"));
    }

    #[test]
    fn accepts_an_extensionless_output_named_by_a_verb() {
        let instruction = "Produce a transcript and write it to /app/answer";
        let paths = required_from_instruction(instruction);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].to_str(), Some("/app/answer"));
    }

    #[test]
    fn ignores_a_path_that_is_only_context() {
        let instruction = "The service on /app/config.toml is slow; find out why.";
        assert!(required_from_instruction(instruction).is_empty());
    }

    #[test]
    fn strips_trailing_punctuation_and_deduplicates() {
        let instruction = "Write results to `/app/out.json`, then verify (/app/out.json) parses.";
        let paths = required_from_instruction(instruction);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].to_str(), Some("/app/out.json"));
    }
}
