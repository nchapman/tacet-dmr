//! Pluggable audio output sink abstraction and OS backend dispatch.

pub mod null;

#[cfg(target_os = "macos")]
pub mod coreaudio;

#[cfg(target_os = "linux")]
pub mod alsa;

#[cfg(target_os = "windows")]
pub mod wasapi;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use rtrb::Consumer;

use crate::format::{PcmSpec, SampleFormat};

/// Descriptor for an available OS audio output device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDevice {
    pub id: String,
    pub description: String,
    pub is_default: bool,
}

/// Active hardware output configuration negotiated by the sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveOutput {
    pub device_id: String,
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
    pub exclusive: bool,
    pub hardware_volume: bool,
}

/// Lock-free state shared between the decode engine and the real-time audio
/// sink callback / output thread.
pub struct StreamShared {
    pub paused: AtomicBool,
    pub draining: AtomicBool,
    pub finished: AtomicBool,
    pub failed: AtomicBool,
    pub stop: AtomicBool,
    pub flush_req: AtomicU64,
    pub flush_ack: AtomicU64,
    pub gain_q16: AtomicI64,
    pub frames_played: AtomicU64,
    pub underruns: AtomicU64,
}

impl StreamShared {
    pub fn new(paused: bool, gain_q16: i64) -> Self {
        Self {
            paused: AtomicBool::new(paused),
            draining: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            flush_req: AtomicU64::new(0),
            flush_ack: AtomicU64::new(0),
            gain_q16: AtomicI64::new(gain_q16),
            frames_played: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
        }
    }

    /// If a seek flush was requested, discard all currently queued samples in
    /// `consumer` and publish the acknowledged generation with Release ordering
    /// so back-to-back flush requests can never be lost.
    #[inline]
    pub fn maybe_flush(&self, consumer: &mut Consumer<i32>) {
        let req = self.flush_req.load(Ordering::Acquire);
        if req != self.flush_ack.load(Ordering::Relaxed) {
            let n = consumer.slots();
            if n > 0
                && let Ok(chunk) = consumer.read_chunk(n)
            {
                chunk.commit_all();
            }
            self.flush_ack.store(req, Ordering::Release);
        }
    }

