//! PCM format definitions and bit-exact integer sample packing.
//!
//! Internally the audio pipeline carries interleaved left-aligned `i32` (Q31)
//! samples matching `symphonia`'s `SampleBuffer<i32>` convention:
//! - 16-bit PCM occupies the top 16 bits (`s << 16`).
//! - 24-bit PCM occupies the top 24 bits (`s << 8`).
//! - 32-bit PCM occupies all 32 bits.
//!
//! This keeps 16/24/32-bit integer tracks 100% bit-exact from decoder to DAC
//! DMA buffer without a floating-point roundtrip, including 3-byte packed
//! 24-bit (`S24Packed`) USB DACs.

use std::fmt;

/// Physical wire / DMA sample format accepted by an audio output sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    /// 16-bit signed little-endian integer (2 bytes/sample).
    S16,
    /// 24-bit signed little-endian tightly packed integer (3 bytes/sample).
    S24Packed,
    /// 24-bit signed little-endian integer padded in the top 3 bytes of a 32-bit word.
    S24In32Left,
    /// 24-bit signed little-endian integer padded in the low 3 bytes of a 32-bit word.
    S24In32Right,
    /// 32-bit signed little-endian integer (4 bytes/sample).
    S32,
    /// 32-bit IEEE-754 little-endian floating point (4 bytes/sample).
    F32,
}

impl SampleFormat {
    /// Bytes occupied by one sample of one channel.
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::S16 => 2,
            Self::S24Packed => 3,
            Self::S24In32Left | Self::S24In32Right | Self::S32 | Self::F32 => 4,
        }
    }

    /// Valid bit depth of the format.
    pub const fn bit_depth(self) -> u16 {
        match self {
            Self::S16 => 16,
            Self::S24Packed | Self::S24In32Left | Self::S24In32Right => 24,
            Self::S32 | Self::F32 => 32,
        }
    }

    /// Short human-readable label for logs and diagnostics.
    pub const fn label(self) -> &'static str {
        match self {
            Self::S16 => "s16le",
            Self::S24Packed => "s24le3",
            Self::S24In32Left => "s24le4-lj",
            Self::S24In32Right => "s24le4-rj",
            Self::S32 => "s32le",
            Self::F32 => "f32le",
        }
    }
}

impl fmt::Display for SampleFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Stream format metadata for decoded PCM audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PcmSpec {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
}

impl PcmSpec {
    pub const fn new(sample_rate: u32, channels: u16, bits_per_sample: u16) -> Self {
        Self {
            sample_rate,
            channels,
            bits_per_sample,
        }
    }

    /// Two tracks can transition gaplessly without reconfiguring hardware when
    /// their sample rate and channel count match.
    pub const fn hardware_compatible(self, other: Self) -> bool {
        self.sample_rate == other.sample_rate && self.channels == other.channels
    }
}

/// Scale factor in Q16 fixed point (65536 = unity 1.0) for a 0..=100 volume
/// setting. Uses a gentle perceptual curve (v/100)^2 when below unity and
/// exact 65536 at 100 so unity gain is bit-exact pass-through.
#[inline]
pub fn volume_gain_q16(volume: u8, muted: bool) -> i64 {
    if muted || volume == 0 {
        return 0;
    }
    if volume >= 100 {
        return 1 << 16;
    }
    // Quadratic perceptual taper (approx 40 dB range from 10..100, smooth to 0).
    let v = volume as f64 / 100.0;
    let gain = v * v;
    (gain * 65536.0).round() as i64
}

