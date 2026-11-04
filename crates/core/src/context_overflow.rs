//! Recognising a provider's "prompt too long" error.
//!
//! Every provider enforces the context window on its side, and each says so
//! with its own wording. A client that only counts tokens itself has to guess
//! the window; a client that reads these messages learns the real one. The
//! guess is wrong in both directions — too high means the request is rejected
//! with no way to react, too low means compaction runs while there is still
//! room — so the error is the only authoritative source available.
//!
//! Detection is message-shaped rather than status-shaped: the same 400 also
//! carries malformed requests, and only the text says which this is.

/// A provider-reported context overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextOverflow {
    /// The input size the provider actually measured, when it said so.
    pub actual: Option<usize>,
    /// The limit the request exceeded.
    pub limit: usize,
}

impl ContextOverflow {
    /// A window learned from the provider, honouring the reported input size
    /// when it is larger than the stated limit (providers round, and the
    /// difference is the real headroom).
    #[must_use]
    pub fn learned_window(&self) -> usize {
        self.actual
            .map_or(self.limit, |actual| actual.max(self.limit))
    }
}

/// Phrases that mean "the input does not fit", independent of any numbers.
const OVERFLOW_MARKERS: &[&str] = &[
    "prompt is too long",
    "input is too long",
    "input length and",
    "context_length_exceeded",
    "maximum context length",
    "exceeds the maximum context",
    "exceeds the model's maximum context",
    "context window exceeded",
    "too many tokens in the prompt",
];

/// Recognise a context-overflow error from its message.
///
/// Two shapes carry numbers and both are parsed, because the limit is what
/// lets the session stop guessing:
///
/// - `prompt is too long: 213 462 tokens > 200 000 maximum`
/// - `input length and `max_tokens` exceed context limit: 195 000 + 8 192 > 200 000`
///
/// A message with a known marker but no parseable numbers still counts as an
/// overflow; it just teaches nothing about the window.
#[must_use]
pub fn detect(message: &str) -> Option<ContextOverflow> {
    let lowered = message.to_ascii_lowercase();
    if !OVERFLOW_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        return None;
    }

    if let Some(overflow) = parse_greater_than(&lowered) {
        return Some(overflow);
    }

    // The marker matched but nothing could be parsed. Pair it with the first
    // plausible token count if there is one; otherwise report zero, which
    // callers treat as "overflow, limit unknown".
    Some(ContextOverflow {
        actual: None,
        limit: first_number(&lowered).unwrap_or(0),
    })
}

/// `... <size> > <limit>`, in either of the two shapes providers use:
///
/// - `213462 tokens > 200000 maximum` — the input size, named.
/// - `195000 + 8192 > 200000` — the input plus the requested reply.
///
/// The size is taken from the left of the comparison, never from anywhere
/// else in the sentence: `max_tokens` in the surrounding prose must not be
/// mistaken for the measured input.
fn parse_greater_than(message: &str) -> Option<ContextOverflow> {
    let greater = message.find('>')?;
    let left = &message[..greater];
    let limit = first_number(&message[greater + 1..])?;
    if left.contains('+') {
        let plus = left.rfind('+')?;
        let input = last_number(&left[..plus])?;
        return Some(ContextOverflow {
            actual: Some(input),
            limit,
        });
    }
    let left = left.trim_end();
    if left.ends_with("tokens") {
        let actual = last_number(left)?;
        return Some(ContextOverflow {
            actual: Some(actual),
            limit,
        });
    }
    None
}

/// The first number in `text`, ignoring digit-group separators.
fn first_number(text: &str) -> Option<usize> {
    numbers(text).into_iter().next()
}

/// The last number in `text`.
fn last_number(text: &str) -> Option<usize> {
    numbers(text).pop()
}

/// Numbers in `text` as the provider writes them: digits, optionally grouped
/// with spaces, underscores, commas or apostrophes (`213 462`, `1_000_000`).
fn numbers(text: &str) -> Vec<usize> {
    const GROUP_SEPARATORS: [char; 4] = [' ', '_', ',', '\''];
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut digits = String::new();
        let mut previous_was_digit = false;
        while i < chars.len() {
            let c = chars[i];
            if c.is_ascii_digit() {
                digits.push(c);
                previous_was_digit = true;
                i += 1;
            } else if previous_was_digit
                && GROUP_SEPARATORS.contains(&c)
                && chars.get(i + 1).is_some_and(char::is_ascii_digit)
            {
                // A group separator inside a number; not a new number.
                previous_was_digit = false;
                i += 1;
            } else {
                break;
            }
        }
        if let Ok(value) = digits.parse() {
            out.push(value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_anthropic_shape() {
        let overflow = detect("prompt is too long: 213462 tokens > 200000 maximum").unwrap();
        assert_eq!(overflow.actual, Some(213_462));
        assert_eq!(overflow.limit, 200_000);
        assert_eq!(overflow.learned_window(), 213_462);
    }

    #[test]
    fn parses_grouped_digits() {
        let overflow = detect("prompt is too long: 213 462 tokens > 200 000 maximum").unwrap();
        assert_eq!(overflow.actual, Some(213_462));
        assert_eq!(overflow.limit, 200_000);
    }

    #[test]
    fn parses_the_input_plus_output_shape() {
        let overflow =
            detect("input length and `max_tokens` exceed context limit: 195000 + 8192 > 200000")
                .unwrap();
        assert_eq!(overflow.actual, Some(195_000));
        assert_eq!(overflow.limit, 200_000);
    }

    #[test]
    fn parses_openai_style_codes() {
        let overflow = detect(
            "This model's maximum context length is 128000 tokens. However, your messages \
             resulted in 140000 tokens.",
        )
        .unwrap();
        assert_eq!(overflow.limit, 128_000);
    }

    #[test]
    fn marker_without_numbers_is_still_an_overflow() {
        let overflow = detect("input is too long for requested model").unwrap();
        assert_eq!(overflow.actual, None);
        assert_eq!(overflow.limit, 0);
    }

    #[test]
    fn unrelated_errors_are_not_overflows() {
        assert!(detect("invalid api key").is_none());
        assert!(detect("rate limit exceeded, retry later").is_none());
        assert!(detect("max_tokens must be at least 1").is_none());
    }
}
