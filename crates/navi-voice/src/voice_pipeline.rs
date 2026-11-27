//! Core voice worker state machine ported from `apps/navi/workers/voice.py`.
//!
//! The Python worker mixes microphone capture, keyword spotting, endpointing,
//! STT dispatch, playback and UI confirmation in one event loop. This module
//! keeps only the decisions: a [`VoicePipeline`] receives events through
//! [`VoicePipeline::handle`] (or its convenience methods) and returns the
//! side effects the caller must perform as [`VoiceAction`]s. There is no I/O,
//! no clock and no threading here; every window is expressed in milliseconds
//! and supplied by the caller, which keeps the transitions testable.
//!
//! The port keeps four behaviours of the source that are easy to lose:
//! wake phrases are stripped from wake-triggered transcripts only, a bare
//! navigation word is treated as a false wake, a reply that names the wake
//! word needs a second keyword hit to interrupt playback, and a low-confidence
//! transcript is held for a tripled confirmation window.

use crate::speech_out::{SpeechOutputPlan, plan_speech_output};
use crate::vad::{VadConfig, VadEndpointDetector, VadEvent, pcm16_rms};
use navi_types::NaviResult;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Wake phrases used when no policy overrides them.
const DEFAULT_WAKE_WORDS: [&str; 2] = ["navi", "hey navi"];
/// Recognizer spellings of the name, from `voice.py`'s `NAME_SPELLINGS`.
const NAME_SPELLINGS: [&str; 13] = [
    "navi", "navy", "nevi", "nabi", "nadi", "nave", "novi", "navee", "navvy", "navie", "nevie",
    "nevy", "naby",
];
/// Words a caller may put before the name, from `voice.py`'s `_LEAD_INS`.
const LEAD_INS: [&str; 14] = [
    "hey", "hay", "hi", "ok", "okay", "hello", "yo", "so", "um", "uh", "oh", "well", "and", "a",
];
/// Lead-ins actually stripped from the front of a transcript by `_STRIP_RE`.
const STRIP_LEAD_INS: [&str; 6] = ["hey", "hay", "hi", "ok", "okay", "hello"];
/// Bare stop commands, which are dispatched without a confirmation hold.
const STOP_WORDS: [&str; 6] = [
    "stop",
    "cancel",
    "stop navi",
    "navi stop",
    "quiet",
    "shut up",
];
/// Second-hit window for a wake heard while the speaker is likely self-waking.
const WAKE_CONFIRM_MS: u64 = 8_000;
/// Time after playback during which its echo can still fire the spotter.
const ECHO_SUPPRESSION_MS: u64 = 600;
/// Shortest speech burst submitted as a stop-word check.
const STOP_CHECK_MIN_BLOCKS: usize = 3;
/// Longest speech burst submitted as a stop-word check.
const STOP_CHECK_MAX_BLOCKS: usize = 15;
/// Hold used when confidence is unknown and no normal hold is configured.
const UNCERTAIN_FALLBACK_MS: u64 = 4_500;
/// Upper bound for a configured sentence count.
const MAX_SPOKEN_SENTENCES: usize = 1_000;
const DEFAULT_MIN_CONFIDENCE: f64 = 0.6;
const DEFAULT_CONFIRM_TIMEOUT_MS: u64 = 1_500;

/// Lifecycle phase of the voice worker.
///
/// `Idle` waits for a wake word, `Listening` captures a command, `Processing`
/// waits for STT or the confirmation window, `Speaking` owns TTS playback, and
/// `Muted` drops every input until the microphone is enabled again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceMode {
    /// Microphone is on but no utterance is being captured.
    #[default]
    Idle,
    /// An utterance is being captured after a wake (or a barge-in).
    Listening,
    /// STT or the confirmation window is in progress.
    Processing,
    /// An assistant reply is being spoken.
    Speaking,
    /// The microphone is off; all input is ignored.
    Muted,
}

/// Tunable policy for wake words, barge-in and confirmation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct VoicePolicy {
    /// Wake phrases recognised at the front of a transcript.
    ///
    /// Both `"navi"` and the keyword-spotter spelling `"▁NA VI"` are accepted;
    /// each phrase is normalised to lowercase words when the policy is built.
    pub wake_words: Vec<String>,
    /// Whether a wake or a stop burst may interrupt playback.
    pub barge_in_enabled: bool,
    /// Confidence below which a transcript is held for a tripled window.
    pub min_confidence: f64,
    /// Maximum number of sentences handed to TTS for one reply.
    pub max_spoken_sentences: usize,
    /// Time in milliseconds the UI may claim a transcript before dispatch.
    pub confirm_timeout_ms: u64,
}

impl Default for VoicePolicy {
    fn default() -> Self {
        Self {
            wake_words: default_wake_words(),
            barge_in_enabled: true,
            min_confidence: DEFAULT_MIN_CONFIDENCE,
            max_spoken_sentences: 3,
            confirm_timeout_ms: DEFAULT_CONFIRM_TIMEOUT_MS,
        }
    }
}

impl VoicePolicy {
    /// Parses a policy from JSON, normalising invalid values instead of failing.
    ///
    /// Only malformed JSON is an error; unknown fields are ignored by serde and
    /// out-of-range numbers are clamped so a bad setting cannot stall the
    /// pipeline.
    pub fn from_json(text: &str) -> NaviResult<Self> {
        let policy: Self = serde_json::from_str(text)?;
        Ok(policy.sanitized())
    }

    fn sanitized(mut self) -> Self {
        let words: Vec<String> = self
            .wake_words
            .iter()
            .map(|word| normalize_wake_word(word))
            .filter(|word| !word.is_empty())
            .collect();
        self.wake_words = if words.is_empty() {
            default_wake_words()
        } else {
            words
        };
        self.min_confidence = if self.min_confidence.is_finite() {
            self.min_confidence.clamp(0.0, 1.0)
        } else {
            DEFAULT_MIN_CONFIDENCE
        };
        self.max_spoken_sentences = self.max_spoken_sentences.min(MAX_SPOKEN_SENTENCES);
        self
    }
}

/// Side effect requested by the pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceAction {
    /// Dispatch recognized command text to the chief.
    EmitTranscript(String),
    /// Stop the current playback process immediately.
    InterruptSpeech,
    /// Start speaking this planned reply.
    Speak(SpeechOutputPlan),
    /// Move to a new lifecycle phase.
    SetMode(VoiceMode),
    /// Nothing to do for this input.
    Ignore,
}

