//! Native Windows WASAPI audio output backend (Exclusive & Shared + hardware volume).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rtrb::Consumer;
use wasapi::{
    DeviceCollection, Direction, SampleType, ShareMode, StreamMode, WaveFormat, get_default_device,
    initialize_mta,
};

use crate::format::{PcmSpec, SampleFormat, pack_interleaved};
use crate::sink::{ActiveOutput, HardwareVolume, OutputDevice, OutputStream, StreamShared};

fn ensure_com() {
    let _ = initialize_mta();
}

/// Enumerate Windows WASAPI render endpoints.
pub fn list_devices() -> Vec<OutputDevice> {
    ensure_com();
    let default_id = get_default_device(&Direction::Render)
        .ok()
        .and_then(|d| d.get_id().ok());
    let Ok(col) = DeviceCollection::new(&Direction::Render) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for dev in col.into_iter().flatten() {
        let Ok(id) = dev.get_id() else { continue };
        let desc = dev.get_friendlyname().unwrap_or_else(|_| id.clone());
        let is_default = default_id.as_deref() == Some(id.as_str());
        out.push(OutputDevice {
            id: format!("wasapi/{id}"),
            description: desc,
            is_default,
        });
    }
    out
}

fn resolve_wasapi_device(id: Option<&str>) -> Result<(wasapi::Device, String, String), String> {
    ensure_com();
    let dev = match id {
        Some(want) => {
            let col = DeviceCollection::new(&Direction::Render)
                .map_err(|e| format!("WASAPI enumerate: {e}"))?;
            col.into_iter()
                .flatten()
                .find(|d| d.get_id().ok().as_deref() == Some(want))
                .ok_or_else(|| format!("WASAPI device {want:?} not found"))?
        }
        None => get_default_device(&Direction::Render)
            .map_err(|e| format!("WASAPI default device: {e}"))?,
    };
    let dev_id = dev.get_id().unwrap_or_else(|_| "default".into());
    let name = dev.get_friendlyname().unwrap_or_else(|_| dev_id.clone());
    Ok((dev, dev_id, name))
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
    let (dev, dev_id, dev_name) = resolve_wasapi_device(id)?;
    let mut client = dev
        .get_iaudioclient()
        .map_err(|e| format!("WASAPI get_iaudioclient: {e}"))?;

    let share_mode = if exclusive {
        ShareMode::Exclusive
    } else {
        ShareMode::Shared
    };

    // Pick format: in exclusive mode try highest integer match (S32 / S24 / S16 / F32);
    // in shared mode use F32 with auto-convert.
    let (wave_fmt, sample_format) = if exclusive {
        let candidates = [
            (32usize, 32usize, SampleType::Int, SampleFormat::S32),
            (24usize, 32usize, SampleType::Int, SampleFormat::S24In32Left),
            (24usize, 24usize, SampleType::Int, SampleFormat::S24Packed),
            (16usize, 16usize, SampleType::Int, SampleFormat::S16),
            (32usize, 32usize, SampleType::Float, SampleFormat::F32),
        ];
        let mut chosen = None;
        for (bits, store, st, sf) in candidates {
            let wf = WaveFormat::new(
                store,
                bits,
                &st,
                spec.sample_rate as usize,
                spec.channels as usize,
                None,
            );
            if client.is_supported_exclusive_with_quirks(&wf).is_ok() {
                chosen = Some((wf, sf));
                break;
            }
        }
        chosen.ok_or_else(|| {
            format!(
                "WASAPI exclusive format not supported on {dev_name:?} ({} Hz {} ch)",
                spec.sample_rate, spec.channels
            )
        })?
    } else {
        let wf = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            spec.sample_rate as usize,
            spec.channels as usize,
            None,
        );
        (wf, SampleFormat::F32)
    };

    let (def_time, min_time) = client
        .get_device_period()
        .map_err(|e| format!("WASAPI get_device_period: {e}"))?;
    let period = if exclusive { min_time } else { def_time };
    let mode = StreamMode::EventsShared {
        autoconvert: !exclusive,
        buffer_duration_hns: period,
    };
    let mode = if exclusive {
        StreamMode::EventsExclusive {
            buffer_duration_hns: period,
        }
    } else {
        mode
    };

    client
        .initialize_client(&wave_fmt, &Direction::Render, &mode)
        .map_err(|e| format!("WASAPI initialize_client ({share_mode:?}): {e}"))?;

    let h_event = client
        .set_get_eventhandle()
        .map_err(|e| format!("WASAPI event handle: {e}"))?;
    let render = client
        .get_audiorenderclient()
        .map_err(|e| format!("WASAPI render client: {e}"))?;
    let buf_frames = client
        .get_buffer_size()
        .map_err(|e| format!("WASAPI buffer size: {e}"))? as usize;

    client
        .start_stream()
        .map_err(|e| format!("WASAPI start_stream: {e}"))?;

    let active = ActiveOutput {
        device_id: format!("wasapi/{dev_id}"),
        device_name: dev_name,
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        sample_format,
        exclusive,
        hardware_volume: open_hardware_volume(Some(&dev_id)).is_some(),
    };

    let shared_bg = Arc::clone(&shared);
    let worker = thread::Builder::new()
        .name("tacet-wasapi-sink".into())
        .spawn(move || {
            ensure_com();
            let channels = spec.channels.max(1) as usize;
            let bps = sample_format.bytes_per_sample();
            let frame_bytes = bps * channels;
            let mut tmp = vec![0i32; buf_frames * channels];
            let mut bytes = vec![0u8; buf_frames * frame_bytes];

            while !shared_bg.is_stopped() {
                if h_event.wait_for_event(50).is_err() {
                    continue;
                }
                let avail_out = if exclusive {
                    buf_frames
                } else {
                    let pad = client.get_current_padding().unwrap_or(0) as usize;
                    buf_frames.saturating_sub(pad)
                };
                if avail_out == 0 {
                    continue;
                }

                shared_bg.maybe_flush(&mut consumer);
                if shared_bg.is_paused() {
                    bytes[..avail_out * frame_bytes].fill(0);
                    let _ =
                        render.write_to_device(avail_out, &bytes[..avail_out * frame_bytes], None);
                    continue;
                }

                let avail_in = consumer.slots() / channels;
                if avail_in == 0 {
                    bytes[..avail_out * frame_bytes].fill(0);
                    let _ =
                        render.write_to_device(avail_out, &bytes[..avail_out * frame_bytes], None);
                    if shared_bg.is_draining() {
                        shared_bg.finished.store(true, Ordering::Release);
                        break;
                    }
                    shared_bg.underruns.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                let take_frames = avail_in.min(avail_out);
                let take_samples = take_frames * channels;
                if let Ok(chunk) = consumer.read_chunk(take_samples) {
                    let (s1, s2) = chunk.as_slices();
                    tmp[..s1.len()].copy_from_slice(s1);
                    tmp[s1.len()..s1.len() + s2.len()].copy_from_slice(s2);
                    chunk.commit_all();
                }

                let gain = shared_bg.gain_q16();
                let n = pack_interleaved(&tmp[..take_samples], &mut bytes, sample_format, gain);
                if take_frames < avail_out {
                    bytes[n..avail_out * frame_bytes].fill(0);
                }
                if render
                    .write_to_device(avail_out, &bytes[..avail_out * frame_bytes], None)
                    .is_err()
                {
                    break;
                }
                shared_bg
                    .frames_played
                    .fetch_add(take_frames as u64, Ordering::Relaxed);
            }

            let _ = client.stop_stream();
        })
        .map_err(|e| format!("spawn WASAPI thread: {e}"))?;

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

pub struct WasapiVolume {
    id: Option<String>,
}

pub fn open_hardware_volume(id: Option<&str>) -> Option<WasapiVolume> {
    let v = WasapiVolume {
        id: id.map(str::to_string),
    };
    if v.get_volume().is_ok() {
        Some(v)
    } else {
        None
    }
}

impl HardwareVolume for WasapiVolume {
    fn get_volume(&self) -> Result<u8, String> {
        let (dev, _, _) = resolve_wasapi_device(self.id.as_deref())?;
        let ep = dev
            .get_iaudioendpointvolume()
            .map_err(|e| format!("WASAPI endpoint volume: {e}"))?;
        let scalar = ep
            .get_master_volume_level_scalar()
            .map_err(|e| format!("WASAPI get volume: {e}"))?;
        Ok((scalar.clamp(0.0, 1.0) * 100.0).round() as u8)
    }

    fn set_volume(&self, volume: u8) -> Result<(), String> {
        let (dev, _, _) = resolve_wasapi_device(self.id.as_deref())?;
        let ep = dev
            .get_iaudioendpointvolume()
            .map_err(|e| format!("WASAPI endpoint volume: {e}"))?;
        ep.set_master_volume_level_scalar((volume.min(100) as f32) / 100.0)
            .map_err(|e| format!("WASAPI set volume: {e}"))
    }

    fn get_mute(&self) -> Result<bool, String> {
        let (dev, _, _) = resolve_wasapi_device(self.id.as_deref())?;
        let ep = dev
            .get_iaudioendpointvolume()
            .map_err(|e| format!("WASAPI endpoint volume: {e}"))?;
        ep.get_mute().map_err(|e| format!("WASAPI get mute: {e}"))
    }

    fn set_mute(&self, muted: bool) -> Result<(), String> {
        let (dev, _, _) = resolve_wasapi_device(self.id.as_deref())?;
        let ep = dev
            .get_iaudioendpointvolume()
            .map_err(|e| format!("WASAPI endpoint volume: {e}"))?;
        ep.set_mute(muted)
            .map_err(|e| format!("WASAPI set mute: {e}"))
    }
}
