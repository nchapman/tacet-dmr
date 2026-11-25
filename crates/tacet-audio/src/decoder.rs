//! Streaming audio decoder backed by `symphonia`.
//!
//! Decodes FLAC, ALAC, WAV, AIFF, CAF, MP3, AAC, Vorbis, Ogg, MP4/M4A, and
//! MKV/WebM into interleaved Q31 `i32` samples with gapless trimming enabled.

use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

use crate::format::PcmSpec;
use crate::source;

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
    decoder: Box<dyn Decoder>,
    track_id: u32,
    info: TrackInfo,
    sample_buf: Option<SampleBuffer<i32>>,
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
        let format_opts = FormatOptions {
            enable_gapless: true,
            ..Default::default()
        };
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &format_opts, &MetadataOptions::default())
            .map_err(|e| format!("probe audio format: {e}"))?;

        let format = probed.format;
        let track = format
            .default_track()
            .filter(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .or_else(|| {
                format
                    .tracks()
                    .iter()
                    .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            })
            .ok_or_else(|| "no supported audio track found".to_string())?;

        let track_id = track.id;
        let params = track.codec_params.clone();

        let decoder = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .map_err(|e| format!("create audio decoder: {e}"))?;

        let sample_rate = params
            .sample_rate
            .ok_or_else(|| "audio track missing sample rate".to_string())?;
        let channels = params
            .channels
            .map(|c| c.count() as u16)
            .unwrap_or(2)
            .max(1);
        let bits_per_sample = params
            .bits_per_sample
            .or(params.bits_per_coded_sample)
            .unwrap_or(24) as u16;

        let duration_secs = params.n_frames.and_then(|frames| {
            if let Some(tb) = params.time_base {
                let t = tb.calc_time(frames);
                Some(t.seconds as f64 + t.frac)
            } else if sample_rate > 0 {
                Some(frames as f64 / sample_rate as f64)
            } else {
                None
            }
        });

        let time_base_secs = params
            .time_base
            .map(|tb| tb.numer as f64 / tb.denom as f64)
            .unwrap_or_else(|| 1.0 / sample_rate as f64);

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
            sample_buf: None,
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
                Ok(p) => p,
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(SymphoniaError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(e) => return Err(format!("read packet: {e}")),
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    if decoded.frames() == 0 {
                        continue;
                    }
                    let spec = *decoded.spec();
                    let capacity = decoded.capacity() as u64;
                    let needs_alloc = self
                        .sample_buf
                        .as_ref()
                        .is_none_or(|b| b.capacity() < decoded.frames() * spec.channels.count());
                    if needs_alloc {
                        self.sample_buf = Some(SampleBuffer::<i32>::new(capacity, spec));
                    }
                    let buf = self.sample_buf.as_mut().unwrap();
                    buf.copy_interleaved_ref(decoded);
                    return Ok(Some(buf.samples()));
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
        let whole = clamped.floor() as u64;
        let frac = clamped - whole as f64;
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: Time::new(whole, frac),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|e| format!("seek to {clamped:.2}s: {e}"))?;
        self.decoder.reset();
        Ok(seeked.actual_ts as f64 * self.time_base_secs)
    }
}
