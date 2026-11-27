//! Team 3: Voice, Speech Config, VAD/Endpointing, STT, TTS & Voice Pipeline
//! (ported from `apps/navi/workers/{speech_config,speech_out,voice_stream,cloud_stt,hex_asr,tts,voice}.py`).

pub mod speech_config;
pub mod speech_out;
pub mod stt;
pub mod tts;
pub mod vad;
pub mod voice_pipeline;

pub use navi_types::{NaviError, NaviResult};