/// What the caller reports to the pipeline.
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceInput<'a> {
    /// The keyword spotter fired.
    WakeHit,
    /// One PCM16 audio frame from the microphone.
    Audio(&'a [i16]),
    /// STT result for the captured utterance.
    Transcript {
        raw: &'a str,
        confidence: Option<f64>,
        wake_triggered: bool,
    },
    /// A bare stop word was recognized from a stop-check burst.
    StopWord(&'a str),
    /// The UI accepted the held transcript itself.
    Accept,
    /// The UI withdrew the held transcript.
    Cancel,
    /// An assistant reply is ready to be spoken.
    Reply {
        text: &'a str,
        persona: &'a str,
        voice: &'a str,
        rate: &'a str,
    },
    /// The speaker finished the current utterance.
    SpeechFinished,
    /// The microphone was enabled or disabled by the user.
    MicEnabled(bool),
    /// Advance time-based windows without new audio.
    Tick,
}

/// What the first words of an utterance say about a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpeningVerdict {
    /// No spoken words at all.
    None,
    /// The name and a command follow.
    Name,
    /// Only calls to the name were spoken.
    NameOnly,
    /// The name and a stop command follow.
    Stop,
    /// Ordinary talk, or a call in the middle of a sentence.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingConfirmation {
    text: String,
    deadline_ms: u64,
    unsure: bool,
}

/// Decision engine for the voice worker.
///
/// Feed events with [`Self::handle`] and perform the returned actions in order.
/// Audio frames in `Idle` still calibrate the VAD but never open a capture; a
/// wake is required. In `Listening`, the detector's `UtteranceEnd` closes the
/// command and moves to `Processing`; the caller then runs STT and supplies the
/// result with [`Self::on_transcript`]. The captured audio is available through
/// [`Self::take_utterance`] before that call.
#[derive(Debug, Clone)]
pub struct VoicePipeline {
    policy: VoicePolicy,
    vad: VadEndpointDetector,
    mode: VoiceMode,
    pending_confirmation: Option<PendingConfirmation>,
    awaiting_transcript: bool,
    captured_audio: Vec<i16>,
    wake_hits: u32,
    wake_last_ms: u64,
    last_spoken: String,
    speech_ended_at_ms: Option<u64>,
    speech_canceled: bool,
    pending_reply: Option<SpeechOutputPlan>,
    follow_up: bool,
    stop_burst: usize,
    stop_blocks: VecDeque<Vec<i16>>,
    stop_check_ready: bool,
}

impl VoicePipeline {
    /// Creates a pipeline with default VAD tuning.
    pub fn new(policy: VoicePolicy) -> Self {
        Self::with_vad(policy, VadConfig::default())
    }

    /// Creates a pipeline with explicit endpointing tuning.
    pub fn with_vad(policy: VoicePolicy, vad_config: VadConfig) -> Self {
        Self {
            policy: policy.sanitized(),
            vad: VadEndpointDetector::new(vad_config),
            mode: VoiceMode::Idle,
            pending_confirmation: None,
            awaiting_transcript: false,
            captured_audio: Vec::new(),
            wake_hits: 0,
            wake_last_ms: 0,
            last_spoken: String::new(),
            speech_ended_at_ms: None,
            speech_canceled: false,
            pending_reply: None,
            follow_up: false,
            stop_burst: 0,
            stop_blocks: VecDeque::new(),
            stop_check_ready: false,
        }
    }

    /// Returns the active policy.
    pub fn policy(&self) -> &VoicePolicy {
        &self.policy
    }

    /// Returns the current mode.
    pub fn mode(&self) -> VoiceMode {
        self.mode
    }

    /// Returns the underlying endpointing detector.
    pub fn vad(&self) -> &VadEndpointDetector {
        &self.vad
    }

    /// Returns the held transcript, if a confirmation window is open.
    pub fn held_transcript(&self) -> Option<&str> {
        self.pending_confirmation
            .as_ref()
            .map(|pending| pending.text.as_str())
    }

    /// Returns the deadline of the open confirmation window.
    pub fn held_deadline_ms(&self) -> Option<u64> {
        self.pending_confirmation
            .as_ref()
            .map(|pending| pending.deadline_ms)
    }

    /// Returns whether the held transcript was marked unsure.
    pub fn held_is_unsure(&self) -> bool {
        self.pending_confirmation
            .as_ref()
            .is_some_and(|pending| pending.unsure)
    }

    /// Returns whether a transcript is still expected for the closed utterance.
    pub fn awaiting_transcript(&self) -> bool {
        self.awaiting_transcript
    }

    /// Returns whether a stop-word check burst is waiting to be transcribed.
    pub fn stop_check_requested(&self) -> bool {
        self.stop_check_ready
    }

    /// Takes the audio of the last closed utterance, if not taken already.
    pub fn take_utterance(&mut self) -> Option<Vec<i16>> {
        if self.captured_audio.is_empty() {
            None
        } else {
            self.awaiting_transcript = false;
            Some(std::mem::take(&mut self.captured_audio))
        }
    }

    /// Takes the audio collected for a stop-word check.
    pub fn take_stop_check_audio(&mut self) -> Vec<i16> {
        self.stop_check_ready = false;
        self.stop_blocks.drain(..).flatten().collect()
    }

    /// Returns how long a transcript with `confidence` is held, in milliseconds.
    ///
    /// An unsure result gets three times the normal window, or 4.5 seconds when
    /// no normal window is configured, matching `voice.py`'s hold calculation.
    pub fn confirmation_window_ms(&self, confidence: Option<f64>) -> u64 {
        let unsure = confidence
            .filter(|value| value.is_finite())
            .is_some_and(|value| value < self.policy.min_confidence);
        if !unsure {
            return self.policy.confirm_timeout_ms;
        }
        if self.policy.confirm_timeout_ms > 0 {
            self.policy.confirm_timeout_ms.saturating_mul(3)
        } else {
            UNCERTAIN_FALLBACK_MS
        }
    }

    /// Returns true while playback is live or its echo window is still open.
    pub fn recently_spoke(&self, now_ms: u64) -> bool {
        if self.mode == VoiceMode::Speaking {
            return true;
        }
        self.speech_ended_at_ms
            .is_some_and(|end| now_ms.saturating_sub(end) < ECHO_SUPPRESSION_MS)
    }

    /// Returns true when the last spoken text names the wake word.
    ///
    /// That is the moment a wake hit is likely Navi's own voice and needs a
    /// second hit before it may interrupt playback.
    pub fn self_wake_likely(&self, now_ms: u64) -> bool {
        self.recently_spoke(now_ms) && names_wake_word(&self.last_spoken, &self.policy.wake_words)
    }

    /// Handles one input and returns the actions the caller must perform.
    pub fn handle(&mut self, now_ms: u64, input: VoiceInput<'_>) -> Vec<VoiceAction> {
        match input {
            VoiceInput::WakeHit => self.handle_wake(now_ms),
            VoiceInput::Audio(samples) => self.handle_audio(now_ms, samples),
            VoiceInput::Transcript {
                raw,
                confidence,
                wake_triggered,
            } => self.handle_transcript(now_ms, raw, confidence, wake_triggered),
            VoiceInput::StopWord(transcript) => self.handle_stop_word(transcript),
            VoiceInput::Accept => self.handle_hold_decision(),
            VoiceInput::Cancel => self.handle_hold_decision(),
            VoiceInput::Reply {
                text,
                persona,
                voice,
                rate,
            } => self.handle_reply(text, persona, voice, rate),
            VoiceInput::SpeechFinished => self.handle_speech_finished(now_ms),
            VoiceInput::MicEnabled(enabled) => self.handle_mic_enabled(enabled),
            VoiceInput::Tick => self.handle_tick(now_ms),
        }
    }

