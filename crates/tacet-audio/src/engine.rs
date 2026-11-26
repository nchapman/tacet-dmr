//! High-level audio playback engine with gapless transitions, seeking,
//! lock-free PCM buffering, and hardware/software volume control.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rtrb::{Producer, RingBuffer};

use crate::decoder::{AudioDecoder, TrackInfo};
use crate::format::{PcmSpec, volume_gain_q16};
use crate::sink::{self, ActiveOutput, HardwareVolume, OutputStream, StreamShared};

/// Configuration for creating a [`Player`].
/// Safe default volume cap (0..=100) applied on startup in exclusive mode
/// when no explicit initial volume is requested, so a USB DAC left at 100%
/// never blasts full-scale audio into headphones or speakers.
pub const EXCLUSIVE_SAFE_VOLUME: u8 = 30;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Target audio device (e.g. `"auto"`, `"null"`, `"pcm/out.raw"`,
    /// `"coreaudio/<uid>"`, `"alsa/hw:0,0"`, `"wasapi/<id>"`).
    pub audio_device: String,
    /// Request exclusive hardware mode (macOS CoreAudio hog mode, Linux ALSA
    /// `hw:`, Windows WASAPI Exclusive).
    pub exclusive: bool,
    /// Optional explicit initial software/hardware volume (0..=100).
    /// When `None`, shared mode preserves hardware volume (or 100% software)
    /// and exclusive mode caps hardware volume at [`EXCLUSIVE_SAFE_VOLUME`]
    /// (or starts software volume at [`EXCLUSIVE_SAFE_VOLUME`]).
    pub initial_volume: Option<u8>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            audio_device: "auto".into(),
            exclusive: false,
            initial_volume: None,
        }
    }
}

/// Optional pre-gain PCM tap for rebroadcasting decoded playback audio
/// (e.g. OpenHome Songcast `Sender:1`) before local volume scaling is applied.
pub trait PcmTap: Send + Sync {
    /// Return `true` only when downstream listeners are actively consuming audio.
    /// When `false`, the decode worker skips packing/pacing overhead entirely.
    fn is_active(&self) -> bool;

    /// Receive a chunk of full-scale interleaved Q31 `i32` samples.
    fn on_pcm(&self, spec: PcmSpec, info: &TrackInfo, samples: &[i32]);

    /// Notify downstream listeners that playback paused, seeked, or stopped.
    fn on_halt(&self);
}

type SharedOutputStream = Arc<Mutex<Option<Box<dyn OutputStream>>>>;

/// Handle for pushing live interleaved Q31 `i32` PCM samples directly into the
/// audio output sink (e.g. from an OpenHome Songcast `Receiver:1` stream).
///
/// Shares the player's exclusive output mode, hardware volume, and mute state.
pub struct PcmStreamHandle {
    spec: PcmSpec,
    active: ActiveOutput,
    capacity_frames: usize,
    producer: Producer<i32>,
    shared: Arc<StreamShared>,
    cancel: Arc<AtomicBool>,
    stream: SharedOutputStream,
}

impl PcmStreamHandle {
    pub fn spec(&self) -> PcmSpec {
        self.spec
    }

    pub fn active_output(&self) -> &ActiveOutput {
        &self.active
    }

    /// Ringbuffer capacity in audio frames.
    pub fn capacity_frames(&self) -> usize {
        self.capacity_frames
    }

    /// Approximate number of frames currently buffered awaiting playback.
    pub fn buffered_frames(&self) -> usize {
        let ch = self.spec.channels.max(1) as usize;
        let free_frames = self.producer.slots() / ch;
        self.capacity_frames.saturating_sub(free_frames)
    }

    /// Total number of sink underrun callbacks observed on this stream.
    pub fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire) || self.shared.is_stopped()
    }

    pub fn is_failed(&self) -> bool {
        self.shared.is_failed()
    }

    /// Write interleaved Q31 `i32` samples to the output ringbuffer, blocking
    /// briefly for backpressure while honoring cancellation or device failure.
    pub fn write_q31(&mut self, samples: &[i32]) -> Result<(), String> {
        let ch = self.spec.channels.max(1) as usize;
        let aligned = (samples.len() / ch) * ch;
        let mut off = 0;
        while off < aligned {
            if self.is_cancelled() {
                return Err("stream cancelled".into());
            }
            if self.shared.is_failed() {
                return Err("audio output device failed or disconnected".into());
            }
            let written = push_samples(&mut self.producer, &samples[off..aligned], ch);
            if written == 0 {
                thread::sleep(Duration::from_millis(2));
            } else {
                off += written;
            }
        }
        Ok(())
    }

    /// Write `frames` of silence into the output ringbuffer (e.g. Songcast pre-buffer).
    pub fn write_silence_frames(&mut self, frames: usize) -> Result<(), String> {
        let ch = self.spec.channels.max(1) as usize;
        let total = frames.saturating_mul(ch);
        if total == 0 {
            return Ok(());
        }
        let zeros = vec![0i32; total.min(4096 * ch)];
        let mut rem = total;
        while rem > 0 {
            let chunk = rem.min(zeros.len());
            self.write_q31(&zeros[..chunk])?;
            rem -= chunk;
        }
        Ok(())
    }

    /// Discard any buffered samples on the output sink immediately.
    pub fn flush(&self) {
        wait_for_flush(&self.shared, &self.cancel);
    }
}

impl Drop for PcmStreamHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.shared.stop.store(true, Ordering::Release);
        let _ = self.stream.lock().unwrap().take();
    }
}

/// Asynchronous playback lifecycle events emitted by [`Player`].
#[derive(Debug, Clone)]
pub enum PlaybackEvent {
    /// First track opened and audio output stream started.
    Ready {
        session_id: u64,
        url: String,
        spec: PcmSpec,
        duration_secs: Option<f64>,
        codec: String,
        lossless: bool,
        active: ActiveOutput,
    },
    /// Playback seamlessly transitioned to the queued next track.
    TrackChanged {
        session_id: u64,
        url: String,
        spec: PcmSpec,
        duration_secs: Option<f64>,
        codec: String,
        lossless: bool,
        active: ActiveOutput,
    },
    /// Playback reached the end of the current track with no queued next track.
    TrackEnded { session_id: u64 },
    /// Unrecoverable playback error.
    Error { session_id: u64, message: String },
}

impl PlaybackEvent {
    pub fn session_id(&self) -> u64 {
        match self {
            Self::Ready { session_id, .. }
            | Self::TrackChanged { session_id, .. }
            | Self::TrackEnded { session_id }
            | Self::Error { session_id, .. } => *session_id,
        }
    }
}

/// Fast-path playback position snapshot.
#[derive(Debug, Clone, Copy, Default)]
pub struct Position {
    pub position_secs: f64,
    pub duration_secs: f64,
}

/// Full player status snapshot.
#[derive(Debug, Clone)]
pub struct Status {
    pub position_secs: f64,
    pub duration_secs: f64,
    pub paused: bool,
    pub volume: u8,
    pub muted: bool,
    pub active: Option<ActiveOutput>,
}

/// Shared track timing state updated by the decode worker and read on demand.
struct TimingState {
    sample_rate: u32,
    base_frame: u64,
    base_secs: f64,
    duration_secs: f64,
    active: Option<ActiveOutput>,
    /// Queued gapless track transitions waiting for `frames_played` to cross
    /// their exact starting frame in the ringbuffer.
    pending_transitions: VecDeque<PendingTransition>,
}

struct PendingTransition {
    boundary_frame: u64,
    url: String,
    info: TrackInfo,
    active: ActiveOutput,
}

enum WorkerCmd {
    Seek(f64),
    QueueNext(Option<String>),
}

