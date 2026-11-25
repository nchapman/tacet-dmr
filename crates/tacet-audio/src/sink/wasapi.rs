//! Windows WASAPI audio output backend (Exclusive & Shared modes).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rtrb::Consumer;
use wasapi::{
    Device, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat, deinitialize,
    initialize_mta,
};

use crate::format::{PcmSpec, SampleFormat, pack_interleaved};
use crate::sink::{ActiveOutput, HardwareVolume, OutputDevice, OutputStream, StreamShared};

/// RAII guard for COM MTA initialization on a worker thread.
/// Calls `deinitialize()` on drop ONLY if `initialize_mta()` succeeded
/// (never after `RPC_E_CHANGED_MODE`).
struct ComMtaGuard {
    should_uninit: bool,
}

impl ComMtaGuard {
    fn init() -> Self {
        let hr = initialize_mta();
        Self {
            should_uninit: hr.is_ok(),
        }
    }
}

impl Drop for ComMtaGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            deinitialize();
        }
    }
}

/// Enumerate available Windows WASAPI render endpoints.
/// Runs on a short-lived MTA thread so callers on STA UI threads (e.g. Tauri)
/// never hit `RPC_E_CHANGED_MODE`.
pub fn list_devices() -> Vec<OutputDevice> {
    thread::spawn(|| {
        let _com = ComMtaGuard::init();
        let mut out = Vec::new();
        let Ok(enumerator) = DeviceEnumerator::new() else {
            return out;
        };
        let default_id = enumerator
            .get_default_device(Direction::Render)
            .ok()
            .and_then(|d| d.get_id().ok());
        if let Ok(col) = enumerator.get_device_collection(Direction::Render) {
            for dev in (&col).into_iter().flatten() {
                let Ok(id) = dev.get_id() else { continue };
                let name = dev.get_friendlyname().unwrap_or_else(|_| id.clone());
                out.push(OutputDevice {
                    id: format!("wasapi/{id}"),
                    description: name,
                    is_default: default_id.as_deref() == Some(id.as_str()),
                });
            }
        }
        out
    })
    .join()
    .unwrap_or_default()
}

fn resolve_device(enumerator: &DeviceEnumerator, id: Option<&str>) -> Result<Device, String> {
    if let Some(want) = id {
        let col = enumerator
            .get_device_collection(Direction::Render)
            .map_err(|e| format!("WASAPI DeviceCollection: {e}"))?;
        for dev in (&col).into_iter().flatten() {
            if dev.get_id().ok().as_deref() == Some(want) {
                return Ok(dev);
            }
        }
        return Err(format!("WASAPI device not found: {want:?}"));
    }
    enumerator
        .get_default_device(Direction::Render)
        .map_err(|e| format!("WASAPI default device: {e}"))
}

pub struct WasapiStream {
    active: ActiveOutput,
    shared: Arc<StreamShared>,
    worker: Option<JoinHandle<()>>,
}