    /// Reports a keyword-spotter hit.
    pub fn on_wake(&mut self, now_ms: u64) -> Vec<VoiceAction> {
        self.handle(now_ms, VoiceInput::WakeHit)
    }

    /// Feeds one PCM16 audio frame to the endpointing detector.
    pub fn on_audio(&mut self, now_ms: u64, samples: &[i16]) -> Vec<VoiceAction> {
        self.handle(now_ms, VoiceInput::Audio(samples))
    }

    /// Supplies the STT result for the utterance captured in `Listening`.
    pub fn on_transcript(
        &mut self,
        now_ms: u64,
        raw: &str,
        confidence: Option<f64>,
        wake_triggered: bool,
    ) -> Vec<VoiceAction> {
        self.handle(
            now_ms,
            VoiceInput::Transcript {
                raw,
                confidence,
                wake_triggered,
            },
        )
    }

    /// Supplies the STT result of a stop-check burst captured during playback.
    pub fn on_stop_word(&mut self, transcript: &str) -> Vec<VoiceAction> {
        self.handle(0, VoiceInput::StopWord(transcript))
    }

    /// Reports that the UI dispatched the held transcript.
    pub fn on_accept(&mut self) -> Vec<VoiceAction> {
        self.handle(0, VoiceInput::Accept)
    }

    /// Reports that the UI withdrew the held transcript.
    pub fn on_cancel(&mut self) -> Vec<VoiceAction> {
        self.handle(0, VoiceInput::Cancel)
    }

    /// Offers an assistant reply for playback.
    pub fn on_reply(
        &mut self,
        text: &str,
        persona: &str,
        voice: &str,
        rate: &str,
    ) -> Vec<VoiceAction> {
        self.handle(
            0,
            VoiceInput::Reply {
                text,
                persona,
                voice,
                rate,
            },
        )
    }

    /// Reports that the speaker finished the current utterance.
    pub fn on_speech_finished(&mut self, now_ms: u64) -> Vec<VoiceAction> {
        self.handle(now_ms, VoiceInput::SpeechFinished)
    }

    /// Enables or disables the microphone.
    pub fn on_mic_enabled(&mut self, enabled: bool) -> Vec<VoiceAction> {
        self.handle(0, VoiceInput::MicEnabled(enabled))
    }

    /// Advances confirmation, wake-confirmation and echo windows.
    pub fn tick(&mut self, now_ms: u64) -> Vec<VoiceAction> {
        self.handle(now_ms, VoiceInput::Tick)
    }

    fn handle_wake(&mut self, now_ms: u64) -> Vec<VoiceAction> {
        if self.mode == VoiceMode::Muted {
            return ignored();
        }
        // A new capture starts unclaimed: the previous hold is vetoed.
        self.pending_confirmation = None;
        if self.mode == VoiceMode::Speaking {
            if !self.policy.barge_in_enabled {
                return ignored();
            }
            if self.wake_hits == 0 || now_ms.saturating_sub(self.wake_last_ms) > WAKE_CONFIRM_MS {
                self.wake_hits = 1;
            } else {
                self.wake_hits = self.wake_hits.saturating_add(1);
            }
            self.wake_last_ms = now_ms;
            let needed = if self.self_wake_likely(now_ms) { 2 } else { 1 };
            if self.wake_hits < needed {
                // Unconfirmed hit: discard the capture so the next hit counts.
                let _ = self.vad.finish();
                return ignored();
            }
            self.wake_hits = 0;
            self.speech_canceled = true;
            let listening = self.enter_listening();
            return vec![VoiceAction::InterruptSpeech, listening];
        }
        if self.mode == VoiceMode::Listening {
            return ignored();
        }
        let listening = self.enter_listening();
        vec![listening]
    }

    fn handle_audio(&mut self, now_ms: u64, samples: &[i16]) -> Vec<VoiceAction> {
        if self.mode == VoiceMode::Muted {
            return ignored();
        }
        let quiet = pcm16_rms(samples) <= self.vad.threshold();
        let event = self.vad.feed(samples);
        match self.mode {
            VoiceMode::Speaking => {
                self.expire_wake_hits(now_ms);
                if self.policy.barge_in_enabled {
                    self.feed_stop_detector(samples, quiet);
                }
                ignored()
            }
            VoiceMode::Listening => match event {
                Some(VadEvent::UtteranceEnd(audio)) => {
                    self.captured_audio = audio;
                    self.awaiting_transcript = true;
                    self.mode = VoiceMode::Processing;
                    vec![VoiceAction::SetMode(VoiceMode::Processing)]
                }
                _ => ignored(),
            },
            _ => ignored(),
        }
    }

    fn handle_transcript(
        &mut self,
        now_ms: u64,
        raw: &str,
        confidence: Option<f64>,
        wake_triggered: bool,
    ) -> Vec<VoiceAction> {
        if self.mode == VoiceMode::Muted
            || !matches!(self.mode, VoiceMode::Listening | VoiceMode::Processing)
        {
            return ignored();
        }
        let command = command_for_with(raw, wake_triggered, &self.policy.wake_words);
        let verdict = opening_verdict(raw, wake_triggered);
        if command.is_empty() {
            if wake_triggered
                && (verdict == OpeningVerdict::NameOnly
                    || only_wake_with(raw, &self.policy.wake_words))
            {
                // They only called again; keep the capture open for the request.
                return ignored();
            }
            self.follow_up = false;
            return vec![self.enter_idle()];
        }
        self.follow_up = false;
        self.awaiting_transcript = false;
        self.captured_audio.clear();
        let hold_ms = self.confirmation_window_ms(confidence);
        if is_stop(&command) || hold_ms == 0 {
            // Stop commands bypass the hold; every held second delays a stop.
            return vec![VoiceAction::EmitTranscript(command), self.enter_idle()];
        }
        let unsure = confidence
            .filter(|value| value.is_finite())
            .is_some_and(|value| value < self.policy.min_confidence);
        self.pending_confirmation = Some(PendingConfirmation {
            text: command,
            deadline_ms: now_ms.saturating_add(hold_ms),
            unsure,
        });
        self.mode = VoiceMode::Processing;
        vec![VoiceAction::SetMode(VoiceMode::Processing)]
    }

    fn handle_stop_word(&mut self, transcript: &str) -> Vec<VoiceAction> {
        if self.mode != VoiceMode::Speaking || !self.policy.barge_in_enabled || !is_stop(transcript)
        {
            return ignored();
        }
        self.wake_hits = 0;
        self.speech_canceled = true;
        let idle = self.enter_idle();
        vec![VoiceAction::InterruptSpeech, idle]
    }

