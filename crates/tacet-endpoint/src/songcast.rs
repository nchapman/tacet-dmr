//! Songcast (`OHM`/`OHU`/`OHZ`) Sender tap and Receiver worker bridging
//! `openhome-songcast` directly onto `tacet-audio`'s Q31 integer pipeline.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use openhome_core::{ReceiverState, TrackDetails};
use openhome_songcast::{
    AudioFormat, ReceiverConfig, ReceiverEvent, SongcastReceiver, SongcastSender, SongcastUri,
};
use tacet_audio::{
    DriftResampler, PcmSpec, PcmStreamHandle, PcmTap, Player, TrackInfo, pcm_be_into_q31,
    q31_into_pcm_be,
};
use tokio::sync::mpsc as tokio_mpsc;

/// Maximum silence pre-buffer lead applied on Songcast stream start / resync
/// to align the first audio sample with the sender's scheduled `playout_at`.
const MAX_SONGCAST_PREBUFFER: Duration = Duration::from_millis(250);

enum TapCmd {
    Audio { fmt: AudioFormat, payload: Vec<u8> },
    Halt { fmt: AudioFormat },
    SetTrack { uri: String, metadata: String },
}

/// Pre-gain [`PcmTap`] that rebroadcasts decoded `tacet-audio` Q31 PCM over a
/// [`SongcastSender`] before local volume scaling is applied.
///
/// All `Audio`, `Halt`, and `SetTrack` commands flow through a single ordered
/// channel so `HALT` and track metadata never reorder against audio packets.
pub struct SongcastTap {
    sender: Arc<SongcastSender>,
    cmd_tx: tokio_mpsc::UnboundedSender<TapCmd>,
    enabled: AtomicBool,
    state: Mutex<TapState>,
}

struct TapState {
    last_fmt: Option<AudioFormat>,
}

impl SongcastTap {
    pub fn new(sender: Arc<SongcastSender>, runtime: tokio::runtime::Handle) -> Self {
        let (cmd_tx, mut cmd_rx) = tokio_mpsc::unbounded_channel::<TapCmd>();
        let sender_bg = Arc::clone(&sender);
        runtime.spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    TapCmd::Audio { fmt, payload } => {
                        let _ = sender_bg.send_audio(&fmt, payload, false).await;
                    }
                    TapCmd::Halt { fmt } => {
                        let _ = sender_bg.send_audio(&fmt, Vec::new(), true).await;
                    }
                    TapCmd::SetTrack { uri, metadata } => {
                        let _ = sender_bg.set_track(uri, metadata).await;
                    }
                }
            }
        });
        Self {
            sender,
            cmd_tx,
            enabled: AtomicBool::new(true),
            state: Mutex::new(TapState { last_fmt: None }),
        }
    }

    /// Enable or disable rebroadcasting (e.g. disabled when standby or non-Playlist source is active).
    pub fn set_enabled(&self, enabled: bool) {
        let prev = self.enabled.swap(enabled, Ordering::AcqRel);
        if prev && !enabled {
            self.on_halt();
        }
    }

    /// Broadcast updated track URI and DIDL-Lite metadata to connected Songcast followers.
    pub fn set_track(&self, uri: String, metadata: String) {
        let _ = self.cmd_tx.send(TapCmd::SetTrack { uri, metadata });
    }
}

impl PcmTap for SongcastTap {
    fn is_active(&self) -> bool {
        self.enabled.load(Ordering::Acquire) && self.sender.active_receivers() > 0
    }