pub fn open_stream(
    id: Option<&str>,
    exclusive: bool,
    spec: PcmSpec,
    mut consumer: Consumer<i32>,
    shared: Arc<StreamShared>,
) -> Result<Box<dyn OutputStream>, String> {
    let want_id = id.map(str::to_string);
    let shared_bg = Arc::clone(&shared);
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<ActiveOutput, String>>(1);

    // Create and own all !Send WASAPI COM interfaces strictly inside the
    // dedicated MTA render thread.
    let worker = thread::Builder::new()
        .name("tacet-wasapi-sink".into())
        .spawn(move || {
            let _com = ComMtaGuard::init();

            let setup = (|| -> Result<_, String> {
                let enumerator = DeviceEnumerator::new()
                    .map_err(|e| format!("WASAPI DeviceEnumerator: {e}"))?;
                let dev = resolve_device(&enumerator, want_id.as_deref())?;
                let dev_id = dev.get_id().unwrap_or_else(|_| "default".into());
                let dev_name = dev.get_friendlyname().unwrap_or_else(|_| dev_id.clone());

                let mut client = dev
                    .get_iaudioclient()
                    .map_err(|e| format!("WASAPI get_iaudioclient: {e}"))?;

                let (wf, chosen_fmt) = if exclusive {
                    let candidates: &[(usize, usize, SampleType, SampleFormat)] =
                        if spec.bits_per_sample <= 16 {
                            &[
                                (16, 16, SampleType::Int, SampleFormat::S16),
                                (24, 32, SampleType::Int, SampleFormat::S24In32Left),
                                (32, 32, SampleType::Int, SampleFormat::S32),
                                (32, 32, SampleType::Float, SampleFormat::F32),
                            ]
                        } else {
                            &[
                                (24, 32, SampleType::Int, SampleFormat::S24In32Left),
                                (32, 32, SampleType::Int, SampleFormat::S32),
                                (32, 32, SampleType::Float, SampleFormat::F32),
                                (16, 16, SampleType::Int, SampleFormat::S16),
                            ]
                        };
                    let mut picked = None;
                    for &(valid_bits, store_bits, st, our_fmt) in candidates {
                        let cand = WaveFormat::new(
                            store_bits,
                            valid_bits,
                            st,
                            spec.sample_rate as usize,
                            spec.channels as usize,
                            None,
                        );
                        if let Ok(accepted_wf) = client.is_supported_exclusive_with_quirks(&cand) {
                            picked = Some((accepted_wf, our_fmt));
                            break;
                        }
                    }
                    picked.ok_or_else(|| {
                        format!(
                            "no supported WASAPI exclusive format on {dev_name:?} for {} Hz / {} ch",
                            spec.sample_rate, spec.channels
                        )
                    })?
                } else {
                    let wf = WaveFormat::new(
                        32,
                        32,
                        SampleType::Float,
                        spec.sample_rate as usize,
                        spec.channels as usize,
                        None,
                    );
                    (wf, SampleFormat::F32)
                };

                let (def_time, min_time) = client
                    .get_device_period()
                    .map_err(|e| format!("WASAPI get_device_period: {e}"))?;

                let mode = if exclusive {
                    let aligned = client
                        .calculate_aligned_period_near(min_time.max(100_000), Some(128), &wf)
                        .unwrap_or(min_time);
                    StreamMode::EventsExclusive {
                        period_hns: aligned,
                    }
                } else {
                    StreamMode::EventsShared {
                        autoconvert: true,
                        buffer_duration_hns: def_time,
                    }
                };

                client
                    .initialize_client(&wf, Direction::Render, mode, None)
                    .map_err(|e| format!("WASAPI initialize_client: {e}"))?;

                let h_event = client
                    .set_get_eventhandle()
                    .map_err(|e| format!("WASAPI set_get_eventhandle: {e}"))?;
                let render = client
                    .get_audiorenderclient()
                    .map_err(|e| format!("WASAPI get_audiorenderclient: {e}"))?;
                let buffer_frames = client
                    .get_bufferframecount()
                    .map_err(|e| format!("WASAPI get_bufferframecount: {e}"))?
                    as usize;

                // Pre-fill initial silence buffer before start_stream() per WASAPI spec.
                let ch = spec.channels.max(1) as usize;
                let frame_bytes = ch * chosen_fmt.bytes_per_sample();
                let init_silence = vec![0u8; buffer_frames * frame_bytes];
                let _ = render.write_to_device(&init_silence, None);

                client
                    .start_stream()
                    .map_err(|e| format!("WASAPI start_stream: {e}"))?;

                let active = ActiveOutput {
                    device_id: format!("wasapi/{dev_id}"),
                    device_name: dev_name,
                    sample_rate: spec.sample_rate,
                    channels: spec.channels,
                    sample_format: chosen_fmt,
                    exclusive,
                    hardware_volume: false,
                };
                Ok((client, h_event, render, buffer_frames, chosen_fmt, active))
            })();

            let (client, h_event, render, buffer_frames, chosen_fmt, active) = match setup {
                Ok(v) => {
                    let _ = init_tx.send(Ok(v.5.clone()));
                    v
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let ch = active.channels.max(1) as usize;
            let frame_bytes = ch * chosen_fmt.bytes_per_sample();
            let mut pcm_buf = vec![0i32; buffer_frames * ch];
            let mut bytes = vec![0u8; buffer_frames * frame_bytes];
            let mut drain_silence_cycles: u8 = 0;

            while !shared_bg.is_stopped() {
                if h_event.wait_for_event(200).is_err() {
                    continue;
                }
                shared_bg.maybe_flush(&mut consumer);

                let padding = if exclusive {
                    0usize
                } else {
                    match client.get_current_padding() {
                        Ok(p) => p as usize,
                        Err(_) => {
                            shared_bg.failed.store(true, Ordering::Release);
                            break;
                        }
                    }
                };
                let avail_out = buffer_frames.saturating_sub(padding);
                if avail_out == 0 {
                    continue;
                }

                let need_bytes = avail_out * frame_bytes;
                if shared_bg.is_paused() {
                    bytes[..need_bytes].fill(0);
                    if render.write_to_device(&bytes[..need_bytes], None).is_err() {
                        shared_bg.failed.store(true, Ordering::Release);
                        break;
                    }
                    continue;
                }

                let avail_in = consumer.slots() / ch;
                if avail_in == 0 {
                    bytes[..need_bytes].fill(0);
                    if render.write_to_device(&bytes[..need_bytes], None).is_err() {
                        shared_bg.failed.store(true, Ordering::Release);
                        break;
                    }
                    if shared_bg.is_draining() {
                        drain_silence_cycles = drain_silence_cycles.saturating_add(1);
                        if drain_silence_cycles >= 2 {
                            shared_bg.finished.store(true, Ordering::Release);
                        }
                    } else {
                        drain_silence_cycles = 0;
                        shared_bg.underruns.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
                drain_silence_cycles = 0;

                let take = avail_in.min(avail_out);
                let out_samples = avail_out * ch;
                pcm_buf[..out_samples].fill(0);
                if let Ok(chunk) = consumer.read_chunk(take * ch) {
                    let (s1, s2) = chunk.as_slices();
                    let n1 = s1.len();
                    pcm_buf[..n1].copy_from_slice(s1);
                    pcm_buf[n1..n1 + s2.len()].copy_from_slice(s2);
                    chunk.commit_all();
                }

                let gain = shared_bg.gain_q16();
                pack_interleaved(
                    &pcm_buf[..out_samples],
                    &mut bytes[..need_bytes],
                    chosen_fmt,
                    gain,
                );
                if render.write_to_device(&bytes[..need_bytes], None).is_err() {
                    shared_bg.failed.store(true, Ordering::Release);
                    break;
                }
                shared_bg
                    .frames_played
                    .fetch_add(take as u64, Ordering::Relaxed);
            }

            let _ = client.stop_stream();
        })
        .map_err(|e| format!("spawn WASAPI thread: {e}"))?;

    let active = match init_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(active)) => active,
        Ok(Err(e)) => {
            let _ = worker.join();
            return Err(e);
        }
        Err(_) => {
            shared.stop.store(true, Ordering::Release);
            let _ = worker.join();
            return Err("WASAPI stream initialization timed out".into());
        }
    };

    Ok(Box::new(WasapiStream {
        active,
        shared,
        worker: Some(worker),
    }))
}

impl OutputStream for WasapiStream {
    fn active_output(&self) -> &ActiveOutput {
        &self.active
    }
}

impl Drop for WasapiStream {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Hardware endpoint volume is not yet bound via raw COM on Windows;
/// `tacet-audio` transparently falls back to integer Q16 software gain.
pub fn open_hardware_volume(_id: Option<&str>) -> Option<Box<dyn HardwareVolume>> {
    None
}