    fn handle_hold_decision(&mut self) -> Vec<VoiceAction> {
        if self.pending_confirmation.is_none() && self.mode != VoiceMode::Processing {
            return ignored();
        }
        // The other side owns the dispatch; emitting here would run it twice.
        vec![self.enter_idle()]
    }

    fn handle_reply(
        &mut self,
        text: &str,
        persona: &str,
        voice: &str,
        rate: &str,
    ) -> Vec<VoiceAction> {
        if self.mode == VoiceMode::Muted {
            return ignored();
        }
        let plan = plan_speech_output(text, persona, voice, rate, self.policy.max_spoken_sentences);
        if plan.spoken_text.is_empty() {
            return ignored();
        }
        if self.mode == VoiceMode::Speaking {
            // Only the newest reply is kept for a later turn.
            self.pending_reply = Some(plan);
            return ignored();
        }
        self.start_speech(plan)
    }

    fn start_speech(&mut self, plan: SpeechOutputPlan) -> Vec<VoiceAction> {
        self.pending_confirmation = None;
        self.last_spoken = plan.spoken_text.clone();
        self.speech_ended_at_ms = None;
        self.speech_canceled = false;
        self.follow_up = false;
        self.mode = VoiceMode::Speaking;
        vec![
            VoiceAction::Speak(plan),
            VoiceAction::SetMode(VoiceMode::Speaking),
        ]
    }

    fn handle_speech_finished(&mut self, now_ms: u64) -> Vec<VoiceAction> {
        if self.mode != VoiceMode::Speaking {
            return ignored();
        }
        self.speech_ended_at_ms = Some(now_ms);
        if let Some(plan) = self.pending_reply.take() {
            self.last_spoken = plan.spoken_text.clone();
            self.speech_canceled = false;
            return vec![VoiceAction::Speak(plan)];
        }
        let asked_question = self.last_spoken.trim_end().ends_with('?');
        self.follow_up = asked_question && !self.speech_canceled;
        let idle = self.enter_idle();
        vec![idle]
    }

    fn handle_mic_enabled(&mut self, enabled: bool) -> Vec<VoiceAction> {
        if enabled {
            if self.mode == VoiceMode::Muted {
                return vec![self.enter_idle()];
            }
            return ignored();
        }
        self.pending_confirmation = None;
        self.pending_reply = None;
        self.follow_up = false;
        self.wake_hits = 0;
        self.mode = VoiceMode::Muted;
        vec![VoiceAction::SetMode(VoiceMode::Muted)]
    }

    fn handle_tick(&mut self, now_ms: u64) -> Vec<VoiceAction> {
        if self.mode == VoiceMode::Muted {
            return ignored();
        }
        if self.mode == VoiceMode::Speaking {
            self.expire_wake_hits(now_ms);
        }
        let expired = self
            .pending_confirmation
            .as_ref()
            .is_some_and(|pending| now_ms >= pending.deadline_ms);
        if expired && self.mode == VoiceMode::Processing {
            let Some(pending) = self.pending_confirmation.take() else {
                return ignored();
            };
            let idle = self.enter_idle();
            return vec![VoiceAction::EmitTranscript(pending.text), idle];
        }
        let echo_over = self
            .speech_ended_at_ms
            .is_some_and(|end| now_ms.saturating_sub(end) >= ECHO_SUPPRESSION_MS);
        if self.follow_up && echo_over && self.mode == VoiceMode::Idle {
            self.follow_up = false;
            return vec![self.enter_listening()];
        }
        ignored()
    }

    fn enter_listening(&mut self) -> VoiceAction {
        self.pending_confirmation = None;
        self.awaiting_transcript = false;
        self.captured_audio.clear();
        self.follow_up = false;
        let _ = self.vad.finish();
        self.mode = VoiceMode::Listening;
        VoiceAction::SetMode(VoiceMode::Listening)
    }

    fn enter_idle(&mut self) -> VoiceAction {
        self.pending_confirmation = None;
        self.awaiting_transcript = false;
        self.captured_audio.clear();
        let _ = self.vad.finish();
        self.mode = VoiceMode::Idle;
        VoiceAction::SetMode(VoiceMode::Idle)
    }

    fn expire_wake_hits(&mut self, now_ms: u64) {
        if self.wake_hits > 0 && now_ms.saturating_sub(self.wake_last_ms) > WAKE_CONFIRM_MS {
            self.wake_hits = 0;
        }
    }

    fn feed_stop_detector(&mut self, samples: &[i16], quiet: bool) {
        if quiet {
            if self.stop_burst >= STOP_CHECK_MIN_BLOCKS && !self.stop_blocks.is_empty() {
                self.stop_check_ready = true;
            } else {
                self.stop_blocks.clear();
            }
            self.stop_burst = 0;
            return;
        }
        self.stop_burst = self.stop_burst.saturating_add(1);
        if self.stop_burst > STOP_CHECK_MAX_BLOCKS {
            self.stop_burst = 0;
            self.stop_blocks.clear();
            return;
        }
        self.stop_blocks.push_back(samples.to_vec());
        while self.stop_blocks.len() > STOP_CHECK_MAX_BLOCKS {
            self.stop_blocks.pop_front();
        }
    }
}

fn ignored() -> Vec<VoiceAction> {
    vec![VoiceAction::Ignore]
}

fn default_wake_words() -> Vec<String> {
    DEFAULT_WAKE_WORDS
        .iter()
        .map(|&word| word.to_string())
        .collect()
}

/// Maps a keyword-spotter token spelling to the spoken wake phrase.
///
/// The model's tokens are syllable pieces (`▁NA VI`, `▁HE Y ▁NA VI`), so the
/// marker is expanded to spaces and the two documented spellings are mapped to
/// the phrases heard in a transcript. Other configured phrases keep their
/// words in order.
fn normalize_wake_word(word: &str) -> String {
    let lowered = word.replace('\u{2581}', " ").to_lowercase();
    let pieces: Vec<&str> = lowered.split_whitespace().collect();
    match pieces.as_slice() {
        ["na", "vi"] => "navi".to_string(),
        ["he", "y", "na", "vi"] => "hey navi".to_string(),
        _ => pieces.join(" "),
    }
}

