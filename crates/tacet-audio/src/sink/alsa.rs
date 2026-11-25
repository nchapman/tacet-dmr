//! Linux ALSA audio output backend (direct `hw:` bit-exact + `plughw:`/`default`).

use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use rtrb::Consumer;

use crate::format::{PcmSpec, SampleFormat, pack_interleaved};
use crate::sink::{ActiveOutput, OutputDevice, OutputStream, StreamShared};

/// Enumerate available Linux ALSA playback devices.
pub fn list_devices() -> Vec<OutputDevice> {
    let mut out = vec![OutputDevice {
        id: "alsa/default".into(),
        description: "ALSA Default Output".into(),
        is_default: true,
    }];
    if let Ok(hints) = alsa::device_name::HintIter::new_str(None, "pcm") {
        for hint in hints {
            let Some(name) = hint.name else { continue };
            if matches!(hint.direction, Some(Direction::Capture)) {
                continue;
            }
            if name == "null" || name == "default" {
                continue;
            }
            let desc = hint
                .desc
                .unwrap_or_else(|| name.clone())
                .replace('\n', " — ");
            out.push(OutputDevice {
                id: format!("alsa/{name}"),
                description: desc,
                is_default: false,
            });
        }
    }
    out
}

fn candidate_formats(want_bits: u16) -> Vec<(Format, SampleFormat)> {
    // Explicit little-endian ALSA formats matching pack_interleaved output.
    if want_bits <= 16 {
        vec![
            (Format::S16LE, SampleFormat::S16),
            (Format::S243LE, SampleFormat::S24Packed),
            (Format::S24LE, SampleFormat::S24In32Right),
            (Format::S32LE, SampleFormat::S32),
            (Format::FloatLE, SampleFormat::F32),
        ]
    } else if want_bits <= 24 {
        vec![
            (Format::S243LE, SampleFormat::S24Packed),
            (Format::S24LE, SampleFormat::S24In32Right),
            (Format::S32LE, SampleFormat::S32),
            (Format::FloatLE, SampleFormat::F32),
            (Format::S16LE, SampleFormat::S16),
        ]
    } else {
        vec![
            (Format::S32LE, SampleFormat::S32),
            (Format::S243LE, SampleFormat::S24Packed),
            (Format::S24LE, SampleFormat::S24In32Right),
            (Format::FloatLE, SampleFormat::F32),
            (Format::S16LE, SampleFormat::S16),
        ]
    }
}

pub struct AlsaStream {
    active: ActiveOutput,
    shared: Arc<StreamShared>,
    worker: Option<JoinHandle<()>>,
}