struct Session {
    id: u64,
    cancel: Arc<AtomicBool>,
    shared: Arc<StreamShared>,
    cmd_tx: Option<Sender<WorkerCmd>>,
    worker: Option<JoinHandle<()>>,
    /// Direct PCM stream handle (used by `open_pcm` so `stop()` releases
    /// exclusive hardware locks synchronously before the next stream opens).
    pcm_stream: Option<SharedOutputStream>,
}

impl Session {
    fn stop(mut self) {
        self.cancel.store(true, Ordering::Release);
        self.shared.stop.store(true, Ordering::Release);
        if let Some(stream_slot) = self.pcm_stream.take() {
            let _ = stream_slot.lock().unwrap().take();
        }
        if let Some(h) = self.worker.take() {
            // Fast path: wait up to 200 ms so normal playback releases exclusive
            // hardware locks synchronously before the next open(). If the worker
            // is blocked inside a stalled HTTP connect/read, finish joining on a
            // detached reaper thread so callers (including Tokio workers) never hang.
            let deadline = std::time::Instant::now() + Duration::from_millis(200);
            while !h.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if h.is_finished() {
                let _ = h.join();
            } else {
                let _ = thread::Builder::new()
                    .name("tacet-session-reaper".into())
                    .spawn(move || {
                        let _ = h.join();
                    });
            }
        }
    }
}

/// Reusable audiophile audio player.
///
/// Safe to share across threads (`Send + Sync`); suitable for both the
/// standalone `tacet-endpoint` daemon and embedding directly inside a Tauri app.
pub struct Player {
    cfg: EngineConfig,
    /// Serializes session lifecycle transitions (`open`, `open_pcm`, `stop`, `close`)
    /// so exclusive hardware handles are always released before a new stream opens.
    lifecycle: Mutex<()>,
    inner: Mutex<PlayerInner>,
    timing: Arc<Mutex<TimingState>>,
    event_tx: Mutex<Option<Sender<PlaybackEvent>>>,
}

struct PlayerInner {
    next_session_id: u64,
    session: Option<Session>,
    paused: bool,
    volume: u8,
    muted: bool,
    hw_vol: Option<Arc<dyn HardwareVolume>>,
    queued_next: Option<String>,
    pcm_tap: Option<Arc<dyn PcmTap>>,
}

/// Apply current volume/mute state to hardware (when available) and return the
/// resulting software Q16 gain. Never clobbers software mute when a DAC exposes
/// hardware volume without hardware mute.
fn apply_output_gain(st: &mut PlayerInner) -> i64 {
    if let Some(hv) = st.hw_vol.clone() {
        if hv.set_volume(st.volume).is_ok() {
            if hv.set_mute(st.muted).is_ok() {
                return 1 << 16;
            }
            return if st.muted { 0 } else { 1 << 16 };
        }
        // Hardware device disappeared or rejected control; fall back to software.
        st.hw_vol = None;
    }
    volume_gain_q16(st.volume, st.muted)
}

/// Determine initial `(volume, muted)` on startup and apply hardware volume
/// when explicitly requested or when exclusive mode must cap a loud device.
fn resolve_initial_volume(
    initial_volume: Option<u8>,
    exclusive: bool,
    hw_vol: Option<&dyn HardwareVolume>,
) -> (u8, bool) {
    let mut muted = false;
    match initial_volume {
        Some(explicit) => {
            let v = explicit.min(100);
            if let Some(hv) = hw_vol {
                let _ = hv.set_volume(v);
                if let Ok(m) = hv.get_mute() {
                    muted = m;
                }
            }
            (v, muted)
        }
        None => {
            if let Some(hv) = hw_vol {
                if let Ok(m) = hv.get_mute() {
                    muted = m;
                }
                match hv.get_volume() {
                    Ok(cur) if exclusive && cur > EXCLUSIVE_SAFE_VOLUME => {
                        let _ = hv.set_volume(EXCLUSIVE_SAFE_VOLUME);
                        (EXCLUSIVE_SAFE_VOLUME, muted)
                    }
                    Ok(cur) => (cur, muted),
                    Err(_) => (
                        if exclusive {
                            EXCLUSIVE_SAFE_VOLUME
                        } else {
                            100
                        },
                        muted,
                    ),
                }
            } else if exclusive {
                (EXCLUSIVE_SAFE_VOLUME, false)
            } else {
                (100, false)
            }
        }
    }
}

impl Player {
    /// Create a new [`Player`] and return `(player, event_receiver)`.
    pub fn new(cfg: EngineConfig) -> Result<(Self, Receiver<PlaybackEvent>), String> {
        sink::validate_target(&cfg.audio_device)?;
        let (tx, rx) = mpsc::channel();
        let exclusive = sink::is_exclusive_target(&cfg.audio_device, cfg.exclusive);
        let hw_vol = sink::open_hardware_volume(&cfg.audio_device, exclusive);

        let (volume, muted) =
            resolve_initial_volume(cfg.initial_volume, exclusive, hw_vol.as_deref());

        let player = Self {
            cfg,
            lifecycle: Mutex::new(()),
            inner: Mutex::new(PlayerInner {
                next_session_id: 1,
                session: None,
                paused: false,
                volume,
                muted,
                hw_vol,
                queued_next: None,
                pcm_tap: None,
            }),
            timing: Arc::new(Mutex::new(TimingState {
                sample_rate: 0,
                base_frame: 0,
                base_secs: 0.0,
                duration_secs: 0.0,
                active: None,
                pending_transitions: VecDeque::new(),
            })),
            event_tx: Mutex::new(Some(tx)),
        };
        Ok((player, rx))
    }

    /// Register or clear an optional pre-gain [`PcmTap`] (e.g. for Songcast `Sender:1`).
    pub fn set_pcm_tap(&self, tap: Option<Arc<dyn PcmTap>>) {
        self.inner.lock().unwrap().pcm_tap = tap;
    }

    /// Fast-path cached `(volume, muted)` without OS/FFI calls.
    pub fn volume_state(&self) -> (u8, bool) {
        let st = self.inner.lock().unwrap();
        (st.volume, st.muted)
    }

