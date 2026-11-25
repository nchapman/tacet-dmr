//! Native Linux ALSA audio output backend.
//!
//! Supports direct `hw:` / `plughw:` hardware access (including `S24_3LE`
//! 3-byte packed 24-bit USB DACs, `S24_LE`, `S32_LE`, `S16_LE`, and `FLOAT_LE`)
//! as well as `default` (PulseAudio/PipeWire/dmix).

use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use alsa::pcm::{Access, Format as AlsaFormat, HwParams, PCM};
use alsa::{Direction, ValueOr};
use rtrb::Consumer;

use crate::format::{PcmSpec, SampleFormat, pack_interleaved};
use crate::sink::{ActiveOutput, OutputDevice, OutputStream, StreamShared};

/// Enumerate ALSA PCM playback devices.
pub fn list_devices() -> Vec<OutputDevice> {
    let mut out = Vec::new();
    let Ok(hints) = alsa::device_name::HintIter::new_str(None, "pcm") else {
        return out;
    };
    for hint in hints {
        if let Some(dir) = hint.direction
            && dir != Direction::Playback
        {
            continue;
        }
        let Some(name) = hint.name else { continue };
        let desc = hint
            .desc
            .unwrap_or_else(|| name.clone())
            .replace('\n', " — ");
        let is_default = name == "default";
        out.push(OutputDevice {
            id: format!("alsa/{name}"),
            description: desc,
            is_default,
        });
    }
    out
}

pub struct AlsaStream {
    active: ActiveOutput,
    shared: Arc<StreamShared>,
    worker: Option<JoinHandle<()>>,
}

/// Choose the highest-fidelity ALSA format supported by the hardware for `spec`.
fn select_alsa_format(hwp: &HwParams, spec: PcmSpec) -> Option<(AlsaFormat, SampleFormat)> {
    let candidates: &[(AlsaFormat, SampleFormat)] = if spec.bits_per_sample <= 16 {
        &[
            (AlsaFormat::s16(), SampleFormat::S16),
            (AlsaFormat::s24_3(), SampleFormat::S24Packed),
            (AlsaFormat::s24(), SampleFormat::S24In32Right),
            (AlsaFormat::s32(), SampleFormat::S32),
            (AlsaFormat::float(), SampleFormat::F32),
        ]
    } else if spec.bits_per_sample <= 24 {
        &[
            (AlsaFormat::s24_3(), SampleFormat::S24Packed),
            (AlsaFormat::s24(), SampleFormat::S24In32Right),
            (AlsaFormat::s32(), SampleFormat::S32),
            (AlsaFormat::float(), SampleFormat::F32),
            (AlsaFormat::s16(), SampleFormat::S16),
        ]
    } else {
        &[
            (AlsaFormat::s32(), SampleFormat::S32),
            (AlsaFormat::float(), SampleFormat::F32),
            (AlsaFormat::s24_3(), SampleFormat::S24Packed),
            (AlsaFormat::s24(), SampleFormat::S24In32Right),
            (AlsaFormat::s16(), SampleFormat::S16),
        ]
    };

    for &(af, sf) in candidates {
        if hwp.test_format(af).is_ok() {
            return Some((af, sf));
        }
    }
    None
}

