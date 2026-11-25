//! Streaming audio decoder backed by `symphonia` + `symphonia-opus`.
//!
//! Decodes FLAC, ALAC, WAV, AIFF, CAF, MP3, AAC, Vorbis, Opus, Ogg, MP4/M4A,
//! and MKV/WebM into interleaved Q31 `i32` samples with gapless trimming enabled.

use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use symphonia::core::codecs::audio::{AudioDecoder as SymphoniaAudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::common::Tier;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use crate::format::PcmSpec;
use crate::source;

fn codec_registry() -> &'static CodecRegistry {
    static REGISTRY: OnceLock<CodecRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut reg = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut reg);
        reg.register_audio_decoder_at_tier::<symphonia_opus::OpusDecoder>(Tier::Fallback);
        reg
    })
}

/// Metadata discovered when opening a track.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub spec: PcmSpec,
    pub duration_secs: Option<f64>,
    pub codec: String,
}

/// Streaming decoder yielding chunks of interleaved Q31 `i32` PCM samples.
pub struct AudioDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn SymphoniaAudioDecoder>,
    track_id: u32,
    info: TrackInfo,
    sample_buf: Vec<i32>,
    time_base_secs: f64,
}

impl AudioDecoder {
    /// Open and probe a URL or file path.
    pub fn open(url: &str, cancel: Arc<AtomicBool>) -> Result<Self, String> {
        let opened = source::open_media(url, cancel).map_err(|e| format!("open media: {e}"))?;
        let mut hint = Hint::new();
        if let Some(ext) = &opened.extension {
            hint.with_extension(ext);
        }
        if let Some(mime) = &opened.content_type {
            hint.mime_type(mime);
        }

        let mss = MediaSourceStream::new(opened.source, Default::default());
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| format!("probe audio format: {e}"))?;

        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| "no supported audio track found".to_string())?;

        let track_id = track.id;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| "audio track missing codec parameters".to_string())?
            .clone();

        let decoder = codec_registry()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|e| format!("create audio decoder: {e}"))?;

        let sample_rate = params
            .sample_rate
            .ok_or_else(|| "audio track missing sample rate".to_string())?;
        let channels = params
            .channels
            .as_ref()
            .map(|c| c.count() as u16)
            .unwrap_or(2)
            .max(1);
        let bits_per_sample = params
            .bits_per_sample
            .or(params.bits_per_coded_sample)
            .unwrap_or(24) as u16;

        let time_base_secs = track
            .time_base
            .map(|tb| tb.numer.get() as f64 / tb.denom.get() as f64)
            .unwrap_or_else(|| 1.0 / sample_rate as f64);

        let duration_secs = track.num_frames.and_then(|frames| {
            if track.time_base.is_some() {
                Some(frames as f64 * time_base_secs)
            } else if sample_rate > 0 {
                Some(frames as f64 / sample_rate as f64)
            } else {
                None
            }
        });

        let codec = format!("{:?}", params.codec);

        Ok(Self {
            format,
            decoder,
            track_id,
            info: TrackInfo {
                spec: PcmSpec::new(sample_rate, channels, bits_per_sample),
                duration_secs,
                codec,
            },
            sample_buf: Vec::new(),
            time_base_secs,
        })
    }

    pub fn info(&self) -> &TrackInfo {
        &self.info
    }

    /// Decode the next packet of interleaved Q31 `i32` samples.
    ///
    /// Returns `Ok(None)` cleanly on end-of-stream.
    pub fn next_chunk(&mut self) -> Result<Option<&[i32]>, String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(None),
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(SymphoniaError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(e) => return Err(format!("read packet: {e}")),
            };

            if packet.track_id != self.track_id {
                continue;
            }

            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    if decoded.frames() == 0 {
                        continue;
                    }
                    self.sample_buf.clear();
                    decoded.copy_to_vec_interleaved(&mut self.sample_buf);
                    return Ok(Some(&self.sample_buf));
                }
                Err(SymphoniaError::DecodeError(_)) => {
                    // Recoverable frame error; keep streaming.
                    continue;
                }
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(e) => return Err(format!("decode packet: {e}")),
            }
        }
    }

    /// Seek to `seconds` from the start of the track. Returns the actual landed
    /// timestamp in seconds.
    pub fn seek(&mut self, seconds: f64) -> Result<f64, String> {
        let clamped = seconds.max(0.0);
        let time = Time::try_from_secs_f64(clamped)
            .ok_or_else(|| format!("invalid seek time {clamped}"))?;
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time,
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| format!("seek to {clamped:.2}s: {e}"))?;
        self.decoder.reset();
        Ok(seeked.actual_ts.get() as f64 * self.time_base_secs)
    }
}
