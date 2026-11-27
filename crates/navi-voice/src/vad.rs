//! PCM16 energy measurement, adaptive voice activity detection and utterance
//! endpointing ported from `apps/navi/workers/voice_stream.py`.
//!
//! The Python `AdaptiveVad` calibrates a noise floor from a low percentile of
//! recent block RMS values and gates each block at `floor * factor`, never
//! below a hardware minimum. `CaptureBuffer` then turns the quiet/speech
//! decision into an utterance: it keeps a pre-roll so a command starts at its
//! first syllable, waits for a minimum run of speech before starting, and ends
//! only after a run of silence. This module keeps those decisions together in
//! [`VadEndpointDetector`], a three-state machine that emits [`VadEvent`]s.

use navi_types::NaviResult;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Divisor that maps the full PCM16 range onto `[-1, 1]`.
const PCM16_SCALE: f64 = 32768.0;

const DEFAULT_FLOOR: f64 = 0.004;
const DEFAULT_MINIMUM: f64 = 0.001;
const DEFAULT_FACTOR: f64 = 3.0;
const DEFAULT_CALIBRATE_BLOCKS: usize = 10;
const DEFAULT_WINDOW_BLOCKS: usize = 300;
const DEFAULT_PERCENTILE: f64 = 0.1;
const DEFAULT_CEILING: f64 = 0.05;
const DEFAULT_PRE_ROLL_FRAMES: usize = 10;
const DEFAULT_MIN_SPEECH_FRAMES: usize = 2;
const DEFAULT_HANGOVER_FRAMES: usize = 40;

/// Computes the root-mean-square level of PCM16 samples, normalised to `[0, 1]`.
///
/// Each sample is scaled by `1 / 32768` before squaring, so digital silence is
/// `0.0` and a full-scale square wave is `1.0`. The normalised range is what the
/// Python noise-floor defaults (`floor = 0.004`, `minimum = 0.001`, `ceiling =
/// 0.05`) are expressed in. An empty slice yields `0.0`, matching Python's
/// `max(len(samples), 1)` divisor.
pub fn pcm16_rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|&sample| {
            let scaled = f64::from(sample) / PCM16_SCALE;
            scaled * scaled
        })
        .sum();
    (sum / samples.len() as f64).sqrt()
}

/// Decodes little-endian PCM16 bytes into samples.
///
/// Audio frames are always pairs of bytes. A trailing odd byte cannot form a
/// complete sample and is ignored, so byte input from a device is never a
/// fallible operation.
pub fn pcm16_bytes_to_samples(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| i16::from_le_bytes(pair))
        .collect()
}

/// Tuning for [`VadEndpointDetector`].
///
/// The first seven fields mirror `AdaptiveVad` in the Python source. The last
/// three control endpointing: `min_speech_frames` is the run of speech frames
/// required before onset, `silence_hangover_frames` is the run of quiet frames
/// that closes an utterance, and `pre_roll_frames` bounds how much audio before
/// onset is kept for the emitted utterance.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct VadConfig {
    /// Initial noise floor, used until calibration completes.
    pub floor: f64,
    /// Absolute lower bound for the noise floor and the speech threshold.
    pub minimum: f64,
    /// Multiplier applied to the calibrated floor to get the speech threshold.
    pub factor: f64,
    /// Number of RMS observations needed before the floor is calibrated.
    pub calibrate_blocks: usize,
    /// Number of RMS observations kept for percentile estimation.
    pub window_blocks: usize,
    /// Percentile of recent energy used as the quiet floor (0.0 to 1.0).
    pub percentile: f64,
    /// Upper bound for the calibrated noise floor.
    pub ceiling: f64,
    /// Maximum number of frames kept before speech starts.
    pub pre_roll_frames: usize,
    /// Consecutive speech frames required before emitting `SpeechStart`.
    pub min_speech_frames: usize,
    /// Consecutive quiet frames required before emitting `UtteranceEnd`.
    pub silence_hangover_frames: usize,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            floor: DEFAULT_FLOOR,
            minimum: DEFAULT_MINIMUM,
            factor: DEFAULT_FACTOR,
            calibrate_blocks: DEFAULT_CALIBRATE_BLOCKS,
            window_blocks: DEFAULT_WINDOW_BLOCKS,
            percentile: DEFAULT_PERCENTILE,
            ceiling: DEFAULT_CEILING,
            pre_roll_frames: DEFAULT_PRE_ROLL_FRAMES,
            min_speech_frames: DEFAULT_MIN_SPEECH_FRAMES,
            silence_hangover_frames: DEFAULT_HANGOVER_FRAMES,
        }
    }
}

