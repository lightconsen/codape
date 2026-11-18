//! Voice input — the state machine behind `alt+m`.
//!
//! Manages the lifecycle of one transcription run: idle → listening →
//! transcribing → back to idle. There is no audio device here and no
//! recogniser: the REPL runs the command configured as `voice.command` and
//! this only tracks whether one is already running, so a second press is
//! refused rather than starting a second recorder on one microphone.

/// Voice recording state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceState {
    /// Not recording.
    Idle,
    /// Actively listening for audio input.
    Listening,
    /// Audio captured, waiting for transcription result.
    Transcribing,
    /// Transcription failed or error occurred.
    Error(String),
}

impl VoiceState {
    /// Create a new idle voice state.
    #[must_use]
    pub const fn new() -> Self {
        Self::Idle
    }

    /// Start listening.
    ///
    /// A state in `Error` is not a state to start from: recovering from a
    /// failure is [`VoiceState::retry`], so `start_listening` on an error
    /// leaves it as it is rather than clearing the reason the user needs to
    /// see. It used to compare against `Error(String::new())`, which never
    /// matched a real error and so silently swallowed it.
    pub fn start_listening(&mut self) {
        if *self == Self::Idle {
            *self = Self::Listening;
        }
    }

    /// Record a transcription result (consumes Transcribing state).
    ///
    /// A recogniser that heard nothing returns the machine to `Listening`
    /// rather than `Idle`: the run is not an outcome, so the next press is
    /// still the retry.
    pub fn finish_transcribing(&mut self, text: String) -> Option<String> {
        if *self == Self::Transcribing {
            if text.is_empty() {
                *self = Self::Listening;
                None
            } else {
                let result = Some(text);
                *self = Self::Idle;
                result
            }
        } else {
            None
        }
    }

    /// Transition to transcribing (audio captured, processing).
    pub fn begin_transcribing(&mut self) {
        if *self == Self::Listening {
            *self = Self::Transcribing;
        }
    }

    /// Record an error.
    pub fn fail(&mut self, error: String) {
        *self = Self::Error(error);
    }

    /// Retry after error (go back to idle).
    pub fn retry(&mut self) {
        if matches!(self, Self::Error(_)) {
            *self = Self::Idle;
        }
    }

    /// Check if voice input is currently active (not idle).
    #[must_use]
    pub const fn is_active(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

impl Default for VoiceState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_voice_cycle() {
        let mut state = VoiceState::new();
        assert_eq!(state, VoiceState::Idle);
        assert!(!state.is_active());

        state.start_listening();
        assert_eq!(state, VoiceState::Listening);
        assert!(state.is_active());

        state.begin_transcribing();
        assert_eq!(state, VoiceState::Transcribing);

        let result = state.finish_transcribing("hello world".into());
        assert_eq!(result, Some("hello world".into()));
        assert_eq!(state, VoiceState::Idle);
        assert!(!state.is_active());
    }

    #[test]
    fn error_state_and_recovery() {
        let mut state = VoiceState::new();
        state.start_listening();
        state.begin_transcribing();
        state.fail("microphone unavailable".into());
        assert!(matches!(state, VoiceState::Error(_)));
        assert!(state.is_active());

        state.retry();
        assert_eq!(state, VoiceState::Idle);
    }

    #[test]
    fn empty_transcription_returns_to_listening() {
        let mut state = VoiceState::new();
        state.start_listening();
        state.begin_transcribing();
        let result = state.finish_transcribing(String::new());
        assert!(result.is_none());
        assert_eq!(state, VoiceState::Listening);
    }

    #[test]
    fn start_listening_does_not_swallow_an_error() {
        // An error is recovered from with `retry`, never by starting over:
        // clearing it here hid the failure's reason from the notice the user
        // needs to read.
        let mut state = VoiceState::new();
        state.fail("microphone unavailable".into());
        state.start_listening();
        assert!(
            matches!(&state, VoiceState::Error(reason) if reason == "microphone unavailable"),
            "start_listening cleared the error: {state:?}"
        );
        state.retry();
        state.start_listening();
        assert_eq!(state, VoiceState::Listening);
    }

    #[test]
    fn consecutive_voice_inputs() {
        let mut state = VoiceState::new();
        for text in ["hi", "search for tests", "run cargo test"] {
            state.start_listening();
            state.begin_transcribing();
            let result = state.finish_transcribing(text.into());
            assert_eq!(result, Some(text.into()));
            assert_eq!(state, VoiceState::Idle);
        }
    }
}
