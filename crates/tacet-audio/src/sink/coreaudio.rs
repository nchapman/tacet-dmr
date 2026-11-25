//! Native macOS CoreAudio HAL & AUHAL output backend.
//!
//! - **Shared mode (`exclusive = false`)**: Uses `AUHAL` (`kAudioUnitSubType_HALOutput`)
//!   with interleaved `f32` at the track's sample rate so macOS handles system
//!   mixing and resampling seamlessly.
//! - **Exclusive mode (`exclusive = true`)**: Uses direct CoreAudio HAL
//!   (`AudioDeviceCreateIOProcID`) with `kAudioDevicePropertyHogMode` and
//!   `kAudioStreamPropertyPhysicalFormat` switching. Natively packs `S16`,
//!   **`S24Packed` (3-byte LE — fixing 24-bit-only USB DACs like the AudioQuest
//!   DragonFly Black)**, `S24In32Left`, `S24In32Right`, `S32`, and `F32`
//!   directly into the hardware DMA buffer with zero allocations in the
//!   real-time callback.
//! - **Hardware volume & mute**: Controlled directly via CoreAudio HAL with
//!   cached property selectors per UID-verified `AudioDeviceID`.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::slice;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use coreaudio_sys::{
    AURenderCallbackStruct, AudioBufferList, AudioComponentDescription, AudioComponentFindNext,
    AudioComponentInstance, AudioComponentInstanceDispose, AudioComponentInstanceNew,
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectHasProperty, AudioObjectIsPropertySettable, AudioObjectPropertyAddress,
    AudioObjectSetPropertyData, AudioOutputUnitStart, AudioOutputUnitStop,
    AudioStreamBasicDescription, AudioStreamID, AudioStreamRangedDescription, AudioTimeStamp,
    AudioUnitInitialize, AudioUnitRenderActionFlags, AudioUnitSetProperty, AudioUnitUninitialize,
    OSStatus, kAudioDevicePropertyDeviceUID, kAudioDevicePropertyHogMode, kAudioDevicePropertyMute,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyStreams,
    kAudioDevicePropertyVolumeScalar, kAudioFormatFlagIsAlignedHigh, kAudioFormatFlagIsBigEndian,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatFlagIsSignedInteger, kAudioFormatLinearPCM,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    kAudioOutputUnitProperty_CurrentDevice, kAudioStreamPropertyAvailablePhysicalFormats,
    kAudioStreamPropertyPhysicalFormat, kAudioStreamPropertyVirtualFormat,
    kAudioUnitManufacturer_Apple, kAudioUnitProperty_SetRenderCallback,
    kAudioUnitProperty_StreamFormat, kAudioUnitScope_Global, kAudioUnitScope_Input,
    kAudioUnitSubType_HALOutput, kAudioUnitType_Output,
};
use rtrb::Consumer;

use crate::format::{PcmSpec, SampleFormat, pack_interleaved, pack_planar_channel};
use crate::sink::{ActiveOutput, HardwareVolume, OutputDevice, OutputStream, StreamShared};

const K_AUDIO_OBJECT_UNKNOWN: AudioDeviceID = 0;

fn prop_addr(selector: u32, scope: u32, element: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: element,
    }
}

fn cfstring_property(obj: AudioDeviceID, selector: u32, scope: u32) -> Option<String> {
    let addr = prop_addr(selector, scope, kAudioObjectPropertyElementMain);
    let mut cf: CFStringRef = null();
    let mut size = size_of::<CFStringRef>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            obj,
            &addr,
            0,
            null(),
            &mut size,
            (&mut cf as *mut CFStringRef).cast::<c_void>(),
        )
    };
    if status != 0 || cf.is_null() {
        return None;
    }
    let s = unsafe { CFString::wrap_under_create_rule(cf) }.to_string();
    Some(s)
}

fn default_output_device_id() -> Option<AudioDeviceID> {
    let addr = prop_addr(
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    let mut id: AudioDeviceID = K_AUDIO_OBJECT_UNKNOWN;
    let mut size = size_of::<AudioDeviceID>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &addr,
            0,
            null(),
            &mut size,
            (&mut id as *mut AudioDeviceID).cast::<c_void>(),
        )
    };
    if status == 0 && id != K_AUDIO_OBJECT_UNKNOWN {
        Some(id)
    } else {
        None
    }
}

fn all_device_ids() -> Vec<AudioDeviceID> {
    let addr = prop_addr(
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    );
    let mut size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &addr, 0, null(), &mut size)
    };
    if status != 0 || size == 0 {
        return Vec::new();
    }
    let count = (size as usize) / size_of::<AudioDeviceID>();
    let mut ids = vec![0 as AudioDeviceID; count];
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &addr,
            0,
            null(),
            &mut size,
            ids.as_mut_ptr().cast::<c_void>(),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    let actual = ((size as usize) / size_of::<AudioDeviceID>()).min(ids.len());
    ids.truncate(actual);
    ids
}