impl VadConfig {
    /// Parses a configuration from JSON, using defaults for omitted fields.
    ///
    /// Malformed JSON and wrong field types are reported instead of partially
    /// applied; callers can fall back to [`Self::default`].
    pub fn from_json(text: &str) -> NaviResult<Self> {
        let config: Self = serde_json::from_str(text)?;
        Ok(config)
    }

    /// Clamps values that would make the detector undefined or unusable.
    fn sanitized(self) -> Self {
        let finite_or = |value: f64, default: f64| {
            if value.is_finite() { value } else { default }
        };
        let floor = finite_or(self.floor, DEFAULT_FLOOR).max(0.0);
        let minimum = finite_or(self.minimum, DEFAULT_MINIMUM).max(0.0);
        let calibrate_blocks = self.calibrate_blocks.max(1);
        Self {
            floor,
            minimum,
            factor: finite_or(self.factor, DEFAULT_FACTOR).max(1.0),
            calibrate_blocks,
            window_blocks: self.window_blocks.max(calibrate_blocks),
            percentile: finite_or(self.percentile, DEFAULT_PERCENTILE).clamp(0.0, 1.0),
            ceiling: finite_or(self.ceiling, DEFAULT_CEILING).max(minimum),
            pre_roll_frames: self.pre_roll_frames,
            min_speech_frames: self.min_speech_frames,
            silence_hangover_frames: self.silence_hangover_frames,
        }
    }
}

/// Phase of the endpointing state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VadState {
    /// No speech is being captured; frames feed the pre-roll ring.
    #[default]
    Silence,
    /// Speech is being captured.
    Speech,
    /// Speech stopped, but the hang-over window has not elapsed yet.
    TrailingSilence,
}

/// One frame's contribution to an utterance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VadEvent {
    /// A run of speech reached `min_speech_frames` and a capture began.
    SpeechStart,
    /// Another frame of speech (or a resumption) arrived during a capture.
    SpeechContinue,
    /// The hang-over window elapsed; the payload is the captured utterance.
    UtteranceEnd(Vec<i16>),
}

/// Adaptive speech gate with pre-roll and silence-based endpointing.
///
/// Feed one fixed-size audio frame per call with [`Self::feed`]. Each call
/// returns at most one event: `None` during silence and before onset, then a
/// `SpeechStart`, zero or more `SpeechContinue`, and finally an `UtteranceEnd`
/// carrying the pre-roll, speech frames and trailing silence together. A quiet
/// frame that restarts speech inside the hang-over window continues the same
/// utterance. The noise floor stays calibrated between utterances; only
/// [`Self::reset`] clears the learned room level.
#[derive(Debug, Clone)]
pub struct VadEndpointDetector {
    config: VadConfig,
    floor: f64,
    recent: VecDeque<f64>,
    calibrated: bool,
    state: VadState,
    pre_roll: VecDeque<Vec<i16>>,
    utterance: Vec<i16>,
    /// Frames of the current run of speech frames that has not yet reached
    /// `min_speech_frames`.
    speech_candidate: Vec<Vec<i16>>,
    trailing_frames: usize,
}

impl Default for VadEndpointDetector {
    fn default() -> Self {
        Self::new(VadConfig::default())
    }
}

impl VadEndpointDetector {
    /// Creates a detector with the supplied configuration, clamping invalid values.
    pub fn new(config: VadConfig) -> Self {
        let config = config.sanitized();
        let floor = config.floor;
        Self {
            config,
            floor,
            recent: VecDeque::new(),
            calibrated: false,
            state: VadState::Silence,
            pre_roll: VecDeque::new(),
            utterance: Vec::new(),
            speech_candidate: Vec::new(),
            trailing_frames: 0,
        }
    }