    /// Current active session ID, or `0` if stopped.
    pub fn current_session_id(&self) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .session
            .as_ref()
            .map(|s| s.id)
            .unwrap_or(0)
    }

    /// Reserve the next unique session ID before calling [`Self::open_with_id`].
    /// Allows callers to record `active_session_id` before the worker thread
    /// spawns so fast `Ready`/`Error` events can never race ahead of registration.
    pub fn reserve_session_id(&self) -> u64 {
        let mut st = self.inner.lock().unwrap();
        let id = st.next_session_id;
        st.next_session_id = st.next_session_id.wrapping_add(1).max(1);
        id
    }

    /// Open `url` (HTTP/HTTPS or local file) and begin playback (or start paused
    /// if `paused` is true). Cancels any active session cleanly before opening
    /// the new one so exclusive hardware locks are released first.
    pub fn open(&self, url: &str, paused: bool) -> Result<u64, String> {
        let id = self.reserve_session_id();
        self.open_with_id(id, url, paused)?;
        Ok(id)
    }

    /// Open `url` using a pre-reserved `session_id` from [`Self::reserve_session_id`].
    pub fn open_with_id(&self, session_id: u64, url: &str, paused: bool) -> Result<(), String> {
        let _life = self.lifecycle.lock().unwrap();
        // Stop any existing session outside `inner` so worker join cannot deadlock.
        let prev = {
            let mut st = self.inner.lock().unwrap();
            st.queued_next = None;
            st.paused = paused;
            st.session.take()
        };
        if let Some(s) = prev {
            s.stop();
        }

        // Reset timing state.
        {
            let mut t = self.timing.lock().unwrap();
            t.sample_rate = 0;
            t.base_frame = 0;
            t.base_secs = 0.0;
            t.duration_secs = 0.0;
            t.active = None;
            t.pending_transitions.clear();
        }

        let (gain_q16, pcm_tap) = {
            let mut st = self.inner.lock().unwrap();
            // Re-probe hardware volume on open in case a USB DAC was hotplugged.
            let exclusive = sink::is_exclusive_target(&self.cfg.audio_device, self.cfg.exclusive);
            if st.hw_vol.is_none() {
                st.hw_vol = sink::open_hardware_volume(&self.cfg.audio_device, exclusive);
            }
            let gain = apply_output_gain(&mut st);
            (gain, st.pcm_tap.clone())
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(StreamShared::new(paused, gain_q16));
        let stream_slot: SharedOutputStream = Arc::new(Mutex::new(None));
        let (cmd_tx, cmd_rx) = mpsc::channel();

        let url_owned = url.to_string();
        let target = self.cfg.audio_device.clone();
        let exclusive = self.cfg.exclusive;
        let cancel_bg = Arc::clone(&cancel);
        let shared_bg = Arc::clone(&shared);
        let timing_bg = Arc::clone(&self.timing);
        let stream_bg = Arc::clone(&stream_slot);
        let event_tx = self.event_tx.lock().unwrap().clone();

        let worker = thread::Builder::new()
            .name("tacet-audio-worker".into())
            .spawn(move || {
                run_worker(
                    session_id, url_owned, target, exclusive, cancel_bg, shared_bg, timing_bg,
                    stream_bg, cmd_rx, event_tx, pcm_tap,
                );
            })
            .map_err(|e| format!("spawn audio worker: {e}"))?;

        let mut st = self.inner.lock().unwrap();
        st.session = Some(Session {
            id: session_id,
            cancel,
            shared,
            cmd_tx: Some(cmd_tx),
            worker: Some(worker),
            pcm_stream: Some(stream_slot),
        });
        Ok(())
    }

    /// Open a direct live PCM stream (for Songcast Receiver or other external
    /// PCM sources) using the player's configured device, exclusive mode, and
    /// hardware/software volume state. Cancels any active session first so
    /// exclusive hardware locks are cleanly released.
    pub fn open_pcm(&self, spec: PcmSpec) -> Result<(u64, PcmStreamHandle), String> {
        let _life = self.lifecycle.lock().unwrap();
        let (prev, tap) = {
            let mut st = self.inner.lock().unwrap();
            st.queued_next = None;
            st.paused = false;
            (st.session.take(), st.pcm_tap.clone())
        };
        if let Some(s) = prev {
            s.stop();
        }
        if let Some(t) = tap {
            t.on_halt();
        }

        {
            let mut t = self.timing.lock().unwrap();
            t.sample_rate = spec.sample_rate;
            t.base_frame = 0;
            t.base_secs = 0.0;
            t.duration_secs = 0.0;
            t.active = None;
            t.pending_transitions.clear();
        }

        let (session_id, gain_q16) = {
            let mut st = self.inner.lock().unwrap();
            let id = st.next_session_id;
            st.next_session_id = st.next_session_id.wrapping_add(1).max(1);
            let exclusive = sink::is_exclusive_target(&self.cfg.audio_device, self.cfg.exclusive);
            if st.hw_vol.is_none() {
                st.hw_vol = sink::open_hardware_volume(&self.cfg.audio_device, exclusive);
            }
            let gain = apply_output_gain(&mut st);
            (id, gain)
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(StreamShared::new(false, gain_q16));
        let cap_samples = buffer_capacity_samples(spec);
        let capacity_frames = cap_samples / spec.channels.max(1) as usize;
        let (producer, consumer) = RingBuffer::<i32>::new(cap_samples);
        let output = sink::open_stream(
            &self.cfg.audio_device,
            self.cfg.exclusive,
            spec,
            consumer,
            Arc::clone(&shared),
        )?;
        let active = output.active_output().clone();
        self.timing.lock().unwrap().active = Some(active.clone());

        let stream_slot = Arc::new(Mutex::new(Some(output)));
        let handle = PcmStreamHandle {
            spec,
            active,
            capacity_frames,
            producer,
            shared: Arc::clone(&shared),
            cancel: Arc::clone(&cancel),
            stream: Arc::clone(&stream_slot),
        };

        let mut st = self.inner.lock().unwrap();
        st.session = Some(Session {
            id: session_id,
            cancel,
            shared,
            cmd_tx: None,
            worker: None,
            pcm_stream: Some(stream_slot),
        });
        Ok((session_id, handle))
    }

    /// Queue a successor URL for gapless transition when the current track ends.
    pub fn queue_next(&self, url: &str) {
        let mut st = self.inner.lock().unwrap();
        st.queued_next = Some(url.to_string());
        if let Some(sess) = &st.session
            && let Some(tx) = &sess.cmd_tx
        {
            let _ = tx.send(WorkerCmd::QueueNext(Some(url.to_string())));
        }
    }

    /// Clear any previously queued gapless successor.
    pub fn clear_next(&self) {
        let mut st = self.inner.lock().unwrap();
        st.queued_next = None;
        if let Some(sess) = &st.session
            && let Some(tx) = &sess.cmd_tx
        {
            let _ = tx.send(WorkerCmd::QueueNext(None));
        }
    }

    /// Resume playback if paused.
    pub fn play(&self) {
        let mut st = self.inner.lock().unwrap();
        st.paused = false;
        if let Some(sess) = &st.session {
            sess.shared.paused.store(false, Ordering::Release);
        }
    }

    /// Pause playback without releasing the output stream.
    pub fn pause(&self) {
        let tap = {
            let mut st = self.inner.lock().unwrap();
            st.paused = true;
            if let Some(sess) = &st.session {
                sess.shared.paused.store(true, Ordering::Release);
            }
            st.pcm_tap.clone()
        };
        if let Some(t) = tap {
            t.on_halt();
        }
    }

    /// Stop playback immediately and release the OS audio device (and hog lock).
    pub fn stop(&self) {
        let _life = self.lifecycle.lock().unwrap();
        let (prev, tap) = {
            let mut st = self.inner.lock().unwrap();
            st.paused = false;
            st.queued_next = None;
            (st.session.take(), st.pcm_tap.clone())
        };
        if let Some(s) = prev {
            s.stop();
        }
        if let Some(t) = tap {
            t.on_halt();
        }
        let mut t = self.timing.lock().unwrap();
        t.sample_rate = 0;
        t.base_frame = 0;
        t.base_secs = 0.0;
        t.duration_secs = 0.0;
        t.active = None;
        t.pending_transitions.clear();
    }

    /// Seek within the active track to `seconds`.
    pub fn seek(&self, seconds: f64) -> Result<(), String> {
        let st = self.inner.lock().unwrap();
        let Some(sess) = &st.session else {
            return Err("not playing".into());
        };
        let Some(tx) = &sess.cmd_tx else {
            return Err("stream is not seekable".into());
        };
        tx.send(WorkerCmd::Seek(seconds.max(0.0)))
            .map_err(|_| "audio worker stopped".to_string())
    }

    /// Set output volume (0..=100). Drives hardware volume when available (keeping
    /// PCM at unity pass-through), otherwise applies integer software gain.
    pub fn set_volume(&self, volume: u8) -> Result<(), String> {
        let volume = volume.min(100);
        let mut st = self.inner.lock().unwrap();
        st.volume = volume;
        let gain = apply_output_gain(&mut st);
        if let Some(sess) = &st.session {
            sess.shared.gain_q16.store(gain, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Set output mute state.
    pub fn set_mute(&self, muted: bool) -> Result<(), String> {
        let mut st = self.inner.lock().unwrap();
        st.muted = muted;
        let gain = apply_output_gain(&mut st);
        if let Some(sess) = &st.session {
            sess.shared.gain_q16.store(gain, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Sync volume/mute from OS hardware if supported (returns `(volume, muted)`).
    pub fn sync_volume(&self) -> (u8, bool) {
        let mut st = self.inner.lock().unwrap();
        if st.hw_vol.is_none() {
            let exclusive = sink::is_exclusive_target(&self.cfg.audio_device, self.cfg.exclusive);
            st.hw_vol = sink::open_hardware_volume(&self.cfg.audio_device, exclusive);
        }
        if let Some(hv) = &st.hw_vol {
            match (hv.get_volume(), hv.get_mute()) {
                (Ok(v), Ok(m)) => {
                    st.volume = v;
                    st.muted = m;
                }
                (Ok(v), Err(_)) => {
                    st.volume = v;
                }
                _ => {
                    st.hw_vol = None;
                }
            }
        }
        (st.volume, st.muted)
    }

    /// Fast-path playback position query (no OS/FFI calls).
    pub fn position(&self) -> Position {
        let frames_played = {
            let st = self.inner.lock().unwrap();
            st.session
                .as_ref()
                .map(|s| s.shared.frames_played())
                .unwrap_or(0)
        };
        let t = self.timing.lock().unwrap();
        if t.sample_rate == 0 {
            return Position::default();
        }
        let elapsed_frames = frames_played.saturating_sub(t.base_frame);
        let pos = t.base_secs + (elapsed_frames as f64 / t.sample_rate as f64);
        let pos = if t.duration_secs > 0.0 {
            pos.min(t.duration_secs)
        } else {
            pos
        };
        Position {
            position_secs: pos,
            duration_secs: t.duration_secs,
        }
    }

    /// Full player status snapshot.
    pub fn status(&self) -> Status {
        let pos = self.position();
        let st = self.inner.lock().unwrap();
        let active = self.timing.lock().unwrap().active.clone();
        Status {
            position_secs: pos.position_secs,
            duration_secs: pos.duration_secs,
            paused: st.paused,
            volume: st.volume,
            muted: st.muted,
            active,
        }
    }

    /// Stop playback and close the event channel.
    pub fn close(&self) {
        self.stop();
        *self.event_tx.lock().unwrap() = None;
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.close();
    }
}

fn emit(tx: &Option<Sender<PlaybackEvent>>, ev: PlaybackEvent) {
    if let Some(tx) = tx {
        let _ = tx.send(ev);
    }
}

fn buffer_capacity_samples(spec: PcmSpec) -> usize {
    // ~1.5 s ringbuffer, clamped to [64K, 1.5M] samples (~256 KiB .. 6 MiB),
    // rounded down to an exact multiple of channel count so frames never split.
    let ch = spec.channels.max(1) as usize;
    let target = (spec.sample_rate as usize) * ch * 3 / 2;
    let clamped = target.clamp(65_536, 1_572_864);
    (clamped / ch).max(1) * ch
}

/// Request a ringbuffer flush on the sink and wait briefly for the sink to
/// acknowledge this exact generation so post-seek samples are never discarded
/// and back-to-back flush requests can never be lost.
fn wait_for_flush(shared: &StreamShared, cancel: &AtomicBool) {
    let target = shared.flush_req.fetch_add(1, Ordering::Release) + 1;
    let deadline = std::time::Instant::now() + Duration::from_millis(200);
    while shared.flush_ack.load(Ordering::Acquire) < target
        && !cancel.load(Ordering::Relaxed)
        && !shared.is_failed()
        && std::time::Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(2));
    }
}

fn advance_pending_transitions(
    session_id: u64,
    frames_played: u64,
    timing: &Arc<Mutex<TimingState>>,
    event_tx: &Option<Sender<PlaybackEvent>>,
) {
    let mut emitted = Vec::new();
    {
        let mut t = timing.lock().unwrap();
        while let Some(front) = t.pending_transitions.front() {
            if frames_played >= front.boundary_frame {
                let tr = t.pending_transitions.pop_front().unwrap();
                t.sample_rate = tr.info.spec.sample_rate;
                t.base_frame = tr.boundary_frame;
                t.base_secs = 0.0;
                t.duration_secs = tr.info.duration_secs.unwrap_or(0.0);
                t.active = Some(tr.active.clone());
                emitted.push(PlaybackEvent::TrackChanged {
                    session_id,
                    url: tr.url,
                    spec: tr.info.spec,
                    duration_secs: tr.info.duration_secs,
                    codec: tr.info.codec,
                    lossless: tr.info.lossless,
                    active: tr.active,
                });
            } else {
                break;
            }
        }
    }
    for ev in emitted {
        emit(event_tx, ev);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_worker(
    session_id: u64,
    initial_url: String,
    target: String,
    exclusive: bool,
    cancel: Arc<AtomicBool>,
    shared: Arc<StreamShared>,
    timing: Arc<Mutex<TimingState>>,
    stream_slot: SharedOutputStream,
    cmd_rx: Receiver<WorkerCmd>,
    event_tx: Option<Sender<PlaybackEvent>>,
    pcm_tap: Option<Arc<dyn PcmTap>>,
) {
    let mut decoder = match AudioDecoder::open(&initial_url, Arc::clone(&cancel)) {
        Ok(d) => d,
        Err(e) => {
            if !cancel.load(Ordering::Relaxed) {
                emit(
                    &event_tx,
                    PlaybackEvent::Error {
                        session_id,
                        message: e,
                    },
                );
            }
            return;
        }
    };

    let mut cur_spec = decoder.info().spec;
    let (mut producer, consumer) = RingBuffer::<i32>::new(buffer_capacity_samples(cur_spec));

    let initial_stream: Box<dyn OutputStream> =
        match sink::open_stream(&target, exclusive, cur_spec, consumer, Arc::clone(&shared)) {
            Ok(s) => s,
            Err(e) => {
                if !cancel.load(Ordering::Relaxed) {
                    emit(
                        &event_tx,
                        PlaybackEvent::Error {
                            session_id,
                            message: e,
                        },
                    );
                }
                return;
            }
        };

    let mut cur_active = initial_stream.active_output().clone();
    if cancel.load(Ordering::Acquire) {
        drop(initial_stream);
        return;
    }
    *stream_slot.lock().unwrap() = Some(initial_stream);
    {
        let mut t = timing.lock().unwrap();
        t.sample_rate = cur_spec.sample_rate;
        t.base_frame = 0;
        t.base_secs = 0.0;
        t.duration_secs = decoder.info().duration_secs.unwrap_or(0.0);
        t.active = Some(cur_active.clone());
    }
    emit(
        &event_tx,
        PlaybackEvent::Ready {
            session_id,
            url: initial_url,
            spec: cur_spec,
            duration_secs: decoder.info().duration_secs,
            codec: decoder.info().codec.clone(),
            lossless: decoder.info().lossless,
            active: cur_active.clone(),
        },
    );

    let mut queued_next: Option<String> = None;
    let mut frames_pushed: u64 = 0;
    let mut pending_samples: Vec<i32> = Vec::new();
    let mut pending_offset: usize = 0;

    'outer: loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if shared.is_failed() {
            emit(
                &event_tx,
                PlaybackEvent::Error {
                    session_id,
                    message: "audio output device failed or disconnected".into(),
                },
            );
            break;
        }

        // Process any queued control commands (Seek / QueueNext).
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                WorkerCmd::QueueNext(next) => {
                    queued_next = next;
                }
                WorkerCmd::Seek(sec) => {
                    if let Ok(landed) = decoder.seek(sec) {
                        pending_samples.clear();
                        pending_offset = 0;
                        wait_for_flush(&shared, &cancel);
                        if let Some(tap) = &pcm_tap {
                            tap.on_halt();
                        }
                        let cur_played = shared.frames_played();
                        frames_pushed = cur_played;
                        let mut t = timing.lock().unwrap();
                        t.base_frame = cur_played;
                        t.base_secs = landed;
                        t.pending_transitions.clear();
                    }
                }
            }
        }

        // Advance any gapless track boundaries that the hardware just crossed.
        advance_pending_transitions(session_id, shared.frames_played(), &timing, &event_tx);

        // When a live PCM tap (e.g. Songcast Sender) has active listeners, keep
        // ringbuffer lead modest (~100 ms) so rebroadcast packets stay aligned
        // with real-time playback instead of bursting 1.5 s ahead.
        let tap_active = pcm_tap.as_ref().is_some_and(|t| t.is_active());
        if tap_active {
            if shared.is_paused() {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            let max_lead_frames = (cur_spec.sample_rate as u64 / 10).max(512);
            let lead = frames_pushed.saturating_sub(shared.frames_played());
            if lead >= max_lead_frames {
                thread::sleep(Duration::from_millis(3));
                continue;
            }
        }

        let ch = cur_spec.channels.max(1) as usize;
        // When a live PCM tap (Songcast Sender) is active, cap each push to ~5 ms
        // so UDP packets are emitted at a steady cadence rather than bursting a
        // full 90-370 ms decoded frame back-to-back.
        let tap_step_samples = if tap_active {
            ((cur_spec.sample_rate as usize / 200).max(64)) * ch
        } else {
            usize::MAX
        };

        // Push leftover samples from the previous decoded chunk first.
        if pending_offset < pending_samples.len() {
            let prev_off = pending_offset;
            let end_off = pending_samples
                .len()
                .min(prev_off.saturating_add(tap_step_samples));
            let written = push_samples(&mut producer, &pending_samples[prev_off..end_off], ch);
            if written > 0
                && tap_active
                && let Some(tap) = &pcm_tap
            {
                tap.on_pcm(
                    cur_spec,
                    decoder.info(),
                    &pending_samples[prev_off..prev_off + written],
                );
            }
            pending_offset += written;
            frames_pushed += (written / ch) as u64;
            if pending_offset < pending_samples.len() {
                thread::sleep(Duration::from_millis(if tap_active { 4 } else { 5 }));
                continue;
            }
            pending_samples.clear();
            pending_offset = 0;
        }

        // Decode next audio chunk.
        let cur_info = decoder.info().clone();
        match decoder.next_chunk() {
            Ok(Some(samples)) => {
                let slice_end = samples.len().min(tap_step_samples);
                let written = push_samples(&mut producer, &samples[..slice_end], ch);
                if written > 0
                    && tap_active
                    && let Some(tap) = &pcm_tap
                {
                    tap.on_pcm(cur_spec, &cur_info, &samples[..written]);
                }
                frames_pushed += (written / ch) as u64;
                if written < samples.len() {
                    pending_samples.extend_from_slice(&samples[written..]);
                    pending_offset = 0;
                    thread::sleep(Duration::from_millis(if tap_active { 4 } else { 5 }));
                }
            }
            Ok(None) => {
                // Current track reached EOF. Check for pending QueueNext or Seek.
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        WorkerCmd::QueueNext(next) => {
                            queued_next = next;
                        }
                        WorkerCmd::Seek(sec) => {
                            if let Ok(landed) = decoder.seek(sec) {
                                wait_for_flush(&shared, &cancel);
                                if let Some(tap) = &pcm_tap {
                                    tap.on_halt();
                                }
                                let cur_played = shared.frames_played();
                                frames_pushed = cur_played;
                                let mut t = timing.lock().unwrap();
                                t.base_frame = cur_played;
                                t.base_secs = landed;
                                t.pending_transitions.clear();
                                continue 'outer;
                            }
                        }
                    }
                }

                if let Some(next_url) = queued_next.take() {
                    match AudioDecoder::open(&next_url, Arc::clone(&cancel)) {
                        Ok(next_dec) => {
                            let next_info = next_dec.info().clone();
                            let bit_depth_ok = cur_active.sample_format.bit_depth()
                                >= next_info.spec.bits_per_sample;
                            if next_info.spec.hardware_compatible(cur_spec) && bit_depth_ok {
                                // True sample-accurate gapless transition on the same stream!
                                let boundary = frames_pushed;
                                let active = cur_active.clone();
                                {
                                    let mut t = timing.lock().unwrap();
                                    t.pending_transitions.push_back(PendingTransition {
                                        boundary_frame: boundary,
                                        url: next_url,
                                        info: next_info,
                                        active,
                                    });
                                }
                                decoder = next_dec;
                                continue 'outer;
                            }

                            // Sample rate, channel count, or higher bit depth changed: drain
                            // current track cleanly, then reconfigure hardware for the new format.
                            shared.draining.store(true, Ordering::Release);
                            while !shared.is_finished()
                                && !shared.is_failed()
                                && !cancel.load(Ordering::Relaxed)
                            {
                                advance_pending_transitions(
                                    session_id,
                                    shared.frames_played(),
                                    &timing,
                                    &event_tx,
                                );
                                thread::sleep(Duration::from_millis(5));
                            }
                            if cancel.load(Ordering::Relaxed) {
                                break 'outer;
                            }
                            if shared.is_failed() {
                                emit(
                                    &event_tx,
                                    PlaybackEvent::Error {
                                        session_id,
                                        message: "audio output device failed or disconnected"
                                            .into(),
                                    },
                                );
                                break 'outer;
                            }

                            // Release old output stream before opening new rate/format.
                            let _ = stream_slot.lock().unwrap().take();
                            shared.stop.store(false, Ordering::Release);
                            shared.draining.store(false, Ordering::Release);
                            shared.finished.store(false, Ordering::Release);
                            shared.failed.store(false, Ordering::Release);
                            shared.frames_played.store(0, Ordering::Release);
                            frames_pushed = 0;

                            cur_spec = next_info.spec;
                            let (new_prod, new_cons) =
                                RingBuffer::<i32>::new(buffer_capacity_samples(cur_spec));
                            producer = new_prod;
                            let new_stream = match sink::open_stream(
                                &target,
                                exclusive,
                                cur_spec,
                                new_cons,
                                Arc::clone(&shared),
                            ) {
                                Ok(s) => s,
                                Err(e) => {
                                    if !cancel.load(Ordering::Relaxed) {
                                        emit(
                                            &event_tx,
                                            PlaybackEvent::Error {
                                                session_id,
                                                message: e,
                                            },
                                        );
                                    }
                                    return;
                                }
                            };
                            cur_active = new_stream.active_output().clone();
                            if cancel.load(Ordering::Acquire) {
                                drop(new_stream);
                                return;
                            }
                            *stream_slot.lock().unwrap() = Some(new_stream);
                            let active = cur_active.clone();
                            {
                                let mut t = timing.lock().unwrap();
                                t.sample_rate = cur_spec.sample_rate;
                                t.base_frame = 0;
                                t.base_secs = 0.0;
                                t.duration_secs = next_info.duration_secs.unwrap_or(0.0);
                                t.active = Some(active.clone());
                                t.pending_transitions.clear();
                            }
                            emit(
                                &event_tx,
                                PlaybackEvent::TrackChanged {
                                    session_id,
                                    url: next_url,
                                    spec: cur_spec,
                                    duration_secs: next_info.duration_secs,
                                    codec: next_info.codec.clone(),
                                    lossless: next_info.lossless,
                                    active,
                                },
                            );
                            decoder = next_dec;
                            continue 'outer;
                        }
                        Err(e) => {
                            if !cancel.load(Ordering::Relaxed) {
                                emit(
                                    &event_tx,
                                    PlaybackEvent::Error {
                                        session_id,
                                        message: e,
                                    },
                                );
                            }
                            break 'outer;
                        }
                    }
                }

                // No next track queued: drain remaining buffered audio cleanly.
                shared.draining.store(true, Ordering::Release);
                while !shared.is_finished()
                    && !shared.is_failed()
                    && !cancel.load(Ordering::Relaxed)
                {
                    advance_pending_transitions(
                        session_id,
                        shared.frames_played(),
                        &timing,
                        &event_tx,
                    );
                    // Allow late queue_next or seek while draining.
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        match cmd {
                            WorkerCmd::QueueNext(Some(next_url)) => {
                                if let Ok(next_dec) =
                                    AudioDecoder::open(&next_url, Arc::clone(&cancel))
                                    && next_dec.info().spec.hardware_compatible(cur_spec)
                                    && cur_active.sample_format.bit_depth()
                                        >= next_dec.info().spec.bits_per_sample
                                    && !shared.is_finished()
                                {
                                    shared.draining.store(false, Ordering::Release);
                                    let boundary = frames_pushed;
                                    let active = cur_active.clone();
                                    let mut t = timing.lock().unwrap();
                                    t.pending_transitions.push_back(PendingTransition {
                                        boundary_frame: boundary,
                                        url: next_url,
                                        info: next_dec.info().clone(),
                                        active,
                                    });
                                    drop(t);
                                    decoder = next_dec;
                                    continue 'outer;
                                }
                                // Keep draining; if a cross-rate track was queued late,
                                // remember it so we reconfigure hardware right after drain.
                                queued_next = Some(next_url);
                                shared.draining.store(true, Ordering::Release);
                            }
                            WorkerCmd::QueueNext(None) => {
                                queued_next = None;
                            }
                            WorkerCmd::Seek(sec) => {
                                if let Ok(landed) = decoder.seek(sec) {
                                    shared.draining.store(false, Ordering::Release);
                                    shared.finished.store(false, Ordering::Release);
                                    wait_for_flush(&shared, &cancel);
                                    if let Some(tap) = &pcm_tap {
                                        tap.on_halt();
                                    }
                                    let cur_played = shared.frames_played();
                                    frames_pushed = cur_played;
                                    let mut t = timing.lock().unwrap();
                                    t.base_frame = cur_played;
                                    t.base_secs = landed;
                                    t.pending_transitions.clear();
                                    continue 'outer;
                                }
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(5));
                }

                // Drain any last-instant QueueNext that arrived while the sink
                // finished draining (e.g. unpaced PCM sink or very short tracks).
                while let Ok(cmd) = cmd_rx.try_recv() {
                    if let WorkerCmd::QueueNext(next) = cmd {
                        queued_next = next;
                    }
                }

                if !cancel.load(Ordering::Relaxed)
                    && !shared.is_failed()
                    && let Some(next_url) = queued_next.take()
                {
                    // A successor arrived while draining: if same format and
                    // stream is still open (just finished draining), continue
                    // gaplessly on the same stream; otherwise reconfigure hardware.
                    match AudioDecoder::open(&next_url, Arc::clone(&cancel)) {
                        Ok(next_dec) => {
                            let next_info = next_dec.info().clone();
                            let bit_depth_ok = cur_active.sample_format.bit_depth()
                                >= next_info.spec.bits_per_sample;
                            if next_info.spec.hardware_compatible(cur_spec) && bit_depth_ok {
                                shared.draining.store(false, Ordering::Release);
                                shared.finished.store(false, Ordering::Release);
                                let boundary = frames_pushed;
                                let active = cur_active.clone();
                                {
                                    let mut t = timing.lock().unwrap();
                                    t.pending_transitions.push_back(PendingTransition {
                                        boundary_frame: boundary,
                                        url: next_url,
                                        info: next_info,
                                        active,
                                    });
                                }
                                decoder = next_dec;
                                continue 'outer;
                            }
                            let _ = stream_slot.lock().unwrap().take();
                            shared.stop.store(false, Ordering::Release);
                            shared.draining.store(false, Ordering::Release);
                            shared.finished.store(false, Ordering::Release);
                            shared.failed.store(false, Ordering::Release);
                            shared.frames_played.store(0, Ordering::Release);
                            frames_pushed = 0;

                            cur_spec = next_info.spec;
                            let (new_prod, new_cons) =
                                RingBuffer::<i32>::new(buffer_capacity_samples(cur_spec));
                            producer = new_prod;
                            let new_stream = match sink::open_stream(
                                &target,
                                exclusive,
                                cur_spec,
                                new_cons,
                                Arc::clone(&shared),
                            ) {
                                Ok(s) => s,
                                Err(e) => {
                                    if !cancel.load(Ordering::Relaxed) {
                                        emit(
                                            &event_tx,
                                            PlaybackEvent::Error {
                                                session_id,
                                                message: e,
                                            },
                                        );
                                    }
                                    return;
                                }
                            };
                            cur_active = new_stream.active_output().clone();
                            if cancel.load(Ordering::Acquire) {
                                drop(new_stream);
                                return;
                            }
                            *stream_slot.lock().unwrap() = Some(new_stream);
                            let active = cur_active.clone();
                            {
                                let mut t = timing.lock().unwrap();
                                t.sample_rate = cur_spec.sample_rate;
                                t.base_frame = 0;
                                t.base_secs = 0.0;
                                t.duration_secs = next_info.duration_secs.unwrap_or(0.0);
                                t.active = Some(active.clone());
                                t.pending_transitions.clear();
                            }
                            emit(
                                &event_tx,
                                PlaybackEvent::TrackChanged {
                                    session_id,
                                    url: next_url,
                                    spec: cur_spec,
                                    duration_secs: next_info.duration_secs,
                                    codec: next_info.codec.clone(),
                                    lossless: next_info.lossless,
                                    active,
                                },
                            );
                            decoder = next_dec;
                            continue 'outer;
                        }
                        Err(e) => {
                            if !cancel.load(Ordering::Relaxed) {
                                emit(
                                    &event_tx,
                                    PlaybackEvent::Error {
                                        session_id,
                                        message: e,
                                    },
                                );
                            }
                            break 'outer;
                        }
                    }
                }

                if !cancel.load(Ordering::Relaxed) {
                    if shared.is_failed() {
                        emit(
                            &event_tx,
                            PlaybackEvent::Error {
                                session_id,
                                message: "audio output device failed or disconnected".into(),
                            },
                        );
                    } else {
                        advance_pending_transitions(
                            session_id,
                            shared.frames_played(),
                            &timing,
                            &event_tx,
                        );
                        emit(&event_tx, PlaybackEvent::TrackEnded { session_id });
                    }
                }
                break 'outer;
            }
            Err(e) => {
                if !cancel.load(Ordering::Relaxed) {
                    emit(
                        &event_tx,
                        PlaybackEvent::Error {
                            session_id,
                            message: e,
                        },
                    );
                }
                break 'outer;
            }
        }
    }

    if let Some(tap) = &pcm_tap {
        tap.on_halt();
    }
    // Ensure the output stream is released cleanly on worker exit.
    let _ = stream_slot.lock().unwrap().take();
}

fn push_samples(producer: &mut Producer<i32>, samples: &[i32], channels: usize) -> usize {
    let ch = channels.max(1);
    let avail = producer.slots();
    let take = (avail.min(samples.len()) / ch) * ch;
    if take == 0 {
        return 0;
    }
    if let Ok(mut chunk) = producer.write_chunk_uninit(take) {
        let (s1, s2) = chunk.as_mut_slices();
        let n1 = s1.len();
        for (dst, &src) in s1.iter_mut().zip(&samples[..n1]) {
            dst.write(src);
        }
        for (dst, &src) in s2.iter_mut().zip(&samples[n1..take]) {
            dst.write(src);
        }
        unsafe { chunk.commit_all() };
        take
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;
    use std::fs;
    use std::io::Write;

    fn write_test_wav(path: &std::path::Path, sample_rate: u32, frames: usize, freq_hz: f64) {
        let channels: u16 = 2;
        let bits: u16 = 16;
        let data_bytes = (frames * channels as usize * 2) as u32;
        let mut f = fs::File::create(path).unwrap();
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data_bytes).to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&channels.to_le_bytes()).unwrap();
        f.write_all(&sample_rate.to_le_bytes()).unwrap();
        f.write_all(&(sample_rate * channels as u32 * 2).to_le_bytes())
            .unwrap();
        f.write_all(&(channels * 2).to_le_bytes()).unwrap();
        f.write_all(&bits.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_bytes.to_le_bytes()).unwrap();
        for i in 0..frames {
            let s = ((2.0 * PI * freq_hz * i as f64 / sample_rate as f64).sin() * 12000.0) as i16;
            f.write_all(&s.to_le_bytes()).unwrap();
            f.write_all(&s.to_le_bytes()).unwrap();
        }
    }

    #[test]
    fn plays_and_completes_on_null_sink() {
        let dir = std::env::temp_dir().join(format!("tacet-audio-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let wav = dir.join("tone.wav");
        write_test_wav(&wav, 44_100, 8_820, 440.0); // 200 ms

        let (player, rx) = Player::new(EngineConfig {
            audio_device: "null".into(),
            exclusive: false,
            initial_volume: Some(80),
        })
        .unwrap();

        player.open(wav.to_str().unwrap(), false).unwrap();

        let mut saw_ready = false;
        let mut saw_ended = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !saw_ended {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                match ev {
                    PlaybackEvent::Ready { spec, .. } => {
                        assert_eq!(spec.sample_rate, 44_100);
                        assert_eq!(spec.channels, 2);
                        saw_ready = true;
                    }
                    PlaybackEvent::TrackEnded { .. } => {
                        saw_ended = true;
                    }
                    PlaybackEvent::Error { message: e, .. } => panic!("unexpected error: {e}"),
                    _ => {}
                }
            }
        }
        assert!(saw_ready, "expected Ready event");
        assert!(saw_ended, "expected TrackEnded event");
        player.close();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gapless_pcm_transition_preserves_exact_frame_count() {
        let dir = std::env::temp_dir().join(format!("tacet-audio-gapless-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let wav1 = dir.join("t1.wav");
        let wav2 = dir.join("t2.wav");
        let pcm_out = dir.join("out.pcm");
        write_test_wav(&wav1, 44_100, 4_410, 440.0); // 100 ms
        write_test_wav(&wav2, 44_100, 4_410, 880.0); // 100 ms

        let (player, rx) = Player::new(EngineConfig {
            audio_device: format!("pcm/{}", pcm_out.display()),
            exclusive: false,
            initial_volume: Some(100),
        })
        .unwrap();

        // Start paused so queue_next is guaranteed to arrive before EOF.
        player.open(wav1.to_str().unwrap(), true).unwrap();
        let ev = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(matches!(ev, PlaybackEvent::Ready { .. }));

        player.queue_next(wav2.to_str().unwrap());
        player.play();

        let mut saw_changed = false;
        let mut saw_ended = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !saw_ended {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                match ev {
                    PlaybackEvent::TrackChanged { .. } => saw_changed = true,
                    PlaybackEvent::TrackEnded { .. } => saw_ended = true,
                    PlaybackEvent::Error { message: e, .. } => panic!("unexpected error: {e}"),
                    _ => {}
                }
            }
        }
        player.close();

        assert!(saw_changed, "expected TrackChanged event");
        assert!(saw_ended, "expected TrackEnded event");
        let meta = fs::metadata(&pcm_out).unwrap();
        // Exact total bytes = (4410 + 4410) frames * 2 channels * 2 bytes/sample = 35280 bytes.
        assert_eq!(meta.len(), (4_410 + 4_410) * 2 * 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn buffer_capacity_is_always_frame_aligned() {
        for ch in [1u16, 2, 3, 6, 8] {
            let cap = buffer_capacity_samples(PcmSpec {
                sample_rate: 44_100,
                channels: ch,
                bits_per_sample: 24,
            });
            assert_eq!(cap % (ch as usize), 0);
        }
    }

    #[test]
    fn hw_vol_without_hw_mute_preserves_software_mute() {
        struct VolOnlyHw;
        impl HardwareVolume for VolOnlyHw {
            fn get_volume(&self) -> Result<u8, String> {
                Ok(50)
            }
            fn set_volume(&self, _v: u8) -> Result<(), String> {
                Ok(())
            }
            fn get_mute(&self) -> Result<bool, String> {
                Err("no hw mute".into())
            }
            fn set_mute(&self, _m: bool) -> Result<(), String> {
                Err("no hw mute".into())
            }
        }
        let mut st = PlayerInner {
            next_session_id: 1,
            session: None,
            paused: false,
            volume: 80,
            muted: true,
            hw_vol: Some(Arc::new(VolOnlyHw)),
            queued_next: None,
            pcm_tap: None,
        };
        // Changing volume while muted must keep software gain at 0.
        assert_eq!(apply_output_gain(&mut st), 0);
        st.muted = false;
        assert_eq!(apply_output_gain(&mut st), 1 << 16);
    }

    #[test]
    fn cross_sample_rate_transition_reconfigures_stream() {
        let dir = std::env::temp_dir().join(format!("tacet-audio-xrate-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let wav1 = dir.join("t44.wav");
        let wav2 = dir.join("t48.wav");
        write_test_wav(&wav1, 44_100, 4_410, 440.0); // 100 ms @ 44.1 kHz
        write_test_wav(&wav2, 48_000, 4_800, 880.0); // 100 ms @ 48 kHz

        let (player, rx) = Player::new(EngineConfig {
            audio_device: "null".into(),
            exclusive: false,
            initial_volume: Some(100),
        })
        .unwrap();

        let sid = player.open(wav1.to_str().unwrap(), true).unwrap();
        let ev = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(
            matches!(ev, PlaybackEvent::Ready { session_id, spec, .. } if session_id == sid && spec.sample_rate == 44_100)
        );

        player.queue_next(wav2.to_str().unwrap());
        player.play();

        let mut saw_48k = false;
        let mut saw_ended = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !saw_ended {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                match ev {
                    PlaybackEvent::TrackChanged {
                        session_id, spec, ..
                    } => {
                        assert_eq!(session_id, sid);
                        assert_eq!(spec.sample_rate, 48_000);
                        saw_48k = true;
                    }
                    PlaybackEvent::TrackEnded { session_id } => {
                        assert_eq!(session_id, sid);
                        saw_ended = true;
                    }
                    PlaybackEvent::Error { message, .. } => panic!("unexpected error: {message}"),
                    _ => {}
                }
            }
        }
        player.close();
        assert!(saw_48k, "expected TrackChanged at 48 kHz");
        assert!(saw_ended, "expected TrackEnded");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seek_pause_and_error_emit_expected_lifecycle_events() {
        let dir = std::env::temp_dir().join(format!("tacet-audio-seek-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let wav = dir.join("long.wav");
        write_test_wav(&wav, 44_100, 22_050, 440.0); // 500 ms

        let (player, rx) = Player::new(EngineConfig {
            audio_device: "null".into(),
            exclusive: false,
            initial_volume: Some(100),
        })
        .unwrap();

        let sid1 = player.open(wav.to_str().unwrap(), true).unwrap();
        let ev = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(matches!(ev, PlaybackEvent::Ready { session_id, .. } if session_id == sid1));

        // While paused, position stays at 0.
        thread::sleep(Duration::from_millis(40));
        assert!(player.position().position_secs < 0.05);

        // Seek near the end (0.35s) and unpause -> completes quickly.
        player.seek(0.35).unwrap();
        player.play();

        let mut saw_ended = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        while std::time::Instant::now() < deadline && !saw_ended {
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(50))
                && matches!(ev, PlaybackEvent::TrackEnded { session_id } if session_id == sid1)
            {
                saw_ended = true;
            }
        }
        assert!(saw_ended, "expected TrackEnded after seek");

        // Opening a missing file emits Error with a strictly newer session_id.
        let sid2 = player
            .open("/nonexistent/tacet-missing-file.flac", false)
            .unwrap();
        assert!(sid2 > sid1);
        let ev = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(matches!(ev, PlaybackEvent::Error { session_id, .. } if session_id == sid2));

        player.close();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_generation_counter_handles_back_to_back_flushes() {
        let (mut prod, mut cons) = RingBuffer::<i32>::new(64);
        let shared = StreamShared::new(false, 1 << 16);

        // Push 4 samples, request flush #1, sink flushes it.
        assert_eq!(push_samples(&mut prod, &[1, 2, 3, 4], 2), 4);
        let g1 = shared.flush_req.fetch_add(1, Ordering::Release) + 1;
        shared.maybe_flush(&mut cons);
        assert_eq!(shared.flush_ack.load(Ordering::Acquire), g1);
        assert_eq!(cons.slots(), 0);

        // Request flush #2, push stale samples before sink sees #2 -> sink drains them.
        let g2 = shared.flush_req.fetch_add(1, Ordering::Release) + 1;
        assert_eq!(push_samples(&mut prod, &[9, 9], 2), 2);
        shared.maybe_flush(&mut cons);
        assert_eq!(shared.flush_ack.load(Ordering::Acquire), g2);
        assert_eq!(cons.slots(), 0);
    }

    #[test]
    fn exclusive_initial_volume_caps_loud_hardware_and_preserves_quiet_hardware() {
        use std::sync::atomic::AtomicU8;
        struct MockHw {
            vol: AtomicU8,
            writes: Mutex<Vec<u8>>,
        }
        impl HardwareVolume for MockHw {
            fn get_volume(&self) -> Result<u8, String> {
                Ok(self.vol.load(Ordering::Relaxed))
            }
            fn set_volume(&self, v: u8) -> Result<(), String> {
                self.vol.store(v, Ordering::Relaxed);
                self.writes.lock().unwrap().push(v);
                Ok(())
            }
            fn get_mute(&self) -> Result<bool, String> {
                Ok(false)
            }
            fn set_mute(&self, _m: bool) -> Result<(), String> {
                Ok(())
            }
        }

        // Exclusive + loud hardware (100%) -> capped down to EXCLUSIVE_SAFE_VOLUME (30%).
        let loud = MockHw {
            vol: AtomicU8::new(100),
            writes: Mutex::new(Vec::new()),
        };
        assert_eq!(resolve_initial_volume(None, true, Some(&loud)), (30, false));
        assert_eq!(*loud.writes.lock().unwrap(), vec![30]);

        // Exclusive + already-quiet hardware (18%) -> preserved without turning up.
        let quiet = MockHw {
            vol: AtomicU8::new(18),
            writes: Mutex::new(Vec::new()),
        };
        assert_eq!(
            resolve_initial_volume(None, true, Some(&quiet)),
            (18, false)
        );
        assert!(quiet.writes.lock().unwrap().is_empty());

        // Exclusive + no hardware volume -> conservative 30% software gain.
        assert_eq!(resolve_initial_volume(None, true, None), (30, false));

        // Shared + loud hardware (85%) -> preserved untouched.
        let shared = MockHw {
            vol: AtomicU8::new(85),
            writes: Mutex::new(Vec::new()),
        };
        assert_eq!(
            resolve_initial_volume(None, false, Some(&shared)),
            (85, false)
        );
        assert!(shared.writes.lock().unwrap().is_empty());

        // Shared + no hardware volume -> 100% unity software gain.
        assert_eq!(resolve_initial_volume(None, false, None), (100, false));

        // Explicit initial_volume overrides hardware even when > 30%.
        let explicit = MockHw {
            vol: AtomicU8::new(100),
            writes: Mutex::new(Vec::new()),
        };
        assert_eq!(
            resolve_initial_volume(Some(55), true, Some(&explicit)),
            (55, false)
        );
        assert_eq!(*explicit.writes.lock().unwrap(), vec![55]);
    }

    #[test]
    fn pcm_tap_receives_unity_samples_and_halt() {
        use std::sync::atomic::AtomicUsize;

        struct CountTap {
            samples: AtomicUsize,
            halts: AtomicUsize,
        }
        impl PcmTap for CountTap {
            fn is_active(&self) -> bool {
                true
            }
            fn on_pcm(&self, _spec: PcmSpec, _info: &TrackInfo, samples: &[i32]) {
                self.samples.fetch_add(samples.len(), Ordering::Relaxed);
            }
            fn on_halt(&self) {
                self.halts.fetch_add(1, Ordering::Relaxed);
            }
        }

        let dir = std::env::temp_dir().join(format!("tacet-audio-tap-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let wav = dir.join("tap.wav");
        write_test_wav(&wav, 44_100, 4_410, 440.0); // 100 ms stereo = 8820 samples

        let (player, rx) = Player::new(EngineConfig {
            audio_device: "null".into(),
            exclusive: false,
            initial_volume: Some(50),
        })
        .unwrap();
        let tap = Arc::new(CountTap {
            samples: AtomicUsize::new(0),
            halts: AtomicUsize::new(0),
        });
        player.set_pcm_tap(Some(Arc::clone(&tap) as Arc<dyn PcmTap>));
        player.open(wav.to_str().unwrap(), false).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(PlaybackEvent::TrackEnded { .. }) = rx.recv_timeout(Duration::from_millis(50))
            {
                break;
            }
        }
        assert_eq!(tap.samples.load(Ordering::Relaxed), 4_410 * 2);
        assert!(tap.halts.load(Ordering::Relaxed) >= 1);
        player.close();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_pcm_writes_to_file_sink_and_cancels_cleanly() {
        let dir = std::env::temp_dir().join(format!("tacet-audio-pcm-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let out_raw = dir.join("out.raw");
        let target = format!("pcm/{}", out_raw.display());

        let (player, _rx) = Player::new(EngineConfig {
            audio_device: target,
            exclusive: false,
            initial_volume: Some(100),
        })
        .unwrap();

        let spec = PcmSpec::new(44_100, 2, 16);
        let (sid, mut handle) = player.open_pcm(spec).unwrap();
        assert!(sid > 0);
        assert_eq!(handle.spec(), spec);

        let samples = vec![0x1234_0000i32, -0x1234_0000i32, 0x2000_0000, -0x2000_0000];
        handle.write_q31(&samples).unwrap();
        handle.write_silence_frames(4).unwrap();
        thread::sleep(Duration::from_millis(40));

        // Stopping player cancels handle synchronously.
        player.stop();
        assert!(handle.is_cancelled());
        assert!(handle.write_q31(&samples).is_err());

        let bytes = fs::read(&out_raw).unwrap();
        assert!(
            bytes.len() >= 8,
            "expected written PCM bytes, got {}",
            bytes.len()
        );
        assert_eq!(i16::from_le_bytes([bytes[0], bytes[1]]), 0x1234);
        assert_eq!(i16::from_le_bytes([bytes[2], bytes[3]]), -0x1234);

        player.close();
        let _ = fs::remove_dir_all(&dir);
    }
}