pub fn open_stream(
    device_name: &str,
    exclusive: bool,
    spec: PcmSpec,
    mut consumer: Consumer<i32>,
    shared: Arc<StreamShared>,
) -> Result<Box<dyn OutputStream>, String> {
    let c_name = CString::new(device_name)
        .map_err(|_| format!("invalid ALSA device name {device_name:?}"))?;
    let pcm = PCM::open(&c_name, Direction::Playback, false)
        .map_err(|e| format!("ALSA open {device_name:?}: {e}"))?;

    let (sample_format, actual_rate, actual_ch, period_frames) = {
        let hwp = HwParams::any(&pcm).map_err(|e| format!("ALSA hw_params any: {e}"))?;
        hwp.set_access(Access::RWInterleaved)
            .map_err(|e| format!("ALSA set_access: {e}"))?;
        let (af, sf) = select_alsa_format(&hwp, spec)
            .ok_or_else(|| format!("ALSA device {device_name:?} has no supported PCM format"))?;
        hwp.set_format(af)
            .map_err(|e| format!("ALSA set_format: {e}"))?;
        hwp.set_channels(spec.channels as u32)
            .map_err(|e| format!("ALSA set_channels: {e}"))?;
        let actual_rate = hwp
            .set_rate_near(spec.sample_rate, ValueOr::Nearest)
            .map_err(|e| format!("ALSA set_rate: {e}"))?;
        let _ = hwp.set_buffer_time_near(100_000, ValueOr::Nearest);
        let _ = hwp.set_period_time_near(20_000, ValueOr::Nearest);
        pcm.hw_params(&hwp)
            .map_err(|e| format!("ALSA apply hw_params: {e}"))?;
        let cur = pcm
            .hw_params_current()
            .map_err(|e| format!("ALSA hw_params_current: {e}"))?;
        let period = cur.get_period_size().unwrap_or(1024).max(256) as usize;
        let ch = cur.get_channels().unwrap_or(spec.channels as u32) as u16;
        (sf, actual_rate, ch, period)
    };

    pcm.prepare().map_err(|e| format!("ALSA prepare: {e}"))?;

    let is_hw = exclusive || device_name.starts_with("hw:");
    let active = ActiveOutput {
        device_id: format!("alsa/{device_name}"),
        device_name: device_name.to_string(),
        sample_rate: actual_rate,
        channels: actual_ch,
        sample_format,
        exclusive: is_hw,
        hardware_volume: false,
    };

    let shared_bg = Arc::clone(&shared);
    let worker = thread::Builder::new()
        .name("tacet-alsa-sink".into())
        .spawn(move || {
            let channels = actual_ch.max(1) as usize;
            let bps = sample_format.bytes_per_sample();
            let frame_bytes = bps * channels;
            let mut tmp = vec![0i32; period_frames * channels];
            let mut bytes = vec![0u8; period_frames * frame_bytes];
            let silence = vec![0u8; period_frames * frame_bytes];

            let io = pcm.io_bytes();

            while !shared_bg.is_stopped() {
                shared_bg.maybe_flush(&mut consumer);
                if shared_bg.is_paused() {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }

                let avail_frames = consumer.slots() / channels;
                if avail_frames == 0 {
                    if shared_bg.is_draining() {
                        let _ = pcm.drain();
                        shared_bg.finished.store(true, Ordering::Release);
                        break;
                    }
                    shared_bg.underruns.fetch_add(1, Ordering::Relaxed);
                    let _ = io.writei(&silence);
                    continue;
                }

                let take_frames = avail_frames.min(period_frames);
                let take_samples = take_frames * channels;
                if let Ok(chunk) = consumer.read_chunk(take_samples) {
                    let (s1, s2) = chunk.as_slices();
                    tmp[..s1.len()].copy_from_slice(s1);
                    tmp[s1.len()..s1.len() + s2.len()].copy_from_slice(s2);
                    chunk.commit_all();
                }

                let gain = shared_bg.gain_q16();
                let n = pack_interleaved(&tmp[..take_samples], &mut bytes, sample_format, gain);
                match io.writei(&bytes[..n]) {
                    Ok(_) => {
                        shared_bg
                            .frames_played
                            .fetch_add(take_frames as u64, Ordering::Relaxed);
                    }
                    Err(e) => {
                        if pcm.try_recover(e, true).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = pcm.drop();
        })
        .map_err(|e| format!("spawn ALSA thread: {e}"))?;

    Ok(Box::new(AlsaStream {
        active,
        shared,
        worker: Some(worker),
    }))
}

impl OutputStream for AlsaStream {
    fn active_output(&self) -> &ActiveOutput {
        &self.active
    }
}

impl Drop for AlsaStream {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}