/// Extracts lowercase `[a-z']+` words, matching the Python word scanner.
fn extract_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c == '\'' {
            current.push(c);
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn is_vowel_or_y(byte: u8) -> bool {
    matches!(byte, b'a' | b'e' | b'i' | b'o' | b'u' | b'y')
}

fn is_name_consonant(byte: u8) -> bool {
    matches!(byte, b'v' | b'b' | b'p' | b'f' | b'd' | b't')
}

/// Matches `[nm][aeiouy]+[wr]?[vbpfdt]+[aeiouy]*` at `start`.
fn name_shape_from(bytes: &[u8], mut start: usize) -> bool {
    if !bytes
        .get(start)
        .is_some_and(|&byte| matches!(byte, b'n' | b'm'))
    {
        return false;
    }
    start += 1;
    let vowels = start;
    while bytes.get(start).is_some_and(|&byte| is_vowel_or_y(byte)) {
        start += 1;
    }
    if start == vowels {
        return false;
    }
    if bytes
        .get(start)
        .is_some_and(|&byte| matches!(byte, b'w' | b'r'))
    {
        start += 1;
    }
    let consonants = start;
    while bytes
        .get(start)
        .is_some_and(|&byte| is_name_consonant(byte))
    {
        start += 1;
    }
    if start == consonants {
        return false;
    }
    while bytes.get(start).is_some_and(|&byte| is_vowel_or_y(byte)) {
        start += 1;
    }
    start == bytes.len()
}

/// Matches the source's `_NAME_SHAPE` regular expression.
///
/// The optional `[aei]n?` prefix is tried at every plausible split because a
/// greedy match would reject words such as "inavi".
fn matches_name_shape(word: &str) -> bool {
    let bytes = word.as_bytes();
    let mut starts = vec![0];
    if let Some(&first) = bytes.first()
        && matches!(first, b'a' | b'e' | b'i')
    {
        starts.push(1);
        if bytes.get(1) == Some(&b'n') {
            starts.push(2);
        }
    }
    starts.iter().any(|&start| name_shape_from(bytes, start))
}

fn is_name_word(word: &str, spotted: bool) -> bool {
    NAME_SPELLINGS.contains(&word) || (spotted && matches_name_shape(word))
}

fn is_lead_in(word: &str) -> bool {
    LEAD_INS.contains(&word)
}

/// Classifies the first words of an utterance, as `opening_verdict` does.
pub fn opening_verdict(text: &str, spotted: bool) -> OpeningVerdict {
    let words = extract_words(text);
    let mut start = 0;
    while words.get(start).is_some_and(|word| is_lead_in(word)) {
        start += 1;
    }
    let Some(first) = words.get(start) else {
        return OpeningVerdict::None;
    };
    let rest_start = if is_name_word(first, spotted) {
        start + 1
    } else if let Some(second) = words.get(start + 1) {
        let joined = format!("{first}{second}");
        if is_name_word(&joined, spotted) {
            start + 2
        } else {
            return OpeningVerdict::Other;
        }
    } else {
        return OpeningVerdict::Other;
    };
    let rest = &words[rest_start..];
    if rest.is_empty() {
        OpeningVerdict::NameOnly
    } else {
        let joined = rest.join(" ");
        if STOP_WORDS.contains(&joined.as_str()) {
            OpeningVerdict::Stop
        } else {
            OpeningVerdict::Name
        }
    }
}

/// Returns true when a word starts with a navigation term rather than a call.
///
/// The KWS model hears "navigation" and "navigate" as the bare name; this guard
/// runs on the raw transcript so "Navi navigate to X" still yields a command.
pub fn ignored_wake(transcript: &str) -> bool {
    let lower = transcript.to_lowercase();
    NAVIGATION_WORDS.iter().any(|word| {
        lower.strip_prefix(word).is_some_and(|rest| {
            rest.chars()
                .next()
                .is_none_or(|next| !next.is_alphanumeric())
        })
    })
}

const NAVIGATION_WORDS: [&str; 3] = ["navigation", "navigator", "navigate"];

/// Returns true for a bare stop command, ignoring case and outer punctuation.
pub fn is_stop(transcript: &str) -> bool {
    let lowered = transcript.trim().to_lowercase();
    let normalized = lowered.trim_matches(|c: char| matches!(c, ' ' | '.' | '!' | '?'));
    STOP_WORDS.contains(&normalized)
}

fn consume_separators(text: &str) -> usize {
    let mut end = 0;
    for c in text.chars() {
        if c.is_whitespace() || matches!(c, ',' | '.' | '!' | '?') {
            end += c.len_utf8();
        } else {
            break;
        }
    }
    end
}

fn strip_lead_in(text: &str) -> Option<&str> {
    for prefix in STRIP_LEAD_INS {
        if text.len() < prefix.len() || !text[..prefix.len()].eq_ignore_ascii_case(prefix) {
            continue;
        }
        let rest = &text[prefix.len()..];
        let separators = consume_separators(rest);
        if separators > 0 {
            return Some(&rest[separators..]);
        }
    }
    None
}

/// Matches a normalized wake phrase at the start of `text`.
fn match_phrase_chars<'a>(text: &'a str, phrase: &str) -> Option<&'a str> {
    let mut text_at = 0;
    let mut phrase_at = 0;
    while phrase_at < phrase.len() {
        let expected = phrase[phrase_at..].chars().next()?;
        if expected.is_whitespace() {
            let before = text_at;
            while text
                .get(text_at..)
                .and_then(|rest| rest.chars().next())
                .is_some_and(char::is_whitespace)
            {
                let c = text[text_at..].chars().next()?;
                text_at += c.len_utf8();
            }
            if text_at == before {
                return None;
            }
        } else {
            let actual = text.get(text_at..)?.chars().next()?;
            if !expected.eq_ignore_ascii_case(&actual) {
                return None;
            }
            text_at += actual.len_utf8();
        }
        phrase_at += expected.len_utf8();
    }
    text.get(text_at..)
}

fn strip_separators(text: &str) -> &str {
    &text[consume_separators(text)..]
}

fn match_wake_phrase<'a>(text: &'a str, phrase: &str) -> Option<&'a str> {
    let after = match_phrase_chars(text, phrase)?;
    if after
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '\'')
    {
        return None;
    }
    Some(strip_separators(after))
}

/// Removes repeated leading wake phrases from a transcript.
fn command_text_with(transcript: &str, wake_words: &[String]) -> String {
    let mut text = transcript.trim().to_string();
    loop {
        let candidate = strip_lead_in(&text).unwrap_or(&text);
        let stripped = wake_words
            .iter()
            .find_map(|wake| match_wake_phrase(candidate, wake))
            .or_else(|| {
                NAME_SPELLINGS
                    .iter()
                    .find_map(|name| match_wake_phrase(candidate, name))
            });
        let Some(rest) = stripped else {
            return text;
        };
        let next = rest
            .trim_matches(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | '!' | '?'))
            .to_string();
        if next == text {
            return text;
        }
        text = next;
    }
}

/// Drops the leading wake phrase with the default wake words.
pub fn command_text(transcript: &str) -> String {
    command_text_with(transcript, &default_wake_words())
}

/// Decides the dispatch text for a raw STT result with the default wake words.
pub fn command_for(transcript: &str, wake_triggered: bool) -> String {
    command_for_with(transcript, wake_triggered, &default_wake_words())
}