/// Apply Q16 gain to a Q31 sample. Exact pass-through when `gain_q16 == 65536`.
#[inline]
pub fn apply_gain(sample: i32, gain_q16: i64) -> i32 {
    if gain_q16 == (1 << 16) {
        return sample;
    }
    if gain_q16 == 0 {
        return 0;
    }
    let scaled = ((sample as i64) * gain_q16) >> 16;
    scaled.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// Pack interleaved Q31 `i32` samples into `dst` using `format` and `gain_q16`.
///
/// Returns the number of bytes written to `dst`.
///
/// # Panics
/// Panics if `dst.len() < samples.len() * format.bytes_per_sample()`.
pub fn pack_interleaved(
    samples: &[i32],
    dst: &mut [u8],
    format: SampleFormat,
    gain_q16: i64,
) -> usize {
    let bps = format.bytes_per_sample();
    let needed = samples.len() * bps;
    assert!(
        dst.len() >= needed,
        "output buffer too small: {} < {}",
        dst.len(),
        needed
    );

    match format {
        SampleFormat::S16 => {
            for (i, &s) in samples.iter().enumerate() {
                let v = (apply_gain(s, gain_q16) >> 16) as i16;
                dst[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::S24Packed => {
            for (i, &s) in samples.iter().enumerate() {
                let b = apply_gain(s, gain_q16).to_le_bytes();
                let off = i * 3;
                dst[off] = b[1];
                dst[off + 1] = b[2];
                dst[off + 2] = b[3];
            }
        }
        SampleFormat::S24In32Left => {
            for (i, &s) in samples.iter().enumerate() {
                let v = apply_gain(s, gain_q16) & !0xFF;
                dst[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::S24In32Right => {
            for (i, &s) in samples.iter().enumerate() {
                let v = apply_gain(s, gain_q16) >> 8;
                dst[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::S32 => {
            for (i, &s) in samples.iter().enumerate() {
                let v = apply_gain(s, gain_q16);
                dst[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::F32 => {
            const INV_SCALE: f32 = 1.0 / 2_147_483_648.0;
            for (i, &s) in samples.iter().enumerate() {
                let v = (apply_gain(s, gain_q16) as f32) * INV_SCALE;
                dst[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
    }

    needed
}

/// Pack a single channel from interleaved Q31 `samples` into a non-interleaved
/// (planar) destination buffer `dst`.
pub fn pack_planar_channel(
    samples: &[i32],
    channels: usize,
    channel_idx: usize,
    dst: &mut [u8],
    format: SampleFormat,
    gain_q16: i64,
) -> usize {
    let frames = samples.len() / channels;
    let bps = format.bytes_per_sample();
    let needed = frames * bps;
    assert!(dst.len() >= needed);

    match format {
        SampleFormat::S16 => {
            for f in 0..frames {
                let s = samples[f * channels + channel_idx];
                let v = (apply_gain(s, gain_q16) >> 16) as i16;
                dst[f * 2..f * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::S24Packed => {
            for f in 0..frames {
                let s = samples[f * channels + channel_idx];
                let b = apply_gain(s, gain_q16).to_le_bytes();
                let off = f * 3;
                dst[off] = b[1];
                dst[off + 1] = b[2];
                dst[off + 2] = b[3];
            }
        }
        SampleFormat::S24In32Left => {
            for f in 0..frames {
                let s = samples[f * channels + channel_idx];
                let v = apply_gain(s, gain_q16) & !0xFF;
                dst[f * 4..f * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::S24In32Right => {
            for f in 0..frames {
                let s = samples[f * channels + channel_idx];
                let v = apply_gain(s, gain_q16) >> 8;
                dst[f * 4..f * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::S32 => {
            for f in 0..frames {
                let s = samples[f * channels + channel_idx];
                let v = apply_gain(s, gain_q16);
                dst[f * 4..f * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        SampleFormat::F32 => {
            const INV_SCALE: f32 = 1.0 / 2_147_483_648.0;
            for f in 0..frames {
                let s = samples[f * channels + channel_idx];
                let v = (apply_gain(s, gain_q16) as f32) * INV_SCALE;
                dst[f * 4..f * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
    }

    needed
}

/// Unpack interleaved signed big-endian PCM bytes (16, 24, or 32 bit — e.g.
/// OpenHome Songcast wire format) into left-aligned Q31 `i32` samples.
///
/// Preserves 16-, 24-, and 32-bit integer values losslessly without floating-point
/// conversion.
pub fn pcm_be_into_q31(bytes: &[u8], bit_depth: u8, out: &mut Vec<i32>) {
    out.clear();
    match bit_depth {
        16 => {
            out.reserve(bytes.len() / 2);
            for c in bytes.as_chunks::<2>().0 {
                let s = i16::from_be_bytes(*c) as i32;
                out.push(s << 16);
            }
        }
        24 => {
            out.reserve(bytes.len() / 3);
            for &[b0, b1, b2] in bytes.as_chunks::<3>().0 {
                out.push(i32::from_be_bytes([b0, b1, b2, 0]));
            }
        }
        32 => {
            out.reserve(bytes.len() / 4);
            for c in bytes.as_chunks::<4>().0 {
                out.push(i32::from_be_bytes(*c));
            }
        }
        _ => {}
    }
}

/// Pack interleaved left-aligned Q31 `i32` samples into signed big-endian PCM
/// bytes (16, 24, or 32 bit — e.g. OpenHome Songcast wire format).
pub fn q31_into_pcm_be(samples: &[i32], bit_depth: u8, out: &mut Vec<u8>) {
    out.clear();
    match bit_depth {
        16 => {
            out.reserve(samples.len() * 2);
            for &s in samples {
                let b = ((s >> 16) as i16).to_be_bytes();
                out.extend_from_slice(&b);
            }
        }
        24 => {
            out.reserve(samples.len() * 3);
            for &s in samples {
                let b = s.to_be_bytes();
                out.extend_from_slice(&b[0..3]);
            }
        }
        32 => {
            out.reserve(samples.len() * 4);
            for &s in samples {
                out.extend_from_slice(&s.to_be_bytes());
            }
        }
        _ => {}
    }
}

/// Lightweight linear interpolator for fine clock-drift compensation (in ppm)
/// over interleaved Q31 `i32` PCM frames.
///
/// Only engaged when network stream clock skew exceeds a threshold (e.g.
/// `|skew_ppm| >= 2.0`), preserving bit-exact integer pass-through otherwise.
#[derive(Debug, Clone)]
pub struct DriftResampler {
    channels: usize,
    step: f64,
    phase: f64,
    prev_frame: Vec<i32>,
    has_prev: bool,
}

impl DriftResampler {
    pub fn new(channels: u16) -> Self {
        let ch = channels.max(1) as usize;
        Self {
            channels: ch,
            step: 1.0,
            phase: 0.0,
            prev_frame: vec![0; ch],
            has_prev: false,
        }
    }

    /// Set rate adjustment in parts-per-million (clamped to ±2000 ppm).
    /// Positive `ppm` consumes input slightly faster (shortening output slightly).
    pub fn set_rate_adjust_ppm(&mut self, ppm: f64) {
        let clamped = if ppm.is_finite() {
            ppm.clamp(-2000.0, 2000.0)
        } else {
            0.0
        };
        self.step = 1.0 + clamped * 1e-6;
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
        self.prev_frame.fill(0);
        self.has_prev = false;
    }

    /// Record the last frame of a passthrough (unresampled) chunk so re-engaging
    /// drift compensation later interpolates smoothly without a click.
    pub fn update_passthrough(&mut self, input: &[i32]) {
        let ch = self.channels;
        let in_frames = input.len() / ch;
        if in_frames > 0 {
            let last_off = (in_frames - 1) * ch;
            self.prev_frame
                .copy_from_slice(&input[last_off..last_off + ch]);
            self.has_prev = true;
            self.phase = 0.0;
        }
    }

    /// Resample interleaved Q31 `input` into `out` (cleared first).
    pub fn process(&mut self, input: &[i32], out: &mut Vec<i32>) {
        out.clear();
        let ch = self.channels;
        let in_frames = input.len() / ch;
        if in_frames == 0 {
            return;
        }
        if !self.has_prev {
            self.prev_frame.copy_from_slice(&input[..ch]);
            self.has_prev = true;
        }

        let est_frames = ((in_frames as f64) / self.step).ceil() as usize + 2;
        out.reserve(est_frames * ch);

        // Index -1 refers to `self.prev_frame`; 0..in_frames refer to `input`.
        // Stop before `in_frames - 1` so `next_idx` is always `< in_frames`;
        // subtracting `in_frames` then places `self.phase` in `[-1.0, 0.0)`
        // for seamless `prev_frame -> input[0]` interpolation on the next chunk.
        let limit = (in_frames - 1) as f64;
        while self.phase < limit {
            let idx = self.phase.floor() as isize;
            let frac = self.phase - idx as f64;
            let next_idx = (idx + 1) as usize;
            let f0 = if idx < 0 {
                &self.prev_frame[..]
            } else {
                let o = (idx as usize) * ch;
                &input[o..o + ch]
            };
            let f1_off = next_idx * ch;
            let f1 = &input[f1_off..f1_off + ch];
            for c in 0..ch {
                let a = f0[c] as f64;
                let b = f1[c] as f64;
                let v = (a + (b - a) * frac)
                    .round()
                    .clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                out.push(v);
            }
            self.phase += self.step;
        }

        self.phase -= in_frames as f64;
        let last_off = (in_frames - 1) * ch;
        self.prev_frame
            .copy_from_slice(&input[last_off..last_off + ch]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_s24_packed_is_bit_exact() {
        // 24-bit signed samples: +0x7FFFFF, -0x800000, +0x123456, -1 (0xFFFFFF)
        let raw24: [i32; 4] = [0x007F_FFFF, -0x0080_0000, 0x0012_3456, -1];
        let q31: Vec<i32> = raw24.iter().map(|&v| v << 8).collect();
        let mut out = [0u8; 12];
        let n = pack_interleaved(&q31, &mut out, SampleFormat::S24Packed, 1 << 16);
        assert_eq!(n, 12);
        assert_eq!(&out[0..3], &[0xFF, 0xFF, 0x7F]);
        assert_eq!(&out[3..6], &[0x00, 0x00, 0x80]);
        assert_eq!(&out[6..9], &[0x56, 0x34, 0x12]);
        assert_eq!(&out[9..12], &[0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn pack_s16_and_s32_and_f32_unity() {
        let q31 = [i32::MAX & !0xFFFF, i32::MIN, 0];
        let mut out16 = [0u8; 6];
        pack_interleaved(&q31, &mut out16, SampleFormat::S16, 1 << 16);
        assert_eq!(i16::from_le_bytes([out16[0], out16[1]]), i16::MAX);
        assert_eq!(i16::from_le_bytes([out16[2], out16[3]]), i16::MIN);
        assert_eq!(i16::from_le_bytes([out16[4], out16[5]]), 0);

        let mut out_f32 = [0u8; 12];
        pack_interleaved(
            &[0, i32::MIN, 1 << 30],
            &mut out_f32,
            SampleFormat::F32,
            1 << 16,
        );
        let f0 = f32::from_le_bytes(out_f32[0..4].try_into().unwrap());
        let f1 = f32::from_le_bytes(out_f32[4..8].try_into().unwrap());
        let f2 = f32::from_le_bytes(out_f32[8..12].try_into().unwrap());
        assert_eq!(f0, 0.0);
        assert_eq!(f1, -1.0);
        assert_eq!(f2, 0.5);
    }

    #[test]
    fn pack_muted_is_zero() {
        let q31 = [0x1234_5600, -0x4321_0000];
        let mut out = [0xAAu8; 6];
        pack_interleaved(
            &q31,
            &mut out,
            SampleFormat::S24Packed,
            volume_gain_q16(50, true),
        );
        assert_eq!(out, [0u8; 6]);
    }

    #[test]
    fn pack_s24_in_32_left_and_right_are_bit_exact() {
        let raw24: [i32; 2] = [0x0012_3456, -0x0000_0001];
        let q31: Vec<i32> = raw24.iter().map(|&v| v << 8).collect();

        let mut left = [0u8; 8];
        pack_interleaved(&q31, &mut left, SampleFormat::S24In32Left, 1 << 16);
        assert_eq!(
            i32::from_le_bytes(left[0..4].try_into().unwrap()),
            0x1234_5600
        );
        assert_eq!(
            i32::from_le_bytes(left[4..8].try_into().unwrap()),
            -256 // 0xFFFF_FF00
        );

        let mut right = [0u8; 8];
        pack_interleaved(&q31, &mut right, SampleFormat::S24In32Right, 1 << 16);
        assert_eq!(
            i32::from_le_bytes(right[0..4].try_into().unwrap()),
            0x0012_3456
        );
        assert_eq!(i32::from_le_bytes(right[4..8].try_into().unwrap()), -1);
    }

    #[test]
    fn pack_planar_channel_separates_left_and_right() {
        // 2 frames of stereo: (L0, R0), (L1, R1)
        let stereo = [0x1111_0000i32, 0x2222_0000, 0x3333_0000, 0x4444_0000];
        let mut ch0 = [0u8; 4];
        let mut ch1 = [0u8; 4];
        assert_eq!(
            pack_planar_channel(&stereo, 2, 0, &mut ch0, SampleFormat::S16, 1 << 16),
            4
        );
        assert_eq!(
            pack_planar_channel(&stereo, 2, 1, &mut ch1, SampleFormat::S16, 1 << 16),
            4
        );
        assert_eq!(i16::from_le_bytes(ch0[0..2].try_into().unwrap()), 0x1111);
        assert_eq!(i16::from_le_bytes(ch0[2..4].try_into().unwrap()), 0x3333);
        assert_eq!(i16::from_le_bytes(ch1[0..2].try_into().unwrap()), 0x2222);
        assert_eq!(i16::from_le_bytes(ch1[2..4].try_into().unwrap()), 0x4444);
    }

    #[test]
    fn volume_gain_q16_is_monotonic_and_clamped() {
        assert_eq!(volume_gain_q16(0, false), 0);
        assert_eq!(volume_gain_q16(100, false), 1 << 16);
        assert_eq!(volume_gain_q16(255, false), 1 << 16);
        assert_eq!(volume_gain_q16(100, true), 0);
        let mut prev = 0;
        for v in 1..=100 {
            let g = volume_gain_q16(v, false);
            assert!(g >= prev, "volume {v} gain {g} < prev {prev}");
            prev = g;
        }
    }

    #[test]
    fn pcm_be_q31_roundtrip_16_24_32() {
        // 16-bit big-endian
        let raw16: Vec<u8> = [0x7FFFi16, -0x8000i16, 0x1234i16, -1i16]
            .into_iter()
            .flat_map(i16::to_be_bytes)
            .collect();
        let mut q31 = Vec::new();
        pcm_be_into_q31(&raw16, 16, &mut q31);
        assert_eq!(q31, vec![0x7FFF_0000, i32::MIN, 0x1234_0000, -0x10000]);
        let mut back16 = Vec::new();
        q31_into_pcm_be(&q31, 16, &mut back16);
        assert_eq!(back16, raw16);

        // 24-bit big-endian (+0x7FFFFF, -0x800000, +0x123456, -1)
        let raw24: Vec<u8> = vec![
            0x7F, 0xFF, 0xFF, 0x80, 0x00, 0x00, 0x12, 0x34, 0x56, 0xFF, 0xFF, 0xFF,
        ];
        pcm_be_into_q31(&raw24, 24, &mut q31);
        assert_eq!(q31, vec![0x7FFF_FF00, i32::MIN, 0x1234_5600, -256]);
        let mut back24 = Vec::new();
        q31_into_pcm_be(&q31, 24, &mut back24);
        assert_eq!(back24, raw24);

        // 32-bit big-endian
        let raw32: Vec<u8> = [i32::MAX, i32::MIN, 0x0102_0304, -42]
            .into_iter()
            .flat_map(i32::to_be_bytes)
            .collect();
        pcm_be_into_q31(&raw32, 32, &mut q31);
        let mut back32 = Vec::new();
        q31_into_pcm_be(&q31, 32, &mut back32);
        assert_eq!(back32, raw32);
    }

    #[test]
    fn drift_resampler_adjusts_frame_count() {
        let mut r = DriftResampler::new(2);
        r.set_rate_adjust_ppm(1000.0); // +1000 ppm -> ~1 fewer frame per 1000
        let input: Vec<i32> = (0..2000).map(|i| i * 1000).collect(); // 1000 stereo frames
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert_eq!(out.len() % 2, 0);
        let out_frames = out.len() / 2;
        assert!(
            (997..=1000).contains(&out_frames),
            "out_frames={out_frames}"
        );

        // Multi-chunk sine wave continuity: no boundary-clamp discontinuity across 240-frame chunks.
        let mut r2 = DriftResampler::new(1);
        r2.set_rate_adjust_ppm(100.0);
        let sr = 48_000.0;
        let freq = 440.0;
        let mut all_out = Vec::new();
        let mut chunk_out = Vec::new();
        for c in 0..20 {
            let chunk: Vec<i32> = (0..240)
                .map(|i| {
                    let t = (c * 240 + i) as f64 / sr;
                    ((2.0 * std::f64::consts::PI * freq * t).sin() * 1_000_000_000.0) as i32
                })
                .collect();
            r2.process(&chunk, &mut chunk_out);
            all_out.extend_from_slice(&chunk_out);
        }
        let max_diff = all_out
            .windows(2)
            .map(|w| (w[1] as i64 - w[0] as i64).abs())
            .max()
            .unwrap_or(0);
        assert!(
            max_diff < 65_000_000,
            "boundary discontinuity detected: max_diff={max_diff}"
        );
    }
}