    fn on_pcm(&self, spec: PcmSpec, info: &TrackInfo, samples: &[i32]) {
        if !self.is_active() || samples.is_empty() || spec.sample_rate == 0 || spec.channels == 0 {
            return;
        }
        let channels = (spec.channels as u8).max(1);
        // Preserve 24-bit PCM for >16-bit sources (20/24/32-bit, matching ohPipeline's 24-bit cap).
        let bit_depth: u8 = if spec.bits_per_sample > 16 { 24 } else { 16 };
        let dur_secs = info.duration_secs.unwrap_or(0.0).max(0.0);
        let fmt = AudioFormat {
            sample_rate: spec.sample_rate,
            bit_depth,
            channels,
            bit_rate: 0,
            lossless: info.lossless,
            codec: info.codec.clone(),
            volume_offset: 0,
            samples_total: (dur_secs * spec.sample_rate as f64).round() as u64,
        };

        // Chunk into ~5 ms UDP packets capped at <=1360 payload bytes so high-rate / 24-bit
        // streams stay below Ethernet MTU without IP fragmentation.
        let bytes_per_frame = usize::from(channels) * usize::from(bit_depth / 8);
        let mtu_max_frames = (1360 / bytes_per_frame.max(1)).max(1);
        let frames_per_pkt = (spec.sample_rate as usize / 200).max(1).min(mtu_max_frames);
        let samples_per_pkt = frames_per_pkt * usize::from(channels);

        {
            let mut st = self.state.lock().unwrap();
            st.last_fmt = Some(fmt.clone());
        }
        for chunk in samples.chunks(samples_per_pkt) {
            let mut payload = Vec::with_capacity(chunk.len() * usize::from(bit_depth / 8));
            q31_into_pcm_be(chunk, bit_depth, &mut payload);
            if !payload.is_empty() {
                let _ = self.cmd_tx.send(TapCmd::Audio {
                    fmt: fmt.clone(),
                    payload,
                });
            }
        }
    }

    fn on_halt(&self) {
        let last = self.state.lock().unwrap().last_fmt.take();
        if let Some(fmt) = last
            && self.sender.active_receivers() > 0
        {
            let _ = self.cmd_tx.send(TapCmd::Halt { fmt });
        }
    }
}

/// Events emitted from a running Songcast Receiver worker back to the coordinator.
#[derive(Debug, Clone)]
pub enum SongcastEvent {
    State {
        generation: u64,
        state: ReceiverState,
    },
    Track {
        generation: u64,
        uri: String,
        metadata: String,
    },
    Metatext {
        generation: u64,
        metatext: String,
    },
    Details {
        generation: u64,
        details: TrackDetails,
    },
    Time {
        generation: u64,
        seconds: u32,
    },
    Error {
        generation: u64,
        message: String,
    },
}

/// Handle to a running Songcast Receiver OS thread.
pub struct SongcastReceiverWorker {
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SongcastReceiverWorker {
    pub fn spawn(
        generation: u64,
        uri: SongcastUri,
        interface: Ipv4Addr,
        player: Arc<Player>,
        runtime: tokio::runtime::Handle,
        ev_tx: tokio_mpsc::UnboundedSender<SongcastEvent>,
    ) -> Result<Self, String> {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_bg = Arc::clone(&cancel);
        let handle = thread::Builder::new()
            .name("tacet-songcast-rx".into())
            .spawn(move || {
                receiver_loop(
                    generation, uri, interface, player, runtime, cancel_bg, ev_tx,
                );
            })
            .map_err(|e| format!("spawn songcast receiver worker: {e}"))?;
        Ok(Self {
            cancel,
            handle: Some(handle),
        })
    }

    pub fn stop(mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let deadline = Instant::now() + Duration::from_millis(250);
            while !h.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if h.is_finished() {
                let _ = h.join();
            } else {
                let _ = thread::Builder::new()
                    .name("tacet-songcast-reaper".into())
                    .spawn(move || {
                        let _ = h.join();
                    });
            }
        }
    }
}

impl Drop for SongcastReceiverWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