/// Recognizes engine labels for silence such as `[BLANK_AUDIO]` and `[ Silence ]`.
fn is_no_speech_label(transcript: &str) -> bool {
    let trimmed = transcript.trim();
    let Some(opening) = trimmed.chars().next() else {
        return false;
    };
    if !matches!(opening, '[' | '(') {
        return false;
    }
    let body = &trimmed[opening.len_utf8()..];
    let body_len = body
        .chars()
        .take_while(|&c| !matches!(c, ']' | ')'))
        .map(char::len_utf8)
        .sum::<usize>();
    let after = &body[body_len..];
    let after = if after.starts_with([']', ')']) {
        &after[1..]
    } else {
        after
    };
    after.chars().all(|c| c.is_whitespace() || c == '.')
}

fn command_for_with(transcript: &str, wake_triggered: bool, wake_words: &[String]) -> String {
    let raw = transcript.trim();
    if raw.is_empty() || is_no_speech_label(raw) {
        return String::new();
    }
    if wake_triggered && ignored_wake(raw) {
        return String::new();
    }
    if wake_triggered {
        command_text_with(raw, wake_words)
    } else {
        raw.to_string()
    }
}

/// Returns true when a capture held nothing but calls to the name.
pub fn only_wake(raw: &str) -> bool {
    only_wake_with(raw, &default_wake_words())
}

fn only_wake_with(raw: &str, wake_words: &[String]) -> bool {
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && !ignored_wake(trimmed)
        && command_text_with(trimmed, wake_words).is_empty()
}

