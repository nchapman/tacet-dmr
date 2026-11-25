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
}
