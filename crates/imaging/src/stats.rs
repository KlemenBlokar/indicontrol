//! Per-channel statistics and histograms, with clipping detection.

use serde::{Deserialize, Serialize};

use crate::image::Image;

/// Histogram resolution sent to UIs (bins over `0..=adc_max`).
pub const HIST_BINS: usize = 2048;
const FULL: usize = 1 << 16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelStats {
    /// "L" for mono, "R"/"G"/"B" for colour.
    pub name: String,
    pub pixels: u64,
    pub min: u16,
    pub max: u16,
    pub mean: f64,
    pub median: f64,
    /// Median absolute deviation (raw, not scaled to sigma).
    pub mad: f64,
    /// 0.1th percentile: where the dark end of the signal really starts.
    pub p001: u16,
    /// Pixels exactly at 0 (black clipping).
    pub zero: u64,
    /// Pixels at the saturation level.
    pub saturated: u64,
}

impl ChannelStats {
    /// Robust noise estimate (MAD scaled to a Gaussian sigma).
    pub fn sigma(&self) -> f64 {
        1.4826 * self.mad
    }

    pub fn zero_fraction(&self) -> f64 {
        self.zero as f64 / self.pixels.max(1) as f64
    }

    pub fn saturated_fraction(&self) -> f64 {
        self.saturated as f64 / self.pixels.max(1) as f64
    }

