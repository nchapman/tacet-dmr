//! Real-time-paced null sink and WAV/PCM file sink for headless testing and CI.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rtrb::Consumer;

use crate::format::{PcmSpec, SampleFormat, pack_interleaved};
use crate::sink::{ActiveOutput, OutputStream, StreamShared};

/// Real-time paced null audio sink that advances `frames_played` at the exact
/// stream sample rate without touching OS audio hardware.
pub struct NullStream {
    active: ActiveOutput,
    shared: Arc<StreamShared>,
    worker: Option<JoinHandle<()>>,
}

impl NullStream {
    pub fn open(spec: PcmSpec, mut consumer: Consumer<i32>, shared: Arc<StreamShared>) -> Self {
        let active = ActiveOutput {
            device_id: "null".into(),
            device_name: "Null Output".into(),
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            sample_format: SampleFormat::S32,
            exclusive: false,
            hardware_volume: false,
        };
        let shared_bg = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("tacet-null-sink".into())
            .spawn(move || {
                let channels = spec.channels.max(1) as usize;
                let rate = spec.sample_rate.max(1) as u64;
                let quantum_frames = ((rate / 100).max(64)) as usize; // ~10 ms
                let quantum_dur = Duration::from_secs_f64(quantum_frames as f64 / rate as f64);
                let mut next_tick = Instant::now();

                while !shared_bg.is_stopped() {
                    shared_bg.maybe_flush(&mut consumer);
                    if shared_bg.is_paused() {
                        thread::sleep(Duration::from_millis(10));
                        next_tick = Instant::now();
                        continue;
                    }

                    let avail_samples = consumer.slots();
                    let avail_frames = avail_samples / channels;
                    if avail_frames == 0 {
                        if shared_bg.is_draining() {
                            shared_bg.finished.store(true, Ordering::Release);
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                        next_tick = Instant::now();
                        continue;
                    }

                    let take_frames = avail_frames.min(quantum_frames);
                    let take_samples = take_frames * channels;
                    if let Ok(chunk) = consumer.read_chunk(take_samples) {
                        chunk.commit_all();
                    }
                    shared_bg
                        .frames_played
                        .fetch_add(take_frames as u64, Ordering::Relaxed);

                    let step = Duration::from_secs_f64(take_frames as f64 / rate as f64);
                    next_tick += step;
                    let now = Instant::now();
                    if next_tick > now {
                        // Sleep in short slices so stop/pause stays responsive.
                        let mut rem = next_tick - now;
                        while rem > Duration::ZERO && !shared_bg.is_stopped() {
                            let s = rem.min(quantum_dur.min(Duration::from_millis(10)));
                            thread::sleep(s);
                            rem = rem.saturating_sub(s);
                        }
                    } else if now.duration_since(next_tick) > Duration::from_millis(200) {
                        next_tick = now;
                    }
                }
            })
            .expect("spawn null sink thread");

        Self {
            active,
            shared,
            worker: Some(worker),
        }
    }
}

impl OutputStream for NullStream {
    fn active_output(&self) -> &ActiveOutput {
        &self.active
    }
}

impl Drop for NullStream {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Sink that writes packed S16LE PCM directly to a file (unpaced) for
/// deterministic automated audio verification.
pub struct PcmFileStream {
    active: ActiveOutput,
    shared: Arc<StreamShared>,
    worker: Option<JoinHandle<()>>,
}

impl PcmFileStream {
    pub fn open(
        path: &str,
        spec: PcmSpec,
        mut consumer: Consumer<i32>,
        shared: Arc<StreamShared>,
    ) -> Result<Self, String> {
        let file = File::create(path).map_err(|e| format!("create pcm file {path}: {e}"))?;
        let active = ActiveOutput {
            device_id: format!("pcm/{path}"),
            device_name: format!("PCM File ({path})"),
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            sample_format: SampleFormat::S16,
            exclusive: false,
            hardware_volume: false,
        };

        let shared_bg = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("tacet-pcm-sink".into())
            .spawn(move || {
                let mut writer = BufWriter::new(file);
                let channels = spec.channels.max(1) as usize;
                let mut tmp = Vec::with_capacity(4096);
                let mut bytes = Vec::with_capacity(8192);

                while !shared_bg.is_stopped() {
                    shared_bg.maybe_flush(&mut consumer);
                    if shared_bg.is_paused() {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    let avail_frames = consumer.slots() / channels;
                    if avail_frames == 0 {
                        if shared_bg.is_draining() {
                            let _ = writer.flush();
                            shared_bg.finished.store(true, Ordering::Release);
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    let take_frames = avail_frames.min(1024);
                    let take_samples = take_frames * channels;
                    tmp.clear();
                    if let Ok(chunk) = consumer.read_chunk(take_samples) {
                        let (s1, s2) = chunk.as_slices();
                        tmp.extend_from_slice(s1);
                        tmp.extend_from_slice(s2);
                        chunk.commit_all();
                    }
                    bytes.resize(tmp.len() * 2, 0);
                    let gain = shared_bg.gain_q16();
                    let n = pack_interleaved(&tmp, &mut bytes, SampleFormat::S16, gain);
                    if writer.write_all(&bytes[..n]).is_err() {
                        break;
                    }
                    shared_bg
                        .frames_played
                        .fetch_add(take_frames as u64, Ordering::Relaxed);
                }
                let _ = writer.flush();
            })
            .map_err(|e| format!("spawn pcm sink thread: {e}"))?;

        Ok(Self {
            active,
            shared,
            worker: Some(worker),
        })
    }
}

impl OutputStream for PcmFileStream {
    fn active_output(&self) -> &ActiveOutput {
        &self.active
    }
}

impl Drop for PcmFileStream {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}