fn receiver_loop(
    generation: u64,
    uri: SongcastUri,
    interface: Ipv4Addr,
    player: Arc<Player>,
    runtime: tokio::runtime::Handle,
    cancel: Arc<AtomicBool>,
    ev_tx: tokio_mpsc::UnboundedSender<SongcastEvent>,
) {
    let cfg = ReceiverConfig {
        interface,
        ..ReceiverConfig::default()
    };
    let mut receiver = match runtime.block_on(SongcastReceiver::connect(&uri, cfg)) {
        Ok(r) => r,
        Err(err) => {
            if !cancel.load(Ordering::Acquire) {
                let _ = ev_tx.send(SongcastEvent::Error {
                    generation,
                    message: err.to_string(),
                });
            }
            return;
        }
    };

    let _ = ev_tx.send(SongcastEvent::State {
        generation,
        state: ReceiverState::Waiting,
    });

    let mut stream: Option<PcmStreamHandle> = None;
    let mut last_spec: Option<PcmSpec> = None;
    let mut last_details: Option<TrackDetails> = None;
    let mut playing = false;
    let mut prebuffered = false;
    let mut drift_resampler: Option<DriftResampler> = None;
    let mut pi_integral: f64 = 0.0;
    let mut last_underruns: u64 = 0;
    let mut q31_buf: Vec<i32> = Vec::with_capacity(8192);
    let mut drift_buf: Vec<i32> = Vec::new();
    let mut last_reported_sec: u32 = u32::MAX;

    while !cancel.load(Ordering::Acquire) {
        let ev = match receiver.try_recv() {
            Ok(ev) => Some(ev),
            Err(tokio_mpsc::error::TryRecvError::Empty) => {
                match runtime.block_on(async {
                    tokio::time::timeout(Duration::from_millis(15), receiver.recv()).await
                }) {
                    Ok(opt) => opt,
                    Err(_) => continue,
                }
            }
            Err(tokio_mpsc::error::TryRecvError::Disconnected) => None,
        };

        let Some(ev) = ev else {
            break;
        };

        match ev {
            ReceiverEvent::Audio {
                frame,
                playout_at,
                skew_ppm,
            } => {
                if frame.flags.is_halt() {
                    if let Some(s) = stream.as_ref() {
                        s.flush();
                    }
                    prebuffered = false;
                    if let Some(r) = drift_resampler.as_mut() {
                        r.reset();
                    }
                    if playing {
                        playing = false;
                        let _ = ev_tx.send(SongcastEvent::State {
                            generation,
                            state: ReceiverState::Waiting,
                        });
                    }
                    continue;
                }
                if frame.sample_rate == 0 || frame.audio.is_empty() {
                    continue;
                }

                let spec = PcmSpec::new(
                    frame.sample_rate,
                    u16::from(frame.channels.max(1)),
                    u16::from(frame.bit_depth.max(16)),
                );
                let bit_depth_ok = stream.as_ref().is_some_and(|s| {
                    s.active_output().sample_format.bit_depth() >= spec.bits_per_sample
                });
                if !last_spec.is_some_and(|s| s.hardware_compatible(spec)) || !bit_depth_ok {
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    // Drop old stream BEFORE opening new one to avoid EBUSY on exclusive DACs.
                    stream = None;
                    prebuffered = false;
                    drift_resampler = None;
                    pi_integral = 0.0;
                    match player.open_pcm(spec) {
                        Ok((_, handle)) => {
                            last_underruns = handle.underruns();
                            stream = Some(handle);
                            last_spec = Some(spec);
                        }
                        Err(err) => {
                            if !cancel.load(Ordering::Acquire) {
                                let _ = ev_tx.send(SongcastEvent::Error {
                                    generation,
                                    message: err,
                                });
                            }
                            break;
                        }
                    }
                }

                let details = TrackDetails {
                    duration: if frame.samples_total > 0 {
                        u32::try_from(frame.samples_total / u64::from(frame.sample_rate))
                            .unwrap_or(u32::MAX)
                    } else {
                        0
                    },
                    bit_rate: frame.bit_rate,
                    bit_depth: u32::from(frame.bit_depth),
                    sample_rate: frame.sample_rate,
                    lossless: frame.flags.is_lossless(),
                    codec_name: if frame.codec.is_empty() {
                        "PCM".into()
                    } else {
                        frame.codec.clone()
                    },
                };
                if last_details.as_ref() != Some(&details) {
                    last_details = Some(details.clone());
                    let _ = ev_tx.send(SongcastEvent::Details {
                        generation,
                        details,
                    });
                }

                if !playing {
                    playing = true;
                    let _ = ev_tx.send(SongcastEvent::State {
                        generation,
                        state: ReceiverState::Playing,
                    });
                }

                let sec = (frame.sample_start / u64::from(frame.sample_rate)) as u32;
                if sec != last_reported_sec {
                    last_reported_sec = sec;
                    let _ = ev_tx.send(SongcastEvent::Time {
                        generation,
                        seconds: sec,
                    });
                }

                // Pre-buffer silence up to `playout_at` on stream start / resync so the
                // first real Songcast sample hits the DAC at the scheduled instant.
                if !prebuffered {
                    prebuffered = true;
                    let lead = playout_at
                        .saturating_duration_since(Instant::now())
                        .min(MAX_SONGCAST_PREBUFFER);
                    if lead >= Duration::from_millis(2)
                        && let Some(s) = stream.as_mut()
                    {
                        let lead_frames = ((lead.as_micros() * u128::from(spec.sample_rate))
                            / 1_000_000) as usize;
                        if s.write_silence_frames(lead_frames).is_err() {
                            break;
                        }
                    }
                }

                pcm_be_into_q31(&frame.audio, frame.bit_depth, &mut q31_buf);
                if !q31_buf.is_empty()
                    && let Some(s) = stream.as_mut()
                {
                    let cap_frames = s.capacity_frames().max(1);
                    let cur_underruns = s.underruns();
                    if cur_underruns > last_underruns {
                        last_underruns = cur_underruns;
                        pi_integral = 0.0;
                    }
                    let buf_frames = s.buffered_frames();
                    // Overrun guard: if ringbuffer exceeds 85% capacity, drop this chunk
                    // rather than accumulating unbounded multi-room latency or stalling UDP.
                    if buf_frames * 100 >= cap_frames * 85 {
                        pi_integral = 0.0;
                        continue;
                    }
                    // Closed-loop PI controller on local DAC ringbuffer occupancy + sender
                    // `skew_ppm` feedforward. Target ~100 ms buffered frames (clamped to
                    // 15%..50% of ringbuffer capacity).
                    let target_frames =
                        ((spec.sample_rate as usize) / 10).clamp(cap_frames / 7, cap_frames / 2);
                    let err_secs =
                        (buf_frames as f64 - target_frames as f64) / f64::from(spec.sample_rate);
                    let dt_secs = (q31_buf.len() / usize::from(spec.channels.max(1))) as f64
                        / f64::from(spec.sample_rate);
                    pi_integral = (pi_integral + err_secs * dt_secs).clamp(-0.5, 0.5);
                    let ff_ppm = if skew_ppm.is_finite() { skew_ppm } else { 0.0 };
                    let total_ppm =
                        (ff_ppm + 150.0 * err_secs + 15.0 * pi_integral).clamp(-1000.0, 1000.0);

                    let out_samples: &[i32] = if total_ppm.abs() >= 2.0 {
                        let r = drift_resampler
                            .get_or_insert_with(|| DriftResampler::new(spec.channels));
                        r.set_rate_adjust_ppm(total_ppm);
                        r.process(&q31_buf, &mut drift_buf);
                        &drift_buf
                    } else {
                        if let Some(r) = drift_resampler.as_mut() {
                            r.update_passthrough(&q31_buf);
                        }
                        &q31_buf
                    };

                    if let Err(err) = s.write_q31(out_samples) {
                        if !cancel.load(Ordering::Acquire) {
                            let _ = ev_tx.send(SongcastEvent::Error {
                                generation,
                                message: err,
                            });
                        }
                        break;
                    }
                }
            }
            ReceiverEvent::Track(t) => {
                let _ = ev_tx.send(SongcastEvent::Track {
                    generation,
                    uri: t.uri,
                    metadata: t.metadata,
                });
            }
            ReceiverEvent::Metatext(m) => {
                let _ = ev_tx.send(SongcastEvent::Metatext {
                    generation,
                    metatext: m.metatext,
                });
            }
            ReceiverEvent::Resynced => {
                if let Some(s) = stream.as_ref() {
                    s.flush();
                }
                prebuffered = false;
                if let Some(r) = drift_resampler.as_mut() {
                    r.reset();
                }
            }
        }
    }

    // Release hardware output stream immediately before async Songcast Leave.
    drop(stream);
    if !cancel.load(Ordering::Acquire) {
        let _ = ev_tx.send(SongcastEvent::State {
            generation,
            state: ReceiverState::Stopped,
        });
    }
    runtime.spawn(async move {
        receiver.shutdown().await;
    });
}