    /// Feeds one PCM16 frame and returns the event it caused, if any.
    ///
    /// The gate is evaluated against the current threshold before the frame's
    /// energy is added to the noise-floor window, matching the Python order of
    /// operations. An empty frame is treated as digital silence.
    pub fn feed(&mut self, samples: &[i16]) -> Option<VadEvent> {
        let rms = pcm16_rms(samples);
        let speech = rms > self.threshold();
        self.update_floor(rms);

        match self.state {
            VadState::Silence => {
                if !speech {
                    self.speech_candidate.clear();
                    self.push_pre_roll(samples);
                    return None;
                }
                self.speech_candidate.push(samples.to_vec());
                if self.speech_candidate.len() < self.config.min_speech_frames {
                    return None;
                }
                // The run that started the utterance lives in its own buffer,
                // so no pre-roll size can drop part of it.
                self.utterance = self.pre_roll.drain(..).flatten().collect();
                self.utterance
                    .extend(self.speech_candidate.drain(..).flatten());
                self.state = VadState::Speech;
                self.trailing_frames = 0;
                Some(VadEvent::SpeechStart)
            }
            VadState::Speech => {
                self.utterance.extend_from_slice(samples);
                if speech {
                    self.trailing_frames = 0;
                    Some(VadEvent::SpeechContinue)
                } else {
                    self.trailing_frames = 1;
                    self.state = VadState::TrailingSilence;
                    self.end_if_hangover_elapsed()
                }
            }
            VadState::TrailingSilence => {
                self.utterance.extend_from_slice(samples);
                if speech {
                    self.trailing_frames = 0;
                    self.state = VadState::Speech;
                    Some(VadEvent::SpeechContinue)
                } else {
                    self.trailing_frames += 1;
                    self.end_if_hangover_elapsed()
                }
            }
        }
    }

    /// Feeds little-endian PCM16 bytes, decoding them before the gate.
    ///
    /// An odd trailing byte is ignored by [`pcm16_bytes_to_samples`].
    pub fn feed_bytes(&mut self, bytes: &[u8]) -> Option<VadEvent> {
        self.feed(&pcm16_bytes_to_samples(bytes))
    }

    /// Ends an in-flight utterance when the stream stops, if there is one.
    pub fn finish(&mut self) -> Option<VadEvent> {
        if self.state == VadState::Silence {
            return None;
        }
        Some(self.end_utterance())
    }

    /// Returns the current endpointing phase.
    pub fn state(&self) -> VadState {
        self.state
    }

    /// Returns the current calibrated noise floor.
    pub fn noise_floor(&self) -> f64 {
        self.floor
    }

    /// Returns whether the noise floor has been calibrated from observations.
    pub fn is_calibrated(&self) -> bool {
        self.calibrated
    }

    /// Returns the RMS level above which a frame counts as speech.
    ///
    /// During calibration the Python source uses the floor itself so a loud
    /// room does not swallow the first utterance; afterwards it uses
    /// `floor * factor`, never below the hardware minimum.
    pub fn threshold(&self) -> f64 {
        if self.calibrated {
            (self.floor * self.config.factor).max(self.config.minimum)
        } else {
            self.floor.max(self.config.minimum)
        }
    }

    /// Returns the detector to its initial state, forgetting the learned floor.
    pub fn reset(&mut self) {
        self.clear_utterance();
        self.floor = self.config.floor;
        self.recent.clear();
        self.calibrated = false;
    }

    fn push_pre_roll(&mut self, samples: &[i16]) {
        if self.config.pre_roll_frames == 0 {
            return;
        }
        if self.pre_roll.len() == self.config.pre_roll_frames {
            self.pre_roll.pop_front();
        }
        self.pre_roll.push_back(samples.to_vec());
    }