    /// Distance of the dark end of the signal above 0, in sigmas.
    pub fn headroom_sigma(&self) -> Option<f64> {
        let s = self.sigma();
        (s > 0.0).then(|| self.p001 as f64 / s)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Analysis {
    /// Saturation level of the data (e.g. 65535, 65520 for 12-bit data
    /// scaled to 16 bits, or 4095 for unscaled 12-bit data).
    pub adc_max: u16,
    /// Spacing between possible values (16 for 12-bit data scaled to 16 bits).
    pub step: u16,
    pub channels: Vec<ChannelStats>,
    /// One histogram per channel, [`HIST_BINS`] bins spanning `0..=adc_max`.
    pub histograms: Vec<Vec<u32>>,
}

impl Analysis {
    /// Lower edge (ADU) of histogram bin `i`.
    pub fn bin_start(&self, i: usize) -> f64 {
        i as f64 * (self.adc_max as f64 + 1.0) / HIST_BINS as f64
    }
}

/// Analyse an image. `bits_per_pixel` is the camera's ADC depth if known.
pub fn analyze(img: &Image, bits_per_pixel: Option<u32>) -> Analysis {
    let (names, full) = full_histograms(img);

    let mut occupied_or = 0u32;
    let mut distinct = 0usize;
    let mut max_val = 0u16;
    let mut combined = vec![0u64; FULL];
    for h in &full {
        for (v, &n) in h.iter().enumerate() {
            combined[v] += n;
        }
    }
    for (v, &n) in combined.iter().enumerate() {
        if n > 0 {
            distinct += 1;
            occupied_or |= v as u32;
            max_val = v as u16;
        }
    }
    // Data shifted up from a lower bit depth only uses multiples of 2^k.
    let step = if distinct >= 64 && occupied_or != 0 {
        1u16 << occupied_or.trailing_zeros().min(8)
    } else {
        1
    };
    let adc_max = match bits_per_pixel {
        Some(b) if b < 16 && max_val <= ((1u32 << b) - 1) as u16 && step == 1 => {
            ((1u32 << b) - 1) as u16
        }
        _ => (FULL as u32 - step as u32) as u16,
    };

    let channels = names
        .iter()
        .zip(&full)
        .map(|(name, h)| channel_stats(name, h, adc_max))
        .collect();
    let histograms = full
        .iter()
        .map(|h| {
            let mut bins = vec![0u32; HIST_BINS];
            let scale = HIST_BINS as f64 / (adc_max as f64 + 1.0);
            for (v, &n) in h.iter().enumerate() {
                if n > 0 {
                    let b = ((v as f64 * scale) as usize).min(HIST_BINS - 1);
                    bins[b] = bins[b].saturating_add(n.min(u32::MAX as u64) as u32);
                }
            }
            bins
        })
        .collect();
    Analysis {
        adc_max,
        step,
        channels,
        histograms,
    }
}

fn full_histograms(img: &Image) -> (Vec<&'static str>, Vec<Vec<u64>>) {
    if let Some(cfa) = img.cfa {
        let mut h = vec![vec![0u64; FULL]; 3];
        for y in 0..img.height {
            let row = &img.data[y * img.width..(y + 1) * img.width];
            for (x, &v) in row.iter().enumerate() {
                h[cfa.color_at(x, y)][v as usize] += 1;
            }
        }
        (vec!["R", "G", "B"], h)
    } else {
        let names = if img.channels == 3 {
            vec!["R", "G", "B"]
        } else {
            vec!["L"]
        };
        let h = (0..img.channels)
            .map(|c| {
                let mut h = vec![0u64; FULL];
                for &v in img.plane(c) {
                    h[v as usize] += 1;
                }
                h
            })
            .collect();
        (names, h)
    }
}

fn channel_stats(name: &str, h: &[u64], adc_max: u16) -> ChannelStats {
    let pixels: u64 = h.iter().sum();
    let first = h.iter().position(|&n| n > 0).unwrap_or(0);
    let last = h.iter().rposition(|&n| n > 0).unwrap_or(0);
    let sum: f64 = h.iter().enumerate().map(|(v, &n)| v as f64 * n as f64).sum();
    let median = percentile(h, pixels, 0.5);
    let mut dev = vec![0u64; FULL];
    for (v, &n) in h.iter().enumerate().filter(|(_, n)| **n > 0) {
        dev[(v as f64 - median).abs().round() as usize] += n;
    }
    ChannelStats {
        name: name.to_owned(),
        pixels,
        min: first as u16,
        max: last as u16,
        mean: if pixels > 0 { sum / pixels as f64 } else { 0.0 },
        median,
        mad: percentile(&dev, pixels, 0.5),
        p001: percentile(h, pixels, 0.001) as u16,
        zero: h[0],
        saturated: h[adc_max as usize..].iter().sum(),
    }
}

/// Smallest value whose cumulative count reaches fraction `q` of `total`.
fn percentile(h: &[u64], total: u64, q: f64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let target = ((total as f64 * q).ceil() as u64).max(1);
    let mut acc = 0;
    for (v, &n) in h.iter().enumerate() {
        acc += n;
        if acc >= target {
            return v as f64;
        }
    }
    (h.len() - 1) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Cfa;

    fn mono(data: Vec<u16>) -> Image {
        Image {
            width: data.len(),
            height: 1,
            channels: 1,
            data,
            cfa: None,
        }
    }

    #[test]
    fn basic_stats_and_clipping() {
        // 1000 pixels spread over 100..=1099, plus 5 black and 3 saturated.
        let mut data: Vec<u16> = (100..1100).collect();
        data.extend([0; 5]);
        data.extend([65535; 3]);
        let a = analyze(&mono(data), Some(16));
        assert_eq!(a.adc_max, 65535);
        assert_eq!(a.step, 1);
        let s = &a.channels[0];
        assert_eq!(s.name, "L");
        assert_eq!(s.pixels, 1008);
        assert_eq!((s.min, s.max), (0, 65535));
        assert_eq!(s.zero, 5);
        assert_eq!(s.saturated, 3);
        assert!((s.median - 599.0).abs() <= 4.0, "{}", s.median);
        assert!((s.mad - 250.0).abs() <= 5.0, "{}", s.mad);
        assert_eq!(a.histograms[0].iter().map(|&n| n as u64).sum::<u64>(), 1008);
        assert_eq!(a.histograms[0][0], 5);
        assert_eq!(a.histograms[0][HIST_BINS - 1], 3);
    }

    #[test]
    fn detects_scaled_12_bit_data() {
        // 12-bit values shifted left by 4, saturating at 4095 << 4.
        let mut data: Vec<u16> = (10..2000).map(|v| v << 4).collect();
        data.push(4095 << 4);
        let a = analyze(&mono(data), Some(12));
        assert_eq!(a.step, 16);
        assert_eq!(a.adc_max, 65520);
        assert_eq!(a.channels[0].saturated, 1);
    }

    #[test]
    fn unscaled_12_bit_data() {
        let mut data: Vec<u16> = (1..4000).collect();
        data.push(4095);
        let a = analyze(&mono(data), Some(12));
        assert_eq!(a.adc_max, 4095);
        assert_eq!(a.channels[0].saturated, 1);
        assert_eq!(a.channels[0].zero, 0);
    }

    #[test]
    fn bayer_channels() {
        // RGGB 2x2 tiles: R=100, G=200, B=300.
        let (w, h) = (4, 4);
        let cfa = Cfa::parse("RGGB", 0, 0).unwrap();
        let data = (0..w * h)
            .map(|i| [100, 200, 300][cfa.color_at(i % w, i / w)])
            .collect();
        let img = Image {
            width: w,
            height: h,
            channels: 1,
            data,
            cfa: Some(cfa),
        };
        let a = analyze(&img, None);
        let medians: Vec<f64> = a.channels.iter().map(|c| c.median).collect();
        assert_eq!(medians, [100.0, 200.0, 300.0]);
        assert_eq!(a.channels[1].pixels, 8);
    }

    #[test]
    fn headroom() {
        let s = ChannelStats {
            name: "L".into(),
            pixels: 1,
            min: 0,
            max: 0,
            mean: 0.0,
            median: 0.0,
            mad: 10.0,
            p001: 148,
            zero: 0,
            saturated: 0,
        };
        assert!((s.headroom_sigma().unwrap() - 148.0 / 14.826).abs() < 1e-9);
    }
}