fn names_wake_word(text: &str, wake_words: &[String]) -> bool {
    let lower = text.to_lowercase();
    wake_words.iter().any(|wake| {
        wake.split_whitespace()
            .next_back()
            .is_some_and(|name| lower.contains(&name.to_lowercase()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn short_vad() -> VadConfig {
        VadConfig {
            calibrate_blocks: 100,
            pre_roll_frames: 1,
            min_speech_frames: 1,
            silence_hangover_frames: 2,
            ..VadConfig::default()
        }
    }

    fn listening_pipeline() -> VoicePipeline {
        let mut pipeline = VoicePipeline::new(VoicePolicy::default());
        assert_eq!(
            pipeline.on_wake(0),
            vec![VoiceAction::SetMode(VoiceMode::Listening)]
        );
        pipeline
    }

    fn captured_pipeline() -> VoicePipeline {
        let mut pipeline = VoicePipeline::with_vad(VoicePolicy::default(), short_vad());
        assert_eq!(
            pipeline.on_wake(0),
            vec![VoiceAction::SetMode(VoiceMode::Listening)]
        );
        assert_eq!(pipeline.on_audio(1, &[8_000]), vec![VoiceAction::Ignore]);
        assert_eq!(pipeline.on_audio(2, &[8_000]), vec![VoiceAction::Ignore]);
        assert_eq!(pipeline.on_audio(3, &[0]), vec![VoiceAction::Ignore]);
        assert_eq!(
            pipeline.on_audio(4, &[0]),
            vec![VoiceAction::SetMode(VoiceMode::Processing)]
        );
        pipeline
    }

    fn speaking_pipeline(text: &str) -> VoicePipeline {
        let mut pipeline = VoicePipeline::new(VoicePolicy::default());
        let actions = pipeline.on_reply(text, "navi", "voice", "+0%");
        assert!(actions.contains(&VoiceAction::SetMode(VoiceMode::Speaking)));
        pipeline
    }

    #[test]
    fn policy_defaults_and_json_normalise_keyword_tokens() {
        let policy = VoicePolicy::default();
        assert_eq!(policy.wake_words, ["navi", "hey navi"]);
        assert!(policy.barge_in_enabled);
        assert!((policy.min_confidence - 0.6).abs() < 1e-12);
        assert_eq!(policy.max_spoken_sentences, 3);
        assert_eq!(policy.confirm_timeout_ms, 1_500);

        let parsed = VoicePolicy::from_json(
            r#"{"wake_words":["▁NA VI","▁HE Y ▁NA VI","","▁NA VI"],
                "min_confidence":9,"max_spoken_sentences":5000,
                "confirm_timeout_ms":250,"barge_in_enabled":false}"#,
        )
        .expect("parse policy");
        assert_eq!(parsed.wake_words, ["navi", "hey navi", "navi"]);
        assert!((parsed.min_confidence - 1.0).abs() < 1e-12);
        assert_eq!(parsed.max_spoken_sentences, MAX_SPOKEN_SENTENCES);
        assert_eq!(parsed.confirm_timeout_ms, 250);
        assert!(!parsed.barge_in_enabled);

        assert!(
            VoicePolicy::from_json(r#"{"wake_words":[]}"#)
                .expect("empty words")
                .wake_words
                .iter()
                .all(|word| !word.is_empty())
        );
        assert!(VoicePolicy::from_json("not json").is_err());
    }

    #[test]
    fn opening_verdict_separates_calls_stops_and_ordinary_talk() {
        assert_eq!(opening_verdict("", true), OpeningVerdict::None);
        assert_eq!(opening_verdict("navi", true), OpeningVerdict::NameOnly);
        assert_eq!(
            opening_verdict("hey navi, navi", true),
            OpeningVerdict::Name
        );
        assert_eq!(opening_verdict("navi stop", true), OpeningVerdict::Stop);
        assert_eq!(
            opening_verdict("okay navy open files", false),
            OpeningVerdict::Name
        );
        assert_eq!(
            opening_verdict("inavi, what now?", true),
            OpeningVerdict::Name
        );
        assert_eq!(opening_verdict("hello there", true), OpeningVerdict::Other);
        assert_eq!(
            opening_verdict("navy, list files", false),
            OpeningVerdict::Name
        );
        assert_eq!(
            opening_verdict("navy, list files", true),
            OpeningVerdict::Name
        );
    }

    #[test]
    fn command_text_strips_repeated_wakes_but_not_navigation() {
        assert_eq!(command_text("hey navi, open files"), "open files");
        assert_eq!(command_text("navi, navi, open files"), "open files");
        assert_eq!(command_text("navy! list files."), "list files");
        assert_eq!(command_text("hello there"), "hello there");
        assert_eq!(command_text(""), "");

        assert!(ignored_wake("Navigation to the store"));
        assert!(ignored_wake("navigator, please"));
        assert!(ignored_wake("Navigate to X"));
        assert!(!ignored_wake(" navi navigate to X"));
        assert_eq!(command_for("Navi navigate to X", true), "navigate to X");
        assert_eq!(command_for("navigate to X", true), "");
        assert_eq!(command_for("navy was here", false), "navy was here");
        assert!(only_wake("hey navi, navi"));
        assert!(!only_wake("navigation"));
        assert!(is_stop("  NAVI stop! "));
        assert!(!is_stop("please stop now"));
    }

    #[test]
    fn wake_starts_capture_and_vad_closes_the_utterance() {
        let mut pipeline = VoicePipeline::with_vad(VoicePolicy::default(), short_vad());
        assert_eq!(pipeline.mode(), VoiceMode::Idle);
        assert_eq!(pipeline.on_audio(0, &[8_000]), vec![VoiceAction::Ignore]);
        assert!(!pipeline.awaiting_transcript());
        assert_eq!(
            pipeline.on_wake(1),
            vec![VoiceAction::SetMode(VoiceMode::Listening)]
        );
        assert_eq!(pipeline.mode(), VoiceMode::Listening);
        assert_eq!(pipeline.on_audio(2, &[8_000]), vec![VoiceAction::Ignore]);
        assert_eq!(pipeline.on_audio(3, &[0]), vec![VoiceAction::Ignore]);
        assert_eq!(
            pipeline.on_audio(4, &[0]),
            vec![VoiceAction::SetMode(VoiceMode::Processing)]
        );
        assert!(pipeline.awaiting_transcript());
        assert_eq!(pipeline.take_utterance(), Some(vec![8_000, 0, 0]));
        assert_eq!(pipeline.take_utterance(), None);
        assert_eq!(pipeline.vad().state(), crate::vad::VadState::Silence);
    }

    #[test]
    fn confirmation_window_dispatches_exactly_once_at_the_deadline() {
        let mut pipeline = captured_pipeline();
        let actions = pipeline.on_transcript(100, "hey navi open files", Some(0.9), true);
        assert_eq!(actions, vec![VoiceAction::SetMode(VoiceMode::Processing)]);
        assert_eq!(pipeline.held_transcript(), Some("open files"));
        assert_eq!(pipeline.held_deadline_ms(), Some(1_600));
        assert!(!pipeline.held_is_unsure());
        assert_eq!(pipeline.tick(1_599), vec![VoiceAction::Ignore]);
        assert_eq!(
            pipeline.tick(1_600),
            vec![
                VoiceAction::EmitTranscript("open files".to_string()),
                VoiceAction::SetMode(VoiceMode::Idle),
            ]
        );
        assert_eq!(pipeline.held_transcript(), None);
        assert_eq!(pipeline.tick(2_000), vec![VoiceAction::Ignore]);
        assert_eq!(pipeline.tick(3_000), vec![VoiceAction::Ignore]);
    }

    #[test]
    fn low_confidence_triples_the_window_and_zero_timeout_uses_fallback() {
        let mut pipeline = captured_pipeline();
        assert_eq!(pipeline.confirmation_window_ms(Some(0.9)), 1_500);
        assert_eq!(pipeline.confirmation_window_ms(Some(0.2)), 4_500);
        assert_eq!(pipeline.confirmation_window_ms(None), 1_500);

        pipeline.on_transcript(0, "navi open files", Some(0.2), true);
        assert!(pipeline.held_is_unsure());
        assert_eq!(pipeline.held_deadline_ms(), Some(4_500));
        assert_eq!(pipeline.tick(4_499), vec![VoiceAction::Ignore]);
        assert_eq!(
            pipeline.tick(4_500),
            vec![
                VoiceAction::EmitTranscript("open files".to_string()),
                VoiceAction::SetMode(VoiceMode::Idle),
            ]
        );

        let policy = VoicePolicy {
            confirm_timeout_ms: 0,
            ..VoicePolicy::default()
        };
        let mut immediate = VoicePipeline::new(policy);
        assert_eq!(
            immediate.confirmation_window_ms(Some(0.2)),
            UNCERTAIN_FALLBACK_MS
        );
        assert_eq!(immediate.confirmation_window_ms(None), 0);
        assert_eq!(
            immediate.on_wake(0),
            vec![VoiceAction::SetMode(VoiceMode::Listening)]
        );
        let actions = immediate.on_transcript(0, "stop", Some(0.2), false);
        assert_eq!(
            actions,
            vec![
                VoiceAction::EmitTranscript("stop".to_string()),
                VoiceAction::SetMode(VoiceMode::Idle),
            ]
        );
    }

    #[test]
    fn accept_cancel_and_new_wake_veto_the_held_transcript() {
        let mut accepted = captured_pipeline();
        accepted.on_transcript(0, "navi open files", Some(0.9), true);
        assert_eq!(
            accepted.on_accept(),
            vec![VoiceAction::SetMode(VoiceMode::Idle)]
        );
        assert_eq!(accepted.tick(10_000), vec![VoiceAction::Ignore]);

        let mut canceled = captured_pipeline();
        canceled.on_transcript(0, "navi open files", Some(0.9), true);
        assert_eq!(
            canceled.on_cancel(),
            vec![VoiceAction::SetMode(VoiceMode::Idle)]
        );
        assert_eq!(canceled.tick(10_000), vec![VoiceAction::Ignore]);

        let mut vetoed = captured_pipeline();
        vetoed.on_transcript(0, "navi open files", Some(0.9), true);
        assert_eq!(
            vetoed.on_wake(10),
            vec![VoiceAction::SetMode(VoiceMode::Listening)]
        );
        assert_eq!(vetoed.tick(10_000), vec![VoiceAction::Ignore]);
    }

    #[test]
    fn only_a_call_keeps_the_capture_open_and_empty_text_closes_it() {
        let mut pipeline = listening_pipeline();
        assert_eq!(
            pipeline.on_transcript(0, "hey navi, navi", Some(0.9), true),
            vec![VoiceAction::Ignore]
        );
        assert_eq!(pipeline.mode(), VoiceMode::Listening);
        assert_eq!(
            pipeline.on_transcript(1, "  [BLANK_AUDIO]  ", None, true),
            vec![VoiceAction::SetMode(VoiceMode::Idle)]
        );
        assert_eq!(pipeline.mode(), VoiceMode::Idle);
        assert_eq!(
            pipeline.on_transcript(2, "open files", Some(0.9), true),
            vec![VoiceAction::Ignore]
        );
    }

    #[test]
    fn stop_words_bypass_the_confirmation_hold() {
        let mut pipeline = captured_pipeline();
        assert_eq!(
            pipeline.on_transcript(0, "navi stop", Some(0.1), true),
            vec![
                VoiceAction::EmitTranscript("stop".to_string()),
                VoiceAction::SetMode(VoiceMode::Idle),
            ]
        );
        assert_eq!(pipeline.held_transcript(), None);

        let mut second = captured_pipeline();
        assert_eq!(
            second.on_transcript(0, "cancel", None, false),
            vec![
                VoiceAction::EmitTranscript("cancel".to_string()),
                VoiceAction::SetMode(VoiceMode::Idle),
            ]
        );
    }

    #[test]
    fn self_wake_needs_two_hits_and_the_window_expires() {
        let mut pipeline = speaking_pipeline("Your wake word is Navi.");
        assert!(pipeline.self_wake_likely(0));
        assert_eq!(pipeline.on_wake(1_000), vec![VoiceAction::Ignore]);
        assert_eq!(
            pipeline.on_wake(2_000),
            vec![
                VoiceAction::InterruptSpeech,
                VoiceAction::SetMode(VoiceMode::Listening),
            ]
        );
        assert_eq!(pipeline.mode(), VoiceMode::Listening);

        let mut expired = speaking_pipeline("Navi here.");
        assert_eq!(expired.on_wake(0), vec![VoiceAction::Ignore]);
        assert_eq!(expired.tick(8_001), vec![VoiceAction::Ignore]);
        assert_eq!(expired.on_wake(8_002), vec![VoiceAction::Ignore]);
        assert_eq!(expired.mode(), VoiceMode::Speaking);
    }

    #[test]
    fn ordinary_reply_interrupts_on_one_hit_and_barge_in_can_be_disabled() {
        let mut pipeline = speaking_pipeline("Done.");
        assert!(!pipeline.self_wake_likely(0));
        assert_eq!(
            pipeline.on_wake(100),
            vec![
                VoiceAction::InterruptSpeech,
                VoiceAction::SetMode(VoiceMode::Listening),
            ]
        );

        let policy = VoicePolicy {
            barge_in_enabled: false,
            ..VoicePolicy::default()
        };
        let mut disabled = VoicePipeline::new(policy);
        let actions = disabled.on_reply("Done.", "navi", "voice", "+0%");
        assert!(actions.contains(&VoiceAction::SetMode(VoiceMode::Speaking)));
        assert_eq!(disabled.on_wake(0), vec![VoiceAction::Ignore]);
        assert_eq!(disabled.on_stop_word("stop"), vec![VoiceAction::Ignore]);
        assert_eq!(disabled.mode(), VoiceMode::Speaking);
    }

    #[test]
    fn stop_burst_is_collected_and_a_stop_word_interrupts_playback() {
        let mut pipeline = speaking_pipeline("Listening now.");
        let speech = [10_000_i16; 8];
        for _ in 0..3 {
            assert_eq!(pipeline.on_audio(0, &speech), vec![VoiceAction::Ignore]);
        }
        assert!(!pipeline.stop_check_requested());
        assert_eq!(pipeline.on_audio(1, &[0]), vec![VoiceAction::Ignore]);
        assert!(pipeline.stop_check_requested());
        assert_eq!(pipeline.take_stop_check_audio(), vec![10_000; 24]);
        assert!(!pipeline.stop_check_requested());
        assert_eq!(
            pipeline.on_stop_word("hello there"),
            vec![VoiceAction::Ignore]
        );

        assert_eq!(
            pipeline.on_stop_word("Stop!"),
            vec![
                VoiceAction::InterruptSpeech,
                VoiceAction::SetMode(VoiceMode::Idle),
            ]
        );
        assert_eq!(pipeline.mode(), VoiceMode::Idle);
    }

    #[test]
    fn newest_reply_queued_during_playback_is_spoken_when_finished() {
        let mut pipeline = speaking_pipeline("First answer.");
        let actions = pipeline.on_reply("Second answer.", "navi", "voice", "+0%");
        assert_eq!(actions, vec![VoiceAction::Ignore]);
        assert_eq!(
            pipeline.on_speech_finished(100),
            vec![VoiceAction::Speak(plan_speech_output(
                "Second answer.",
                "navi",
                "voice",
                "+0%",
                3,
            ))]
        );
        assert_eq!(pipeline.mode(), VoiceMode::Speaking);
        assert_eq!(
            pipeline.on_speech_finished(200),
            vec![VoiceAction::SetMode(VoiceMode::Idle)]
        );
        assert_eq!(
            pipeline.on_reply("  ", "navi", "voice", "+0%"),
            vec![VoiceAction::Ignore]
        );
        assert_eq!(pipeline.mode(), VoiceMode::Idle);
    }

    #[test]
    fn question_opens_a_follow_up_only_after_the_echo_window() {
        let mut pipeline = speaking_pipeline("Which one: A or B?");
        assert_eq!(
            pipeline.on_speech_finished(100),
            vec![VoiceAction::SetMode(VoiceMode::Idle)]
        );
        assert!(pipeline.recently_spoke(699));
        assert_eq!(pipeline.tick(699), vec![VoiceAction::Ignore]);
        assert_eq!(
            pipeline.tick(700),
            vec![VoiceAction::SetMode(VoiceMode::Listening)]
        );
        assert_eq!(pipeline.mode(), VoiceMode::Listening);
        assert_eq!(pipeline.tick(1_000), vec![VoiceAction::Ignore]);

        let mut canceled = speaking_pipeline("Which one?");
        canceled.on_wake(10);
        canceled.on_stop_word("stop");
        canceled.on_speech_finished(20);
        assert_eq!(canceled.tick(1_000), vec![VoiceAction::Ignore]);
    }

    #[test]
    fn mute_drops_all_input_and_recovering_returns_to_idle() {
        let mut pipeline = listening_pipeline();
        assert_eq!(
            pipeline.on_mic_enabled(false),
            vec![VoiceAction::SetMode(VoiceMode::Muted)]
        );
        assert_eq!(pipeline.on_wake(0), vec![VoiceAction::Ignore]);
        assert_eq!(pipeline.on_audio(0, &[8_000]), vec![VoiceAction::Ignore]);
        assert_eq!(
            pipeline.on_transcript(0, "open files", Some(0.9), true),
            vec![VoiceAction::Ignore]
        );
        assert_eq!(
            pipeline.on_reply("Hello.", "navi", "voice", "+0%"),
            vec![VoiceAction::Ignore]
        );
        assert_eq!(pipeline.tick(10_000), vec![VoiceAction::Ignore]);
        assert_eq!(pipeline.mode(), VoiceMode::Muted);
        assert_eq!(
            pipeline.on_mic_enabled(true),
            vec![VoiceAction::SetMode(VoiceMode::Idle)]
        );
        assert_eq!(pipeline.on_mic_enabled(true), vec![VoiceAction::Ignore]);
    }

    #[test]
    fn public_types_round_trip_through_serde() {
        assert_eq!(
            serde_json::to_string(&VoiceMode::Speaking).expect("mode"),
            "\"speaking\""
        );
        assert_eq!(
            serde_json::to_string(&OpeningVerdict::NameOnly).expect("verdict"),
            "\"name_only\""
        );
        let action = VoiceAction::EmitTranscript("go".to_string());
        let json = serde_json::to_string(&action).expect("action");
        assert_eq!(
            serde_json::from_str::<VoiceAction>(&json).expect("parse action"),
            action
        );
        let policy_json = serde_json::to_string(&VoicePolicy::default()).expect("policy");
        assert_eq!(
            serde_json::from_str::<VoicePolicy>(&policy_json).expect("parse policy"),
            VoicePolicy::default()
        );
        assert!(serde_json::from_value::<VoiceMode>(json!("unknown")).is_err());
    }
}
