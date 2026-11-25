//! `tacet-audio` — Cross-platform audiophile playback engine.
//!
//! Designed for both the standalone `tacet-dmr` UPnP/LinkPlay renderer and
//! direct in-process embedding inside Tauri desktop applications.

pub mod decoder;
pub mod engine;
pub mod format;
pub mod sink;
pub mod source;

pub use engine::{EngineConfig, PlaybackEvent, Player, Position, Status};
pub use format::{PcmSpec, SampleFormat};
pub use sink::{ActiveOutput, OutputDevice, is_exclusive_target, list_devices, validate_target};