    #[inline]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    #[inline]
    pub fn gain_q16(&self) -> i64 {
        self.gain_q16.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn frames_played(&self) -> u64 {
        self.frames_played.load(Ordering::Relaxed)
    }
}

/// Running output stream handle. Dropping it stops audio output and releases
/// the OS audio device (including macOS hog mode / WASAPI exclusive lock).
pub trait OutputStream: Send {
    fn active_output(&self) -> &ActiveOutput;
}

/// Optional OS-level hardware volume/mute controller for an audio target.
pub trait HardwareVolume: Send + Sync {
    fn get_volume(&self) -> Result<u8, String>;
    fn set_volume(&self, volume: u8) -> Result<(), String>;
    fn get_mute(&self) -> Result<bool, String>;
    fn set_mute(&self, muted: bool) -> Result<(), String>;
}

/// Validate an `-audio-device` target string at startup.
pub fn validate_target(target: &str) -> Result<(), String> {
    let target = target.trim();
    if target.is_empty() || target == "auto" || target == "null" || target.starts_with("null/") {
        return Ok(());
    }
    if let Some(rest) = target.strip_prefix("pcm/") {
        if rest.is_empty() {
            return Err("pcm/ target requires a file path".into());
        }
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        if target == "coreaudio"
            || target.starts_with("coreaudio/")
            || target == "coreaudio_exclusive"
            || target.starts_with("coreaudio_exclusive/")
        {
            return Ok(());
        }
    }
    #[cfg(target_os = "linux")]
    {
        if target == "alsa" || target.starts_with("alsa/") {
            return Ok(());
        }
    }
    #[cfg(target_os = "windows")]
    {
        if target == "wasapi" || target.starts_with("wasapi/") {
            return Ok(());
        }
    }
    Err(format!(
        "unsupported audio target {target:?} on {}",
        std::env::consts::OS
    ))
}

/// Report whether a target + flag combination implies exclusive hardware output.
pub fn is_exclusive_target(target: &str, exclusive: bool) -> bool {
    if exclusive {
        return true;
    }
    let target = target.trim();
    target == "coreaudio_exclusive"
        || target.starts_with("coreaudio_exclusive/")
        || target.starts_with("alsa/hw:")
}

/// List available OS audio output devices (plus `auto` and `null`).
pub fn list_devices() -> Vec<OutputDevice> {
    let mut out = vec![OutputDevice {
        id: "auto".into(),
        description: "System default output".into(),
        is_default: true,
    }];

    #[cfg(target_os = "macos")]
    {
        out.extend(coreaudio::list_devices());
    }
    #[cfg(target_os = "linux")]
    {
        out.extend(alsa::list_devices());
    }
    #[cfg(target_os = "windows")]
    {
        out.extend(wasapi::list_devices());
    }

    out.push(OutputDevice {
        id: "null".into(),
        description: "Null output (discard audio, real-time paced)".into(),
        is_default: false,
    });
    out
}

/// Open an audio output stream for `target` and `spec`.
pub fn open_stream(
    target: &str,
    exclusive: bool,
    spec: PcmSpec,
    consumer: Consumer<i32>,
    shared: Arc<StreamShared>,
) -> Result<Box<dyn OutputStream>, String> {
    let target = target.trim();
    if target == "null" || target.starts_with("null/") {
        return Ok(Box::new(null::NullStream::open(spec, consumer, shared)));
    }
    if let Some(path) = target.strip_prefix("pcm/") {
        return Ok(Box::new(null::PcmFileStream::open(
            path, spec, consumer, shared,
        )?));
    }

    #[cfg(target_os = "macos")]
    {
        let (uid, want_exclusive) = if let Some(uid) = target.strip_prefix("coreaudio_exclusive/") {
            (Some(uid), true)
        } else if target == "coreaudio_exclusive" {
            (None, true)
        } else if let Some(uid) = target.strip_prefix("coreaudio/") {
            (Some(uid), exclusive)
        } else if target.is_empty() || target == "auto" || target == "coreaudio" {
            (None, exclusive)
        } else {
            return Err(format!("unsupported macOS audio target {target:?}"));
        };
        coreaudio::open_stream(uid, want_exclusive, spec, consumer, shared)
    }

    #[cfg(target_os = "linux")]
    {
        let name = if let Some(n) = target.strip_prefix("alsa/") {
            n
        } else if target.is_empty() || target == "auto" || target == "alsa" {
            "default"
        } else {
            return Err(format!("unsupported Linux audio target {target:?}"));
        };
        alsa::open_stream(name, exclusive, spec, consumer, shared)
    }

    #[cfg(target_os = "windows")]
    {
        let id = if let Some(n) = target.strip_prefix("wasapi/") {
            Some(n)
        } else if target.is_empty() || target == "auto" || target == "wasapi" {
            None
        } else {
            return Err(format!("unsupported Windows audio target {target:?}"));
        };
        wasapi::open_stream(id, exclusive, spec, consumer, shared)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (exclusive, spec, consumer, shared);
        Err(format!(
            "no native audio backend on {}",
            std::env::consts::OS
        ))
    }
}

/// Create a hardware volume controller for `target` if supported on this OS.
pub fn open_hardware_volume(target: &str, exclusive: bool) -> Option<Arc<dyn HardwareVolume>> {
    let target = target.trim();
    if target == "null" || target.starts_with("null/") || target.starts_with("pcm/") {
        return None;
    }

    #[cfg(target_os = "macos")]
    {
        let uid = target
            .strip_prefix("coreaudio_exclusive/")
            .or_else(|| target.strip_prefix("coreaudio/"));
        let _ = exclusive;
        coreaudio::open_hardware_volume(uid).map(|v| Arc::new(v) as Arc<dyn HardwareVolume>)
    }

    #[cfg(target_os = "linux")]
    {
        let _ = (target, exclusive);
        None
    }

    #[cfg(target_os = "windows")]
    {
        let id = target.strip_prefix("wasapi/");
        let _ = exclusive;
        wasapi::open_hardware_volume(id).map(|v| Arc::new(v) as Arc<dyn HardwareVolume>)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = (target, exclusive);
        None
    }
}
