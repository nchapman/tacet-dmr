//! `tacet-audio` — Cross-platform audiophile playback engine.
//!
//! Designed for both the standalone `tacet-endpoint` OpenHome + UPnP/DLNA
//! renderer and direct in-process embedding inside Tauri desktop applications.

pub mod decoder;
pub mod engine;
pub mod format;
pub mod sink;
pub mod source;

pub use decoder::TrackInfo;
pub use engine::{
    EXCLUSIVE_SAFE_VOLUME, EngineConfig, PcmStreamHandle, PcmTap, PlaybackEvent, Player, Position,
    Status,
};
pub use format::{DriftResampler, PcmSpec, SampleFormat, pcm_be_into_q31, q31_into_pcm_be};
pub use sink::{ActiveOutput, OutputDevice, is_exclusive_target, list_devices, validate_target};
