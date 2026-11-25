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
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Target audio device (e.g. `"auto"`, `"null"`, `"pcm/out.raw"`,
    /// `"coreaudio/<uid>"`, `"alsa/hw:0,0"`, `"wasapi/<id>"`).
    pub audio_device: String,
    /// Request exclusive hardware mode (macOS CoreAudio hog mode, Linux ALSA
    /// `hw:`, Windows WASAPI Exclusive).
    pub exclusive: bool,
    /// Initial software/hardware volume (0..=100).
    pub initial_volume: u8,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            audio_device: "auto".into(),
            exclusive: false,
            initial_volume: 100,
        }
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
        active: ActiveOutput,
    },
    /// Playback seamlessly transitioned to the queued next track.
    TrackChanged {
        session_id: u64,
        url: String,
        spec: PcmSpec,
        duration_secs: Option<f64>,
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
    cmd_tx: Sender<WorkerCmd>,
    worker: Option<JoinHandle<()>>,
}

impl Session {
    fn stop(mut self) {
        self.cancel.store(true, Ordering::Release);
        self.shared.stop.store(true, Ordering::Release);
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
/// standalone `tacet-dmr` daemon and embedding directly inside a Tauri app.
pub struct Player {
    cfg: EngineConfig,
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

impl Player {
    /// Create a new [`Player`] and return `(player, event_receiver)`.
    pub fn new(cfg: EngineConfig) -> Result<(Self, Receiver<PlaybackEvent>), String> {
        sink::validate_target(&cfg.audio_device)?;
        let (tx, rx) = mpsc::channel();
        let exclusive = sink::is_exclusive_target(&cfg.audio_device, cfg.exclusive);
        let hw_vol = sink::open_hardware_volume(&cfg.audio_device, exclusive);

        let mut volume = cfg.initial_volume.min(100);
        let mut muted = false;
        if let Some(hv) = &hw_vol {
            if let Ok(v) = hv.get_volume() {
                volume = v;
            }
            if let Ok(m) = hv.get_mute() {
                muted = m;
            }
        }

        let player = Self {
            cfg,
            inner: Mutex::new(PlayerInner {
                next_session_id: 1,
                session: None,
                paused: false,
                volume,
                muted,
                hw_vol,
                queued_next: None,
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

    /// Open `url` (HTTP/HTTPS or local file) and begin playback (or start paused
    /// if `paused` is true). Cancels any active session cleanly before opening
    /// the new one so exclusive hardware locks are released first.
    pub fn open(&self, url: &str, paused: bool) -> Result<u64, String> {
        // Stop any existing session outside the lock so worker join cannot deadlock.
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

        let (session_id, gain_q16) = {
            let mut st = self.inner.lock().unwrap();
            let id = st.next_session_id;
            st.next_session_id = st.next_session_id.wrapping_add(1).max(1);
            // Re-probe hardware volume on open in case a USB DAC was hotplugged.
            let exclusive = sink::is_exclusive_target(&self.cfg.audio_device, self.cfg.exclusive);
            if st.hw_vol.is_none() {
                st.hw_vol = sink::open_hardware_volume(&self.cfg.audio_device, exclusive);
            }
            let gain = apply_output_gain(&mut st);
            (id, gain)
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(StreamShared::new(paused, gain_q16));
        let (cmd_tx, cmd_rx) = mpsc::channel();

        let url_owned = url.to_string();
        let target = self.cfg.audio_device.clone();
        let exclusive = self.cfg.exclusive;
        let cancel_bg = Arc::clone(&cancel);
        let shared_bg = Arc::clone(&shared);
        let timing_bg = Arc::clone(&self.timing);
        let event_tx = self.event_tx.lock().unwrap().clone();

        let worker = thread::Builder::new()
            .name("tacet-audio-worker".into())
            .spawn(move || {
                run_worker(
                    session_id, url_owned, target, exclusive, cancel_bg, shared_bg, timing_bg,
                    cmd_rx, event_tx,
                );
            })
            .map_err(|e| format!("spawn audio worker: {e}"))?;

        let mut st = self.inner.lock().unwrap();
        st.session = Some(Session {
            id: session_id,
            cancel,
            shared,
            cmd_tx,
            worker: Some(worker),
        });
        Ok(session_id)
    }

    /// Queue a successor URL for gapless transition when the current track ends.
    pub fn queue_next(&self, url: &str) {
        let mut st = self.inner.lock().unwrap();
        st.queued_next = Some(url.to_string());
        if let Some(sess) = &st.session {
            let _ = sess
                .cmd_tx
                .send(WorkerCmd::QueueNext(Some(url.to_string())));
        }
    }

    /// Clear any previously queued gapless successor.
    pub fn clear_next(&self) {
        let mut st = self.inner.lock().unwrap();
        st.queued_next = None;
        if let Some(sess) = &st.session {
            let _ = sess.cmd_tx.send(WorkerCmd::QueueNext(None));
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
        let mut st = self.inner.lock().unwrap();
        st.paused = true;
        if let Some(sess) = &st.session {
            sess.shared.paused.store(true, Ordering::Release);
        }
    }

    /// Stop playback immediately and release the OS audio device (and hog lock).
    pub fn stop(&self) {
        let prev = {
            let mut st = self.inner.lock().unwrap();
            st.paused = false;
            st.queued_next = None;
            st.session.take()
        };
        if let Some(s) = prev {
            s.stop();
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
        sess.cmd_tx
            .send(WorkerCmd::Seek(seconds.max(0.0)))
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
    cmd_rx: Receiver<WorkerCmd>,
    event_tx: Option<Sender<PlaybackEvent>>,
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

    let mut stream: Box<dyn OutputStream> =
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

    let active_info = stream.active_output().clone();
    {
        let mut t = timing.lock().unwrap();
        t.sample_rate = cur_spec.sample_rate;
        t.base_frame = 0;
        t.base_secs = 0.0;
        t.duration_secs = decoder.info().duration_secs.unwrap_or(0.0);
        t.active = Some(active_info.clone());
    }
    emit(
        &event_tx,
        PlaybackEvent::Ready {
            session_id,
            url: initial_url,
            spec: cur_spec,
            duration_secs: decoder.info().duration_secs,
            active: active_info,
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

        let ch = cur_spec.channels.max(1) as usize;
        // Push leftover samples from the previous decoded chunk first.
        if pending_offset < pending_samples.len() {
            let written = push_samples(&mut producer, &pending_samples[pending_offset..], ch);
            pending_offset += written;
            frames_pushed += (written / ch) as u64;
            if pending_offset < pending_samples.len() {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            pending_samples.clear();
            pending_offset = 0;
        }

        // Decode next audio chunk.
        match decoder.next_chunk() {
            Ok(Some(samples)) => {
                let written = push_samples(&mut producer, samples, ch);
                frames_pushed += (written / ch) as u64;
                if written < samples.len() {
                    pending_samples.extend_from_slice(&samples[written..]);
                    pending_offset = 0;
                    thread::sleep(Duration::from_millis(5));
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
                            if next_info.spec.hardware_compatible(cur_spec) {
                                // True sample-accurate gapless transition on the same stream!
                                let boundary = frames_pushed;
                                let active = stream.active_output().clone();
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

                            // Sample rate or channel count changed: drain current track cleanly,
                            // then reconfigure hardware for the new format.
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
                            drop(stream);
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
                            stream = match sink::open_stream(
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
                            let active = stream.active_output().clone();
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
                                    && !shared.is_finished()
                                {
                                    shared.draining.store(false, Ordering::Release);
                                    let boundary = frames_pushed;
                                    let active = stream.active_output().clone();
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
                                // If incompatible format or open failed, keep draining
                                // so TrackEnded fires and the controller opens it cleanly.
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

    drop(stream);
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
            initial_volume: 80,
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
            initial_volume: 100,
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
        };
        // Changing volume while muted must keep software gain at 0.
        assert_eq!(apply_output_gain(&mut st), 0);
        st.muted = false;
        assert_eq!(apply_output_gain(&mut st), 1 << 16);
    }
}