pub fn open_stream(
    name: &str,
    exclusive: bool,
    spec: PcmSpec,
    mut consumer: Consumer<i32>,
    shared: Arc<StreamShared>,
) -> Result<Box<dyn OutputStream>, String> {
    let is_hw = name.starts_with("hw:");
    let c_name = CString::new(name).map_err(|_| format!("invalid ALSA device name {name:?}"))?;
    let pcm = PCM::open(
        c_name.to_str().unwrap_or("default"),
        Direction::Playback,
        false,
    )
    .map_err(|e| format!("ALSA open {name:?} failed: {e}"))?;

    let (actual_rate, actual_ch, chosen_fmt, period_frames, can_pause) = {
        let hwp = HwParams::any(&pcm).map_err(|e| format!("ALSA HwParams::any: {e}"))?;
        hwp.set_access(Access::RWInterleaved)
            .map_err(|e| format!("ALSA set_access: {e}"))?;

        let mut picked = None;
        for (alsa_fmt, our_fmt) in candidate_formats(spec.bits_per_sample) {
            if hwp.set_format(alsa_fmt).is_ok() {
                picked = Some(our_fmt);
                break;
            }
        }
        let chosen_fmt =
            picked.ok_or_else(|| format!("no supported ALSA PCM format on {name:?}"))?;

        hwp.set_channels(spec.channels as u32)
            .map_err(|e| format!("ALSA set_channels({}): {e}", spec.channels))?;
        let actual_rate = hwp
            .set_rate_near(spec.sample_rate, ValueOr::Nearest)
            .map_err(|e| format!("ALSA set_rate({}): {e}", spec.sample_rate))?;
        if (is_hw || exclusive) && actual_rate != spec.sample_rate {
            return Err(format!(
                "ALSA device {name:?} does not support {} Hz (nearest: {actual_rate} Hz)",
                spec.sample_rate
            ));
        }

        let _ = hwp.set_buffer_time_near(100_000, ValueOr::Nearest);
        let _ = hwp.set_period_time_near(20_000, ValueOr::Nearest);
        pcm.hw_params(&hwp)
            .map_err(|e| format!("ALSA apply hw_params: {e}"))?;

        let applied = pcm
            .hw_params_current()
            .map_err(|e| format!("ALSA hw_params_current: {e}"))?;
        let pf = applied.get_period_size().unwrap_or(1024).max(256) as usize;
        let ch = applied.get_channels().unwrap_or(spec.channels as u32) as u16;
        let can_pause = applied.can_pause();
        (actual_rate, ch, chosen_fmt, pf, can_pause)
    };

    pcm.prepare().map_err(|e| format!("ALSA prepare: {e}"))?;

    let active = ActiveOutput {
        device_id: format!("alsa/{name}"),
        device_name: format!("ALSA ({name})"),
        sample_rate: actual_rate,
        channels: actual_ch,
        sample_format: chosen_fmt,
        exclusive: is_hw,
        hardware_volume: false,
    };

    let shared_bg = Arc::clone(&shared);
    let worker = thread::Builder::new()
        .name("tacet-alsa-sink".into())
        .spawn(move || {
            let io = match pcm.io_bytes() {
                io => io,
            };
            let ch = actual_ch.max(1) as usize;
            let bps = chosen_fmt.bytes_per_sample();
            let frame_bytes = ch * bps;
            let mut pcm_buf = vec![0i32; period_frames * ch];
            let mut bytes = vec![0u8; period_frames * frame_bytes];
            let silence = vec![0u8; period_frames * frame_bytes];
            let mut was_paused = false;

            while !shared_bg.is_stopped() {
                shared_bg.maybe_flush(&mut consumer);

                if shared_bg.is_paused() {
                    if !was_paused {
                        if can_pause {
                            let _ = pcm.pause(true);
                        } else {
                            let _ = pcm.drop();
                        }
                        was_paused = true;
                    }
                    thread::sleep(Duration::from_millis(10));
                    continue;
                } else if was_paused {
                    if can_pause {
                        if let Err(e) = pcm.pause(false) {
                            let _ = pcm.try_recover(e, true);
                        }
                    } else {
                        let _ = pcm.prepare();
                    }
                    was_paused = false;
                }

                let avail_frames = consumer.slots() / ch;
                if avail_frames == 0 {
                    if shared_bg.is_draining() {
                        if !shared_bg.is_finished() {
                            let _ = pcm.drain();
                            shared_bg.finished.store(true, Ordering::Release);
                        }
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    shared_bg.underruns.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = io.writei(&silence) {
                        if pcm.try_recover(e, true).is_err() {
                            shared_bg.failed.store(true, Ordering::Release);
                            break;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    continue;
                }

                let take_frames = avail_frames.min(period_frames);
                let take_samples = take_frames * ch;
                if let Ok(chunk) = consumer.read_chunk(take_samples) {
                    let (s1, s2) = chunk.as_slices();
                    let n1 = s1.len();
                    pcm_buf[..n1].copy_from_slice(s1);
                    pcm_buf[n1..n1 + s2.len()].copy_from_slice(s2);
                    chunk.commit_all();
                }

                let gain = shared_bg.gain_q16();
                let total_bytes = take_frames * frame_bytes;
                pack_interleaved(
                    &pcm_buf[..take_samples],
                    &mut bytes[..total_bytes],
                    chosen_fmt,
                    gain,
                );

                // Write all frames, handling short writes and XRUN/suspend recovery.
                let mut written_frames = 0usize;
                let mut unrecoverable = false;
                while written_frames < take_frames && !shared_bg.is_stopped() {
                    let byte_off = written_frames * frame_bytes;
                    match io.writei(&bytes[byte_off..total_bytes]) {
                        Ok(0) => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Ok(n) => {
                            written_frames += n;
                            shared_bg
                                .frames_played
                                .fetch_add(n as u64, Ordering::Relaxed);
                        }
                        Err(e) => {
                            if pcm.try_recover(e, true).is_err() {
                                unrecoverable = true;
                                break;
                            }
                        }
                    }
                }
                if unrecoverable {
                    shared_bg.failed.store(true, Ordering::Release);
                    break;
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