fn output_streams(dev: AudioDeviceID) -> Vec<AudioStreamID> {
    let addr = prop_addr(
        kAudioDevicePropertyStreams,
        kAudioObjectPropertyScopeOutput,
        kAudioObjectPropertyElementMain,
    );
    let mut size: u32 = 0;
    let status = unsafe { AudioObjectGetPropertyDataSize(dev, &addr, 0, null(), &mut size) };
    if status != 0 || size == 0 {
        return Vec::new();
    }
    let count = (size as usize) / size_of::<AudioStreamID>();
    let mut streams = vec![0 as AudioStreamID; count];
    let status = unsafe {
        AudioObjectGetPropertyData(
            dev,
            &addr,
            0,
            null(),
            &mut size,
            streams.as_mut_ptr().cast::<c_void>(),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    let actual = ((size as usize) / size_of::<AudioStreamID>()).min(streams.len());
    streams.truncate(actual);
    streams
}

fn resolve_device(uid: Option<&str>) -> Result<(AudioDeviceID, String, String), String> {
    if let Some(want_uid) = uid {
        for dev in all_device_ids() {
            if output_streams(dev).is_empty() {
                continue;
            }
            if let Some(got) = cfstring_property(
                dev,
                kAudioDevicePropertyDeviceUID,
                kAudioObjectPropertyScopeGlobal,
            ) && got == want_uid
            {
                let name = cfstring_property(
                    dev,
                    kAudioObjectPropertyName,
                    kAudioObjectPropertyScopeGlobal,
                )
                .unwrap_or_else(|| got.clone());
                return Ok((dev, got, name));
            }
        }
        return Err(format!("CoreAudio output device {want_uid:?} not found"));
    }

    let dev = default_output_device_id()
        .ok_or_else(|| "no default CoreAudio output device available".to_string())?;
    let got_uid = cfstring_property(
        dev,
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
    )
    .unwrap_or_else(|| format!("device-{dev}"));
    let name = cfstring_property(
        dev,
        kAudioObjectPropertyName,
        kAudioObjectPropertyScopeGlobal,
    )
    .unwrap_or_else(|| got_uid.clone());
    Ok((dev, got_uid, name))
}

/// Enumerate all macOS CoreAudio output devices.
pub fn list_devices() -> Vec<OutputDevice> {
    let default_id = default_output_device_id();
    let mut out = Vec::new();
    for dev in all_device_ids() {
        if output_streams(dev).is_empty() {
            continue;
        }
        let Some(uid) = cfstring_property(
            dev,
            kAudioDevicePropertyDeviceUID,
            kAudioObjectPropertyScopeGlobal,
        ) else {
            continue;
        };
        let name = cfstring_property(
            dev,
            kAudioObjectPropertyName,
            kAudioObjectPropertyScopeGlobal,
        )
        .unwrap_or_else(|| uid.clone());
        out.push(OutputDevice {
            id: format!("coreaudio/{uid}"),
            description: name,
            is_default: default_id == Some(dev),
        });
    }
    out
}

/// Inspect an `AudioStreamBasicDescription` and map it to our `SampleFormat`
/// + interleaved flag. Returns `None` for non-PCM or big-endian formats.
pub(crate) fn asbd_to_sample_format(
    asbd: &AudioStreamBasicDescription,
) -> Option<(SampleFormat, bool)> {
    if asbd.mFormatID != kAudioFormatLinearPCM {
        return None;
    }
    if (asbd.mFormatFlags & kAudioFormatFlagIsBigEndian) != 0 {
        return None;
    }
    let non_interleaved = (asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0;
    let is_float = (asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0;
    let is_signed = (asbd.mFormatFlags & kAudioFormatFlagIsSignedInteger) != 0;
    let is_high = (asbd.mFormatFlags & kAudioFormatFlagIsAlignedHigh) != 0;

    let ch = asbd.mChannelsPerFrame.max(1);
    let bytes_per_sample = if non_interleaved {
        asbd.mBytesPerFrame
    } else {
        asbd.mBytesPerFrame / ch
    };

    let fmt = if is_float && asbd.mBitsPerChannel == 32 && bytes_per_sample == 4 {
        SampleFormat::F32
    } else if is_signed && asbd.mBitsPerChannel == 16 && bytes_per_sample == 2 {
        SampleFormat::S16
    } else if is_signed && asbd.mBitsPerChannel == 24 && bytes_per_sample == 3 {
        // 3-byte tightly packed 24-bit integer (e.g. AudioQuest DragonFly Black!).
        SampleFormat::S24Packed
    } else if is_signed && asbd.mBitsPerChannel == 24 && bytes_per_sample == 4 {
        if is_high {
            SampleFormat::S24In32Left
        } else {
            SampleFormat::S24In32Right
        }
    } else if is_signed && asbd.mBitsPerChannel == 32 && bytes_per_sample == 4 {
        SampleFormat::S32
    } else {
        return None;
    };

    Some((fmt, non_interleaved))
}

/// Select the best available physical format from `available` for `spec`.
///
/// Prefers:
/// 1. Exact sample rate match (falls back to closest supported rate).
/// 2. Exact channel count match (falls back to >= requested channels).
/// 3. Integer format with bit depth >= `spec.bits_per_sample` (preferring the
///    smallest integer depth >= requested, e.g. S24Packed for 24-bit DACs),
///    then higher integer depths, then F32, then lower depths.
pub(crate) fn select_best_physical_format(
    available: &[AudioStreamRangedDescription],
    spec: PcmSpec,
) -> Option<(AudioStreamBasicDescription, SampleFormat, bool)> {
    let target_rate = spec.sample_rate as f64;
    let want_bits = spec.bits_per_sample.max(16);

    let mut best: Option<(i64, AudioStreamBasicDescription, SampleFormat, bool)> = None;

    for ranged in available {
        let mut asbd = ranged.mFormat;
        let Some((fmt, non_interleaved)) = asbd_to_sample_format(&asbd) else {
            continue;
        };
        if asbd.mChannelsPerFrame < spec.channels as u32 {
            continue;
        }

        // Choose concrete sample rate within ranged description.
        // Require an exact sample-rate match in exclusive mode: tacet-audio
        // does not software-resample, so silently clocking PCM at the wrong
        // rate would play at the wrong speed/pitch.
        let min_r = ranged.mSampleRateRange.mMinimum;
        let max_r = ranged.mSampleRateRange.mMaximum;
        if target_rate < min_r - 1.0 || target_rate > max_r + 1.0 || target_rate <= 0.0 {
            continue;
        }
        let chosen_rate = target_rate.clamp(min_r, max_r);
        if (chosen_rate - target_rate).abs() >= 1.0 {
            continue;
        }
        asbd.mSampleRate = chosen_rate;

        let ch_exact = asbd.mChannelsPerFrame == spec.channels as u32;

        // Higher score wins.
        let mut score: i64 = 10_000_000;
        if ch_exact {
            score += 1_000_000;
        } else {
            score -= (asbd.mChannelsPerFrame as i64) * 1000;
        }

        // Format preference: integer >= want_bits (prefer exact bit depth), then F32, then lower bits.
        let is_int = fmt != SampleFormat::F32;
        let bits = fmt.bit_depth();
        if is_int && bits >= want_bits {
            score += 500_000 - ((bits - want_bits) as i64) * 100;
        } else if fmt == SampleFormat::F32 {
            score += 400_000;
        } else {
            score += 100_000 + (bits as i64) * 100;
        }
        // Slight preference for interleaved when equal.
        if !non_interleaved {
            score += 10;
        }

        if best.as_ref().is_none_or(|b| score > b.0) {
            best = Some((score, asbd, fmt, non_interleaved));
        }
    }

    best.map(|(_, asbd, fmt, ni)| (asbd, fmt, ni))
}

/// Open a macOS CoreAudio stream (exclusive HAL hog mode or shared AUHAL).
pub fn open_stream(
    uid: Option<&str>,
    exclusive: bool,
    spec: PcmSpec,
    consumer: Consumer<i32>,
    shared: Arc<StreamShared>,
) -> Result<Box<dyn OutputStream>, String> {
    let (dev_id, dev_uid, dev_name) = resolve_device(uid)?;
    if exclusive {
        CoreAudioHalStream::open(dev_id, dev_uid, dev_name, spec, consumer, shared)
            .map(|s| Box::new(s) as Box<dyn OutputStream>)
    } else {
        CoreAudioAuhalStream::open(dev_id, dev_uid, dev_name, spec, consumer, shared)
            .map(|s| Box::new(s) as Box<dyn OutputStream>)
    }
}

// ---------------------------------------------------------------------------
// Exclusive Mode: Direct CoreAudio HAL (Hog Mode + Physical Format Switching)
// ---------------------------------------------------------------------------

struct HalIoContext {
    consumer: Consumer<i32>,
    shared: Arc<StreamShared>,
    format: SampleFormat,
    src_channels: usize,
    dst_channels: usize,
    non_interleaved: bool,
    drain_silence_callbacks: u8,
    scratch: Vec<i32>,
}

unsafe extern "C" fn hal_io_proc(
    _in_device: AudioDeviceID,
    _in_now: *const AudioTimeStamp,
    _in_input_data: *const AudioBufferList,
    _in_input_time: *const AudioTimeStamp,
    out_output_data: *mut AudioBufferList,
    _in_output_time: *const AudioTimeStamp,
    in_client_data: *mut c_void,
) -> OSStatus {
    if out_output_data.is_null() || in_client_data.is_null() {
        return 0;
    }
    let ctx = unsafe { &mut *(in_client_data as *mut HalIoContext) };
    let abl = unsafe { &mut *out_output_data };
    let num_buffers = abl.mNumberBuffers as usize;
    if num_buffers == 0 {
        return 0;
    }

    let buffers = unsafe { slice::from_raw_parts_mut(abl.mBuffers.as_mut_ptr(), num_buffers) };
    let bps = ctx.format.bytes_per_sample();

    let frames = if ctx.non_interleaved {
        (buffers[0].mDataByteSize as usize) / bps
    } else {
        let ch = buffers[0].mNumberChannels.max(1) as usize;
        (buffers[0].mDataByteSize as usize) / (bps * ch)
    };
    if frames == 0 {
        return 0;
    }

    let zero_all = |bufs: &mut [coreaudio_sys::AudioBuffer]| {
        for b in bufs.iter_mut() {
            if !b.mData.is_null() && b.mDataByteSize > 0 {
                let dst = unsafe {
                    slice::from_raw_parts_mut(b.mData as *mut u8, b.mDataByteSize as usize)
                };
                dst.fill(0);
            }
        }
    };

    ctx.shared.maybe_flush(&mut ctx.consumer);
    if ctx.shared.is_paused() || ctx.shared.is_stopped() {
        zero_all(buffers);
        return 0;
    }

    let src_ch = ctx.src_channels;
    let dst_ch = ctx.dst_channels;
    let avail_frames = ctx.consumer.slots() / src_ch;

    if avail_frames == 0 {
        zero_all(buffers);
        if ctx.shared.is_draining() {
            // Wait one full silence callback after the final audio buffer so
            // hardware finishes clocking out the previous DMA buffer before stop.
            ctx.drain_silence_callbacks = ctx.drain_silence_callbacks.saturating_add(1);
            if ctx.drain_silence_callbacks >= 2 {
                ctx.shared.finished.store(true, Ordering::Release);
            }
        } else {
            ctx.drain_silence_callbacks = 0;
            ctx.shared.underruns.fetch_add(1, Ordering::Relaxed);
        }
        return 0;
    }
    ctx.drain_silence_callbacks = 0;

    let take_frames = avail_frames.min(frames);
    let need_scratch = frames * dst_ch;
    if ctx.scratch.len() < need_scratch {
        // Scratch is pre-sized generously on open; if CoreAudio ever asks for a
        // larger burst, output silence rather than allocating in RT.
        zero_all(buffers);
        return 0;
    }

    // Read interleaved source frames into scratch (upmixing mono->stereo or padding to dst_ch).
    let dst_slice = &mut ctx.scratch[..frames * dst_ch];
    dst_slice.fill(0);
    if let Ok(chunk) = ctx.consumer.read_chunk(take_frames * src_ch) {
        let (s1, s2) = chunk.as_slices();
        if src_ch == dst_ch {
            let n1 = s1.len();
            dst_slice[..n1].copy_from_slice(s1);
            dst_slice[n1..n1 + s2.len()].copy_from_slice(s2);
        } else if src_ch == 1 && dst_ch >= 2 {
            let mut src_iter = s1.iter().chain(s2.iter());
            for f in 0..take_frames {
                let v = *src_iter.next().unwrap_or(&0);
                dst_slice[f * dst_ch] = v;
                dst_slice[f * dst_ch + 1] = v;
            }
        } else {
            let mut src_iter = s1.iter().chain(s2.iter());
            for f in 0..take_frames {
                for c in 0..src_ch {
                    let v = *src_iter.next().unwrap_or(&0);
                    if c < dst_ch {
                        dst_slice[f * dst_ch + c] = v;
                    }
                }
            }
        }
        chunk.commit_all();
    }

    let gain = ctx.shared.gain_q16();
    if ctx.non_interleaved {
        let need_bytes = frames * bps;
        for (ch_idx, b) in buffers.iter_mut().enumerate() {
            if b.mData.is_null() || b.mDataByteSize == 0 {
                continue;
            }
            let out =
                unsafe { slice::from_raw_parts_mut(b.mData as *mut u8, b.mDataByteSize as usize) };
            if ch_idx < dst_ch && out.len() >= need_bytes {
                pack_planar_channel(dst_slice, dst_ch, ch_idx, out, ctx.format, gain);
            } else {
                out.fill(0);
            }
        }
    } else {
        // Zero any secondary stream buffers first.
        for b in buffers.iter_mut().skip(1) {
            if !b.mData.is_null() && b.mDataByteSize > 0 {
                let extra = unsafe {
                    slice::from_raw_parts_mut(b.mData as *mut u8, b.mDataByteSize as usize)
                };
                extra.fill(0);
            }
        }
        let need_bytes = dst_slice.len() * bps;
        if !buffers[0].mData.is_null() {
            let out = unsafe {
                slice::from_raw_parts_mut(
                    buffers[0].mData as *mut u8,
                    buffers[0].mDataByteSize as usize,
                )
            };
            if buffers[0].mNumberChannels as usize == dst_ch && out.len() >= need_bytes {
                pack_interleaved(dst_slice, out, ctx.format, gain);
            } else {
                out.fill(0);
            }
        }
    }

    ctx.shared
        .frames_played
        .fetch_add(take_frames as u64, Ordering::Relaxed);

    0
}

struct CoreAudioHalStream {
    dev_id: AudioDeviceID,
    stream_id: Option<AudioStreamID>,
    original_phys: Option<AudioStreamBasicDescription>,
    hog_acquired: bool,
    io_proc_id: AudioDeviceIOProcID,
    ctx: *mut HalIoContext,
    shared: Arc<StreamShared>,
    active: ActiveOutput,
}

unsafe impl Send for CoreAudioHalStream {}

impl CoreAudioHalStream {
    fn open(
        dev_id: AudioDeviceID,
        dev_uid: String,
        dev_name: String,
        spec: PcmSpec,
        consumer: Consumer<i32>,
        shared: Arc<StreamShared>,
    ) -> Result<Self, String> {
        // 1. Acquire hog mode for our PID so no other process can touch the DAC.
        let my_pid: i32 = std::process::id() as i32;
        let mut hog_pid: i32 = my_pid;
        let hog_addr = prop_addr(
            kAudioDevicePropertyHogMode,
            kAudioObjectPropertyScopeOutput,
            kAudioObjectPropertyElementMain,
        );
        let hog_status = unsafe {
            AudioObjectSetPropertyData(
                dev_id,
                &hog_addr,
                0,
                null(),
                size_of::<i32>() as u32,
                (&mut hog_pid as *mut i32).cast::<c_void>(),
            )
        };
        // Verify ownership by reading back the hog PID.
        let mut verify_pid: i32 = -1;
        let mut verify_sz = size_of::<i32>() as u32;
        let verify_status = unsafe {
            AudioObjectGetPropertyData(
                dev_id,
                &hog_addr,
                0,
                null(),
                &mut verify_sz,
                (&mut verify_pid as *mut i32).cast::<c_void>(),
            )
        };
        let hog_acquired = hog_status == 0 && (verify_status != 0 || verify_pid == my_pid);
        if !hog_acquired {
            return Err(format!(
                "CoreAudio hog mode failed on {dev_name:?} (OSStatus {hog_status}, owner pid {verify_pid})"
            ));
        }

        // Helper closure to restore physical format + release hog mode on any error.
        let cleanup_hal = |sid: Option<AudioStreamID>,
                           orig: Option<AudioStreamBasicDescription>| {
            if let (Some(sid), Some(orig)) = (sid, orig) {
                let phys_addr = prop_addr(
                    kAudioStreamPropertyPhysicalFormat,
                    kAudioObjectPropertyScopeGlobal,
                    kAudioObjectPropertyElementMain,
                );
                let _ = unsafe {
                    AudioObjectSetPropertyData(
                        sid,
                        &phys_addr,
                        0,
                        null(),
                        size_of::<AudioStreamBasicDescription>() as u32,
                        (&orig as *const AudioStreamBasicDescription).cast::<c_void>(),
                    )
                };
            }
            let mut free_pid: i32 = -1;
            let _ = unsafe {
                AudioObjectSetPropertyData(
                    dev_id,
                    &hog_addr,
                    0,
                    null(),
                    size_of::<i32>() as u32,
                    (&mut free_pid as *mut i32).cast::<c_void>(),
                )
            };
        };

        // 2. Inspect output stream physical formats and switch to best match.
        let streams = output_streams(dev_id);
        let stream_id = streams.first().copied();
        let mut original_phys: Option<AudioStreamBasicDescription> = None;

        let (chosen_asbd, chosen_fmt, non_interleaved) = if let Some(sid) = stream_id {
            // Save current physical format so Drop restores it cleanly.
            let phys_addr = prop_addr(
                kAudioStreamPropertyPhysicalFormat,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            );
            let mut cur_phys: AudioStreamBasicDescription = unsafe { zeroed() };
            let mut sz = size_of::<AudioStreamBasicDescription>() as u32;
            if unsafe {
                AudioObjectGetPropertyData(
                    sid,
                    &phys_addr,
                    0,
                    null(),
                    &mut sz,
                    (&mut cur_phys as *mut AudioStreamBasicDescription).cast::<c_void>(),
                )
            } == 0
            {
                original_phys = Some(cur_phys);
            }

            // Query available physical formats.
            let avail_addr = prop_addr(
                kAudioStreamPropertyAvailablePhysicalFormats,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            );
            let mut avail_size: u32 = 0;
            let mut available: Vec<AudioStreamRangedDescription> = Vec::new();
            if unsafe {
                AudioObjectGetPropertyDataSize(sid, &avail_addr, 0, null(), &mut avail_size)
            } == 0
                && avail_size > 0
            {
                let n = (avail_size as usize) / size_of::<AudioStreamRangedDescription>();
                available.resize(n, unsafe { zeroed() });
                if unsafe {
                    AudioObjectGetPropertyData(
                        sid,
                        &avail_addr,
                        0,
                        null(),
                        &mut avail_size,
                        available.as_mut_ptr().cast::<c_void>(),
                    )
                } == 0
                {
                    let actual =
                        ((avail_size as usize) / size_of::<AudioStreamRangedDescription>()).min(n);
                    available.truncate(actual);
                } else {
                    available.clear();
                }
            }

            let Some((target_asbd, _fmt, _ni)) = select_best_physical_format(&available, spec)
            else {
                cleanup_hal(None, None);
                return Err(format!(
                    "no compatible CoreAudio physical PCM format on {dev_name:?} for {} Hz / {} ch",
                    spec.sample_rate, spec.channels
                ));
            };

            // Apply physical format.
            let set_status = unsafe {
                AudioObjectSetPropertyData(
                    sid,
                    &phys_addr,
                    0,
                    null(),
                    size_of::<AudioStreamBasicDescription>() as u32,
                    (&target_asbd as *const AudioStreamBasicDescription).cast::<c_void>(),
                )
            };
            if set_status != 0 {
                // Fallback: try setting nominal sample rate on the device itself.
                let rate_addr = prop_addr(
                    kAudioDevicePropertyNominalSampleRate,
                    kAudioObjectPropertyScopeGlobal,
                    kAudioObjectPropertyElementMain,
                );
                let rate = target_asbd.mSampleRate;
                let _ = unsafe {
                    AudioObjectSetPropertyData(
                        dev_id,
                        &rate_addr,
                        0,
                        null(),
                        size_of::<f64>() as u32,
                        (&rate as *const f64).cast::<c_void>(),
                    )
                };
            }

            // Wait briefly for the virtual/physical stream format to settle, then
            // read the actual active stream format that IOProc will receive.
            let virt_addr = prop_addr(
                kAudioStreamPropertyVirtualFormat,
                kAudioObjectPropertyScopeGlobal,
                kAudioObjectPropertyElementMain,
            );
            let deadline = Instant::now() + Duration::from_millis(500);
            let mut active_asbd = target_asbd;
            let mut settled = false;
            loop {
                let mut cur: AudioStreamBasicDescription = unsafe { zeroed() };
                let mut cur_sz = size_of::<AudioStreamBasicDescription>() as u32;
                let ok = unsafe {
                    AudioObjectGetPropertyData(
                        sid,
                        &virt_addr,
                        0,
                        null(),
                        &mut cur_sz,
                        (&mut cur as *mut AudioStreamBasicDescription).cast::<c_void>(),
                    )
                } == 0;
                if ok {
                    active_asbd = cur;
                    if (cur.mSampleRate - target_asbd.mSampleRate).abs() < 1.0 {
                        settled = true;
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            if !settled {
                cleanup_hal(Some(sid), original_phys);
                return Err(format!(
                    "CoreAudio device {dev_name:?} did not switch to {} Hz (active: {} Hz)",
                    spec.sample_rate, active_asbd.mSampleRate
                ));
            }

            // In direct HAL IOProc, CoreAudio delivers buffers in the stream's
            // *virtual* format. Fail cleanly if unrecognized rather than risking
            // a byte-size mismatch inside the real-time IOProc.
            let Some((io_fmt, io_ni)) = asbd_to_sample_format(&active_asbd) else {
                cleanup_hal(Some(sid), original_phys);
                return Err(format!(
                    "unsupported CoreAudio active virtual PCM format on {dev_name:?}"
                ));
            };
            (active_asbd, io_fmt, io_ni)
        } else {
            cleanup_hal(None, None);
            return Err(format!(
                "CoreAudio device {dev_name:?} has no output streams"
            ));
        };

        let dst_channels = chosen_asbd.mChannelsPerFrame.max(1) as usize;
        // Pre-allocate scratch for up to 16,384 frames so IOProc never allocates.
        let ctx = Box::into_raw(Box::new(HalIoContext {
            consumer,
            shared: Arc::clone(&shared),
            format: chosen_fmt,
            src_channels: spec.channels.max(1) as usize,
            dst_channels,
            non_interleaved,
            drain_silence_callbacks: 0,
            scratch: vec![0i32; 16_384 * dst_channels],
        }));

        let mut io_proc_id: AudioDeviceIOProcID = None;
        let create_status = unsafe {
            AudioDeviceCreateIOProcID(
                dev_id,
                Some(hal_io_proc),
                ctx.cast::<c_void>(),
                &mut io_proc_id,
            )
        };
        if create_status != 0 {
            unsafe { drop(Box::from_raw(ctx)) };
            cleanup_hal(stream_id, original_phys);
            return Err(format!(
                "AudioDeviceCreateIOProcID failed on {dev_name:?} (OSStatus {create_status})"
            ));
        }

        let start_status = unsafe { AudioDeviceStart(dev_id, io_proc_id) };
        if start_status != 0 {
            unsafe {
                let _ = AudioDeviceDestroyIOProcID(dev_id, io_proc_id);
                drop(Box::from_raw(ctx));
            }
            cleanup_hal(stream_id, original_phys);
            return Err(format!(
                "AudioDeviceStart failed on {dev_name:?} (OSStatus {start_status})"
            ));
        }

        let active = ActiveOutput {
            device_id: format!("coreaudio/{dev_uid}"),
            device_name: dev_name,
            sample_rate: chosen_asbd.mSampleRate.round() as u32,
            channels: dst_channels as u16,
            sample_format: chosen_fmt,
            exclusive: true,
            hardware_volume: open_hardware_volume(Some(&dev_uid)).is_some(),
        };

        Ok(Self {
            dev_id,
            stream_id,
            original_phys,
            hog_acquired,
            io_proc_id,
            ctx,
            shared,
            active,
        })
    }
}

impl OutputStream for CoreAudioHalStream {
    fn active_output(&self) -> &ActiveOutput {
        &self.active
    }
}

impl Drop for CoreAudioHalStream {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        unsafe {
            let _ = AudioDeviceStop(self.dev_id, self.io_proc_id);
            let _ = AudioDeviceDestroyIOProcID(self.dev_id, self.io_proc_id);
            if let (Some(sid), Some(orig)) = (self.stream_id, self.original_phys) {
                let phys_addr = prop_addr(
                    kAudioStreamPropertyPhysicalFormat,
                    kAudioObjectPropertyScopeGlobal,
                    kAudioObjectPropertyElementMain,
                );
                let _ = AudioObjectSetPropertyData(
                    sid,
                    &phys_addr,
                    0,
                    null(),
                    size_of::<AudioStreamBasicDescription>() as u32,
                    (&orig as *const AudioStreamBasicDescription).cast::<c_void>(),
                );
            }
            if self.hog_acquired {
                let hog_addr = prop_addr(
                    kAudioDevicePropertyHogMode,
                    kAudioObjectPropertyScopeOutput,
                    kAudioObjectPropertyElementMain,
                );
                let mut free_pid: i32 = -1;
                let _ = AudioObjectSetPropertyData(
                    self.dev_id,
                    &hog_addr,
                    0,
                    null(),
                    size_of::<i32>() as u32,
                    (&mut free_pid as *mut i32).cast::<c_void>(),
                );
            }
            drop(Box::from_raw(self.ctx));
        }
    }
}

// ---------------------------------------------------------------------------
// Shared Mode: CoreAudio AUHAL (Interleaved F32, system mixing + resampling)
// ---------------------------------------------------------------------------

struct AuhalContext {
    consumer: Consumer<i32>,
    shared: Arc<StreamShared>,
    channels: usize,
    drain_silence_callbacks: u8,
    scratch: Vec<i32>,
}

unsafe extern "C" fn auhal_render_cb(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut AudioUnitRenderActionFlags,
    _in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    if in_ref_con.is_null() || io_data.is_null() {
        return 0;
    }
    let ctx = unsafe { &mut *(in_ref_con as *mut AuhalContext) };
    let abl = unsafe { &mut *io_data };
    if abl.mNumberBuffers == 0 {
        return 0;
    }
    let buf = unsafe { &mut *abl.mBuffers.as_mut_ptr() };
    if buf.mData.is_null() || buf.mDataByteSize == 0 {
        return 0;
    }
    let out =
        unsafe { slice::from_raw_parts_mut(buf.mData as *mut u8, buf.mDataByteSize as usize) };
    let frames = in_number_frames as usize;
    let ch = ctx.channels;

    ctx.shared.maybe_flush(&mut ctx.consumer);
    if ctx.shared.is_paused() || ctx.shared.is_stopped() || frames == 0 {
        out.fill(0);
        return 0;
    }

    let avail_frames = ctx.consumer.slots() / ch;
    if avail_frames == 0 {
        out.fill(0);
        if ctx.shared.is_draining() {
            ctx.drain_silence_callbacks = ctx.drain_silence_callbacks.saturating_add(1);
            if ctx.drain_silence_callbacks >= 2 {
                ctx.shared.finished.store(true, Ordering::Release);
            }
        } else {
            ctx.drain_silence_callbacks = 0;
            ctx.shared.underruns.fetch_add(1, Ordering::Relaxed);
        }
        return 0;
    }
    ctx.drain_silence_callbacks = 0;

    let need_samples = frames * ch;
    let need_bytes = need_samples * 4;
    if ctx.scratch.len() < need_samples || out.len() < need_bytes {
        out.fill(0);
        return 0;
    }
    let take_frames = avail_frames.min(frames);
    let dst_slice = &mut ctx.scratch[..need_samples];
    dst_slice.fill(0);
    if let Ok(chunk) = ctx.consumer.read_chunk(take_frames * ch) {
        let (s1, s2) = chunk.as_slices();
        let n1 = s1.len();
        dst_slice[..n1].copy_from_slice(s1);
        dst_slice[n1..n1 + s2.len()].copy_from_slice(s2);
        chunk.commit_all();
    }

    let gain = ctx.shared.gain_q16();
    pack_interleaved(dst_slice, out, SampleFormat::F32, gain);

    ctx.shared
        .frames_played
        .fetch_add(take_frames as u64, Ordering::Relaxed);

    0
}

struct CoreAudioAuhalStream {
    unit: AudioComponentInstance,
    ctx: *mut AuhalContext,
    shared: Arc<StreamShared>,
    active: ActiveOutput,
}

unsafe impl Send for CoreAudioAuhalStream {}

impl CoreAudioAuhalStream {
    fn open(
        dev_id: AudioDeviceID,
        dev_uid: String,
        dev_name: String,
        spec: PcmSpec,
        consumer: Consumer<i32>,
        shared: Arc<StreamShared>,
    ) -> Result<Self, String> {
        let desc = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_HALOutput,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = unsafe { AudioComponentFindNext(null_mut(), &desc) };
        if comp.is_null() {
            return Err("CoreAudio HALOutput AudioComponent not found".into());
        }

        let mut unit: AudioComponentInstance = null_mut();
        let st = unsafe { AudioComponentInstanceNew(comp, &mut unit) };
        if st != 0 || unit.is_null() {
            return Err(format!("AudioComponentInstanceNew failed (OSStatus {st})"));
        }

        // Bind AUHAL to the selected output device.
        let st = unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_CurrentDevice,
                kAudioUnitScope_Global,
                0,
                (&dev_id as *const AudioDeviceID).cast::<c_void>(),
                size_of::<AudioDeviceID>() as u32,
            )
        };
        if st != 0 {
            unsafe { AudioComponentInstanceDispose(unit) };
            return Err(format!(
                "AUHAL CurrentDevice failed on {dev_name:?} (OSStatus {st})"
            ));
        }

        // Request interleaved F32 at the track's sample rate & channels.
        let ch = spec.channels.max(1) as u32;
        let asbd = AudioStreamBasicDescription {
            mSampleRate: spec.sample_rate as f64,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 4 * ch,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4 * ch,
            mChannelsPerFrame: ch,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let st = unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Input,
                0,
                (&asbd as *const AudioStreamBasicDescription).cast::<c_void>(),
                size_of::<AudioStreamBasicDescription>() as u32,
            )
        };
        if st != 0 {
            unsafe { AudioComponentInstanceDispose(unit) };
            return Err(format!("AUHAL StreamFormat failed (OSStatus {st})"));
        }

        let ctx = Box::into_raw(Box::new(AuhalContext {
            consumer,
            shared: Arc::clone(&shared),
            channels: ch as usize,
            drain_silence_callbacks: 0,
            scratch: vec![0i32; 16_384 * ch as usize],
        }));

        let cb = AURenderCallbackStruct {
            inputProc: Some(auhal_render_cb),
            inputProcRefCon: ctx.cast::<c_void>(),
        };
        let st = unsafe {
            AudioUnitSetProperty(
                unit,
                kAudioUnitProperty_SetRenderCallback,
                kAudioUnitScope_Input,
                0,
                (&cb as *const AURenderCallbackStruct).cast::<c_void>(),
                size_of::<AURenderCallbackStruct>() as u32,
            )
        };
        if st != 0 {
            unsafe {
                AudioComponentInstanceDispose(unit);
                drop(Box::from_raw(ctx));
            }
            return Err(format!("AUHAL SetRenderCallback failed (OSStatus {st})"));
        }

        let st = unsafe { AudioUnitInitialize(unit) };
        if st != 0 {
            unsafe {
                AudioComponentInstanceDispose(unit);
                drop(Box::from_raw(ctx));
            }
            return Err(format!("AudioUnitInitialize failed (OSStatus {st})"));
        }

        let st = unsafe { AudioOutputUnitStart(unit) };
        if st != 0 {
            unsafe {
                AudioUnitUninitialize(unit);
                AudioComponentInstanceDispose(unit);
                drop(Box::from_raw(ctx));
            }
            return Err(format!("AudioOutputUnitStart failed (OSStatus {st})"));
        }

        let active = ActiveOutput {
            device_id: format!("coreaudio/{dev_uid}"),
            device_name: dev_name,
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            sample_format: SampleFormat::F32,
            exclusive: false,
            hardware_volume: open_hardware_volume(Some(&dev_uid)).is_some(),
        };

        Ok(Self {
            unit,
            ctx,
            shared,
            active,
        })
    }
}

impl OutputStream for CoreAudioAuhalStream {
    fn active_output(&self) -> &ActiveOutput {
        &self.active
    }
}

impl Drop for CoreAudioAuhalStream {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        unsafe {
            let _ = AudioOutputUnitStop(self.unit);
            let _ = AudioUnitUninitialize(self.unit);
            let _ = AudioComponentInstanceDispose(self.unit);
            drop(Box::from_raw(self.ctx));
        }
    }
}

// ---------------------------------------------------------------------------
// Hardware Volume & Mute Controller (Cached selectors per verified device ID)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct VolCache {
    device_id: AudioDeviceID,
    vol_elements: Vec<u32>,
    mute_elements: Vec<u32>,
    probed: bool,
}

pub struct CoreAudioVolume {
    uid: Option<String>,
    cache: Mutex<VolCache>,
}

/// Create a CoreAudio hardware volume controller for `uid` (or the default
/// output device when `uid` is `None`), if the device exposes settable volume.
pub fn open_hardware_volume(uid: Option<&str>) -> Option<CoreAudioVolume> {
    let ctrl = CoreAudioVolume {
        uid: uid.map(str::to_string),
        cache: Mutex::new(VolCache::default()),
    };
    if ctrl.get_volume().is_ok() {
        Some(ctrl)
    } else {
        None
    }
}

fn is_settable(dev: AudioDeviceID, selector: u32, element: u32) -> bool {
    let addr = prop_addr(selector, kAudioObjectPropertyScopeOutput, element);
    if unsafe { AudioObjectHasProperty(dev, &addr) } == 0 {
        return false;
    }
    let mut settable: u8 = 0;
    let st = unsafe { AudioObjectIsPropertySettable(dev, &addr, &mut settable) };
    st == 0 && settable != 0
}

impl CoreAudioVolume {
    fn resolve_and_cache(&self, cache: &mut VolCache) -> Result<AudioDeviceID, String> {
        let dev = if cache.probed && cache.device_id != K_AUDIO_OBJECT_UNKNOWN {
            let still_matches = match &self.uid {
                Some(want) => {
                    cfstring_property(
                        cache.device_id,
                        kAudioDevicePropertyDeviceUID,
                        kAudioObjectPropertyScopeGlobal,
                    )
                    .as_deref()
                        == Some(want.as_str())
                }
                None => default_output_device_id() == Some(cache.device_id),
            };
            if still_matches {
                return Ok(cache.device_id);
            }
            *cache = VolCache::default();
            resolve_device(self.uid.as_deref())?.0
        } else {
            resolve_device(self.uid.as_deref())?.0
        };

        let mut vol_elements = Vec::new();
        if is_settable(
            dev,
            kAudioDevicePropertyVolumeScalar,
            kAudioObjectPropertyElementMain,
        ) {
            vol_elements.push(kAudioObjectPropertyElementMain);
        } else {
            for ch in [1u32, 2u32] {
                if is_settable(dev, kAudioDevicePropertyVolumeScalar, ch) {
                    vol_elements.push(ch);
                }
            }
        }
        if vol_elements.is_empty() {
            return Err("CoreAudio device has no settable volume scalar".into());
        }

        let mut mute_elements = Vec::new();
        if is_settable(
            dev,
            kAudioDevicePropertyMute,
            kAudioObjectPropertyElementMain,
        ) {
            mute_elements.push(kAudioObjectPropertyElementMain);
        } else {
            for ch in [1u32, 2u32] {
                if is_settable(dev, kAudioDevicePropertyMute, ch) {
                    mute_elements.push(ch);
                }
            }
        }

        cache.device_id = dev;
        cache.vol_elements = vol_elements;
        cache.mute_elements = mute_elements;
        cache.probed = true;
        Ok(dev)
    }
}

impl HardwareVolume for CoreAudioVolume {
    fn get_volume(&self) -> Result<u8, String> {
        let mut cache = self.cache.lock().unwrap();
        let dev = self.resolve_and_cache(&mut cache)?;
        let mut sum = 0.0f32;
        let mut count = 0usize;
        for &el in &cache.vol_elements {
            let addr = prop_addr(
                kAudioDevicePropertyVolumeScalar,
                kAudioObjectPropertyScopeOutput,
                el,
            );
            let mut val: f32 = 0.0;
            let mut sz = size_of::<f32>() as u32;
            let st = unsafe {
                AudioObjectGetPropertyData(
                    dev,
                    &addr,
                    0,
                    null(),
                    &mut sz,
                    (&mut val as *mut f32).cast::<c_void>(),
                )
            };
            if st != 0 {
                *cache = VolCache::default();
                return Err(format!("CoreAudio get volume failed (OSStatus {st})"));
            }
            sum += val;
            count += 1;
        }
        if count == 0 {
            return Err("no volume channels".into());
        }
        let pct = ((sum / count as f32).clamp(0.0, 1.0) * 100.0).round() as u8;
        Ok(pct)
    }

    fn set_volume(&self, volume: u8) -> Result<(), String> {
        let mut cache = self.cache.lock().unwrap();
        let dev = self.resolve_and_cache(&mut cache)?;
        let scalar = (volume.min(100) as f32) / 100.0;
        for &el in &cache.vol_elements {
            let addr = prop_addr(
                kAudioDevicePropertyVolumeScalar,
                kAudioObjectPropertyScopeOutput,
                el,
            );
            let st = unsafe {
                AudioObjectSetPropertyData(
                    dev,
                    &addr,
                    0,
                    null(),
                    size_of::<f32>() as u32,
                    (&scalar as *const f32).cast::<c_void>(),
                )
            };
            if st != 0 {
                *cache = VolCache::default();
                return Err(format!("CoreAudio set volume failed (OSStatus {st})"));
            }
        }
        Ok(())
    }

    fn get_mute(&self) -> Result<bool, String> {
        let mut cache = self.cache.lock().unwrap();
        let dev = self.resolve_and_cache(&mut cache)?;
        if cache.mute_elements.is_empty() {
            return Ok(false);
        }
        let mut all_muted = true;
        for &el in &cache.mute_elements {
            let addr = prop_addr(
                kAudioDevicePropertyMute,
                kAudioObjectPropertyScopeOutput,
                el,
            );
            let mut val: u32 = 0;
            let mut sz = size_of::<u32>() as u32;
            let st = unsafe {
                AudioObjectGetPropertyData(
                    dev,
                    &addr,
                    0,
                    null(),
                    &mut sz,
                    (&mut val as *mut u32).cast::<c_void>(),
                )
            };
            if st != 0 {
                *cache = VolCache::default();
                return Err(format!("CoreAudio get mute failed (OSStatus {st})"));
            }
            if val == 0 {
                all_muted = false;
            }
        }
        Ok(all_muted)
    }

    fn set_mute(&self, muted: bool) -> Result<(), String> {
        let mut cache = self.cache.lock().unwrap();
        let dev = self.resolve_and_cache(&mut cache)?;
        if cache.mute_elements.is_empty() {
            return Err("CoreAudio device does not support hardware mute".into());
        }
        let val: u32 = if muted { 1 } else { 0 };
        for &el in &cache.mute_elements {
            let addr = prop_addr(
                kAudioDevicePropertyMute,
                kAudioObjectPropertyScopeOutput,
                el,
            );
            let st = unsafe {
                AudioObjectSetPropertyData(
                    dev,
                    &addr,
                    0,
                    null(),
                    size_of::<u32>() as u32,
                    (&val as *const u32).cast::<c_void>(),
                )
            };
            if st != 0 {
                *cache = VolCache::default();
                return Err(format!("CoreAudio set mute failed (OSStatus {st})"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coreaudio_sys::AudioValueRange;

    fn dragonfly_ranged(rate: f64) -> AudioStreamRangedDescription {
        // Exact descriptor reported by AudioQuest DragonFly Black v1.5 in Issue #1:
        // 24-bit int LE signed packed, 6 bytes per frame (2ch x 3 bytes).
        AudioStreamRangedDescription {
            mFormat: AudioStreamBasicDescription {
                mSampleRate: rate,
                mFormatID: kAudioFormatLinearPCM,
                mFormatFlags: kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked,
                mBytesPerPacket: 6,
                mFramesPerPacket: 1,
                mBytesPerFrame: 6,
                mChannelsPerFrame: 2,
                mBitsPerChannel: 24,
                mReserved: 0,
            },
            mSampleRateRange: AudioValueRange {
                mMinimum: rate,
                mMaximum: rate,
            },
        }
    }

    #[test]
    fn selects_s24_packed_for_dragonfly_black() {
        let available = [
            dragonfly_ranged(96_000.0),
            dragonfly_ranged(88_200.0),
            dragonfly_ranged(48_000.0),
            dragonfly_ranged(44_100.0),
        ];

        let (asbd, fmt, ni) = select_best_physical_format(&available, PcmSpec::new(44_100, 2, 16))
            .expect("DragonFly Black 24-bit packed must match");
        assert_eq!(asbd.mSampleRate, 44_100.0);
        assert_eq!(fmt, SampleFormat::S24Packed);
        assert!(!ni);

        let (asbd, fmt, _) = select_best_physical_format(&available, PcmSpec::new(96_000, 2, 24))
            .expect("96kHz 24-bit must match");
        assert_eq!(asbd.mSampleRate, 96_000.0);
        assert_eq!(fmt, SampleFormat::S24Packed);

        // 192 kHz on a 96 kHz-max DragonFly Black must return None rather than
        // silently playing at half speed.
        assert!(select_best_physical_format(&available, PcmSpec::new(192_000, 2, 24)).is_none());
    }

    #[test]
    fn coreaudio_read_only_smoke() {
        // Non-mutating read-only smoke test against live macOS HAL.
        let devices = list_devices();
        // Every listed device must have non-empty coreaudio/<uid> id.
        for d in &devices {
            assert!(d.id.starts_with("coreaudio/"), "unexpected id {}", d.id);
            assert!(!d.description.is_empty());
        }
    }
}