    fn update_floor(&mut self, rms: f64) {
        if self.recent.len() == self.config.window_blocks {
            self.recent.pop_front();
        }
        self.recent.push_back(rms);
        if self.recent.len() < self.config.calibrate_blocks {
            return;
        }
        let mut ordered: Vec<f64> = self.recent.iter().copied().collect();
        ordered.sort_unstable_by(f64::total_cmp);
        let index = ((ordered.len() as f64) * self.config.percentile) as usize;
        let quiet = ordered[index.min(ordered.len() - 1)];
        self.floor = self.config.minimum.max(quiet.min(self.config.ceiling));
        self.calibrated = true;
    }

    fn end_if_hangover_elapsed(&mut self) -> Option<VadEvent> {
        if self.trailing_frames >= self.config.silence_hangover_frames {
            Some(self.end_utterance())
        } else {
            None
        }
    }

    fn end_utterance(&mut self) -> VadEvent {
        let audio = std::mem::take(&mut self.utterance);
        self.clear_utterance();
        VadEvent::UtteranceEnd(audio)
    }

    fn clear_utterance(&mut self) {
        self.state = VadState::Silence;
        self.pre_roll.clear();
        self.utterance.clear();
        self.speech_candidate.clear();
        self.trailing_frames = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: i16, len: usize) -> Vec<i16> {
        vec![value; len]
    }

    #[test]
    fn pcm16_rms_normalises_samples_and_handles_empty() {
        assert_eq!(pcm16_rms(&[]), 0.0);
        assert_eq!(pcm16_rms(&[0, 0, 0]), 0.0);
        let full = pcm16_rms(&[i16::MIN, i16::MIN]);
        assert!((full - 1.0).abs() < 1e-12, "full-scale RMS was {full}");
        let half = pcm16_rms(&[16_384, -16_384]);
        assert!((half - 0.5).abs() < 1e-12, "half-scale RMS was {half}");
        let mixed = pcm16_rms(&[0, -10_000, 10_000]);
        assert!((0.0..1.0).contains(&mixed));
    }

    #[test]
    fn pcm16_bytes_to_samples_decodes_little_endian_and_ignores_odd_tail() {
        let bytes = [0x00, 0x00, 0xFF, 0x7F, 0x00, 0x80, 0x01, 0x02, 0xAA];
        assert_eq!(
            pcm16_bytes_to_samples(&bytes),
            vec![0, i16::MAX, i16::MIN, 0x0201]
        );
        assert!(pcm16_bytes_to_samples(&[]).is_empty());
        assert_eq!(pcm16_bytes_to_samples(&[0x01]), Vec::<i16>::new());
    }

    #[test]
    fn config_defaults_and_json_roundtrip() {
        let cfg = VadConfig::default();
        assert!((cfg.floor - 0.004).abs() < 1e-12);
        assert!((cfg.factor - 3.0).abs() < 1e-12);
        assert_eq!(cfg.calibrate_blocks, 10);
        assert_eq!(cfg.window_blocks, 300);
        assert_eq!(cfg.silence_hangover_frames, 40);
        let json = serde_json::to_string(&cfg).expect("serialize config");
        let parsed: VadConfig = serde_json::from_str(&json).expect("parse config");
        assert_eq!(parsed, cfg);
        let partial = VadConfig::from_json(r#"{"floor":0.02,"min_speech_frames":3}"#)
            .expect("partial config");
        assert!((partial.floor - 0.02).abs() < 1e-12);
        assert_eq!(partial.min_speech_frames, 3);
        assert_eq!(partial.silence_hangover_frames, cfg.silence_hangover_frames);
        assert!(VadConfig::from_json("not json").is_err());
    }

    #[test]
    fn detector_emits_start_continue_and_end() {
        let config = VadConfig {
            calibrate_blocks: 100,
            min_speech_frames: 2,
            silence_hangover_frames: 3,
            pre_roll_frames: 4,
            ..VadConfig::default()
        };
        let mut detector = VadEndpointDetector::new(config);
        let quiet = frame(0, 16);
        let speech = frame(8_000, 16);

        for _ in 0..3 {
            assert_eq!(detector.feed(&quiet), None);
        }
        assert_eq!(detector.state(), VadState::Silence);
        assert_eq!(detector.feed(&speech), None);
        assert_eq!(detector.feed(&speech), Some(VadEvent::SpeechStart));
        assert_eq!(detector.state(), VadState::Speech);
        assert_eq!(detector.feed(&speech), Some(VadEvent::SpeechContinue));
        assert_eq!(detector.feed(&quiet), None);
        assert_eq!(detector.state(), VadState::TrailingSilence);
        assert_eq!(detector.feed(&quiet), None);

        match detector.feed(&quiet) {
            Some(VadEvent::UtteranceEnd(samples)) => assert_eq!(samples.len(), 9 * 16),
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(detector.state(), VadState::Silence);
    }

    #[test]
    fn pre_roll_captures_audio_before_onset() {
        let config = VadConfig {
            calibrate_blocks: 100,
            pre_roll_frames: 3,
            min_speech_frames: 1,
            silence_hangover_frames: 2,
            ..VadConfig::default()
        };
        let mut detector = VadEndpointDetector::new(config);
        for value in [1_i16, 2, 3, 4, 5] {
            assert_eq!(detector.feed(&[value]), None);
        }
        assert_eq!(detector.feed(&[10_000]), Some(VadEvent::SpeechStart));
        assert_eq!(detector.feed(&[0]), None);
        match detector.feed(&[0]) {
            Some(VadEvent::UtteranceEnd(samples)) => {
                assert_eq!(samples, vec![3, 4, 5, 10_000, 0, 0]);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn onset_run_is_captured_once_including_every_candidate_frame() {
        let mut detector = VadEndpointDetector::new(VadConfig {
            calibrate_blocks: 100,
            pre_roll_frames: 2,
            min_speech_frames: 3,
            silence_hangover_frames: 2,
            ..VadConfig::default()
        });
        assert_eq!(detector.feed(&[1]), None);
        assert_eq!(detector.feed(&[2]), None);
        assert_eq!(detector.feed(&[3]), None);
        assert_eq!(detector.feed(&[7_000]), None);
        assert_eq!(detector.feed(&[7_000]), None);
        assert_eq!(detector.feed(&[7_000]), Some(VadEvent::SpeechStart));
        assert_eq!(detector.feed(&[0]), None);
        match detector.feed(&[0]) {
            Some(VadEvent::UtteranceEnd(samples)) => {
                // The pre-roll holds the last two quiet frames; the three
                // speech frames that started the utterance follow, each once.
                assert_eq!(samples, vec![2, 3, 7_000, 7_000, 7_000, 0, 0]);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn min_speech_run_survives_a_disabled_pre_roll() {
        let mut detector = VadEndpointDetector::new(VadConfig {
            calibrate_blocks: 100,
            pre_roll_frames: 0,
            min_speech_frames: 2,
            silence_hangover_frames: 1,
            ..VadConfig::default()
        });
        assert_eq!(detector.feed(&[9_000]), None);
        assert_eq!(detector.feed(&[9_000]), Some(VadEvent::SpeechStart));
        match detector.feed(&[0]) {
            Some(VadEvent::UtteranceEnd(samples)) => {
                assert_eq!(samples, vec![9_000, 9_000, 0]);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn adaptive_floor_learns_room_and_resists_speech() {
        let mut detector = VadEndpointDetector::new(VadConfig {
            calibrate_blocks: 10,
            window_blocks: 300,
            percentile: 0.1,
            ceiling: 0.05,
            ..VadConfig::default()
        });
        assert!(!detector.is_calibrated());
        for _ in 0..10 {
            detector.update_floor(0.01);
        }
        assert!(detector.is_calibrated());
        assert!((detector.noise_floor() - 0.01).abs() < 1e-9);
        assert!((detector.threshold() - 0.03).abs() < 1e-9);

        // A loud run cannot dominate the low percentile and raise the floor.
        for _ in 0..10 {
            detector.update_floor(0.4);
        }
        for _ in 0..90 {
            detector.update_floor(0.01);
        }
        assert!((detector.noise_floor() - 0.01).abs() < 1e-9);

        // A lasting room change moves the floor, bounded by the ceiling.
        for _ in 0..300 {
            detector.update_floor(0.2);
        }
        assert!((detector.noise_floor() - 0.05).abs() < 1e-9);
    }

    #[test]
    fn min_speech_frames_suppress_single_frame_clicks() {
        let mut detector = VadEndpointDetector::new(VadConfig {
            calibrate_blocks: 100,
            min_speech_frames: 3,
            silence_hangover_frames: 4,
            ..VadConfig::default()
        });
        let speech = frame(12_000, 8);
        assert_eq!(detector.feed(&speech), None);
        assert_eq!(detector.state(), VadState::Silence);
        assert_eq!(detector.feed(&[0]), None);
        assert_eq!(detector.feed(&speech), None);
        assert_eq!(detector.feed(&speech), None);
        assert_eq!(detector.feed(&speech), Some(VadEvent::SpeechStart));
    }

    #[test]
    fn speech_during_hangover_continues_the_utterance() {
        let mut detector = VadEndpointDetector::new(VadConfig {
            calibrate_blocks: 100,
            min_speech_frames: 1,
            silence_hangover_frames: 3,
            ..VadConfig::default()
        });
        let speech = frame(9_000, 8);
        assert_eq!(detector.feed(&speech), Some(VadEvent::SpeechStart));
        assert_eq!(detector.feed(&[0]), None);
        assert_eq!(detector.state(), VadState::TrailingSilence);
        assert_eq!(detector.feed(&speech), Some(VadEvent::SpeechContinue));
        assert_eq!(detector.state(), VadState::Speech);
        assert_eq!(detector.feed(&[0]), None);
        assert_eq!(detector.feed(&[0]), None);
        assert!(matches!(
            detector.feed(&[0]),
            Some(VadEvent::UtteranceEnd(_))
        ));
    }

    #[test]
    fn finish_flushes_and_reset_clears_calibration() {
        let mut detector = VadEndpointDetector::new(VadConfig {
            calibrate_blocks: 10,
            min_speech_frames: 1,
            ..VadConfig::default()
        });
        assert_eq!(detector.finish(), None);
        assert_eq!(detector.feed(&[10_000]), Some(VadEvent::SpeechStart));
        for _ in 0..10 {
            detector.update_floor(0.02);
        }
        assert!(detector.is_calibrated());
        match detector.finish() {
            Some(VadEvent::UtteranceEnd(samples)) => assert_eq!(samples, vec![10_000]),
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(detector.state(), VadState::Silence);
        assert!(
            detector.is_calibrated(),
            "normal endpoint keeps learned floor"
        );
        detector.reset();
        assert!(!detector.is_calibrated());
        assert!((detector.noise_floor() - 0.004).abs() < 1e-12);
        assert_eq!(detector.feed(&[0]), None);
    }

    #[test]
    fn invalid_config_values_cannot_poison_detector() {
        let mut detector = VadEndpointDetector::new(VadConfig {
            floor: f64::NAN,
            minimum: -1.0,
            factor: 0.0,
            percentile: 2.0,
            ceiling: f64::INFINITY,
            calibrate_blocks: 0,
            window_blocks: 0,
            pre_roll_frames: 2,
            min_speech_frames: 0,
            silence_hangover_frames: 0,
        });
        assert!(detector.threshold().is_finite());
        assert!(detector.noise_floor().is_finite());
        assert_eq!(detector.feed(&[10_000]), Some(VadEvent::SpeechStart));
        assert!(matches!(
            detector.feed(&[0]),
            Some(VadEvent::UtteranceEnd(_))
        ));
        assert_eq!(detector.state(), VadState::Silence);
        assert_eq!(detector.feed_bytes(&[0x01]), None);
    }

    #[test]
    fn event_and_state_serde_use_snake_case() {
        assert_eq!(
            serde_json::to_string(&VadState::TrailingSilence).expect("serialize state"),
            "\"trailing_silence\""
        );
        assert_eq!(
            serde_json::to_string(&VadEvent::SpeechContinue).expect("serialize event"),
            "\"speech_continue\""
        );
        let event: VadEvent =
            serde_json::from_str(r#"{"utterance_end":[1,-2]}"#).expect("parse event");
        assert_eq!(event, VadEvent::UtteranceEnd(vec![1, -2]));
    }
}
