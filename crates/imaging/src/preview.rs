//! Screen previews: debayer, downsample, auto-stretch, JPEG.

use crate::image::Image;
use crate::stats::{Analysis, ChannelStats};

/// Screen transfer function (PixInsight-style auto stretch), normalised to [0, 1].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stf {
    /// Shadows clipping point.
    pub c0: f64,
    /// Midtones balance.
    pub m: f64,
}

const SHADOWS_CLIP_SIGMA: f64 = -2.8;
const TARGET_BACKGROUND: f64 = 0.25;

/// Midtones transfer function.
pub fn mtf(m: f64, x: f64) -> f64 {
    if x <= 0.0 {
        0.0
    } else if x >= 1.0 {
        1.0
    } else {
        ((m - 1.0) * x) / ((2.0 * m - 1.0) * x - m)
    }
}

impl Stf {
    pub fn auto(stats: &ChannelStats, adc_max: u16) -> Stf {
        let full = adc_max.max(1) as f64;
        let median = stats.median / full;
        let sigma = stats.sigma() / full;
        let c0 = if sigma > 0.0 {
            (median + SHADOWS_CLIP_SIGMA * sigma).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let bg = if c0 < 1.0 { (median - c0) / (1.0 - c0) } else { 0.0 };
        let m = if bg > 0.0 { mtf(TARGET_BACKGROUND, bg) } else { 0.5 };
        Stf { c0, m }
    }

    /// Lookup table mapping every 16-bit value to an 8-bit display value.
    pub fn lut(&self, adc_max: u16) -> Vec<u8> {
        let full = adc_max.max(1) as f64;
        let span = (1.0 - self.c0).max(1e-9);
        (0..=u16::MAX)
            .map(|v| {
                let x = ((v as f64 / full - self.c0) / span).clamp(0.0, 1.0);
                (mtf(self.m, x) * 255.0).round() as u8
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    pub width: usize,
    pub height: usize,
    /// 1 (grey) or 3 (RGB, interleaved).
    pub channels: usize,
    pub pixels: Vec<u8>,
}

/// Build a stretched preview no larger than `max_dim` on its long side.
pub fn make_preview(img: &Image, analysis: &Analysis, max_dim: usize) -> Preview {
    // 1. Linear planes: CFA data is debayered by 2x2 superpixels.
    let (w, h, planes) = match img.cfa {
        Some(cfa) => superpixel(img, cfa),
        None => (
            img.width,
            img.height,
            (0..img.channels).map(|c| img.plane(c).to_vec()).collect(),
        ),
    };
    // 2. Box-filter downsample.
    let factor = w.max(h).div_ceil(max_dim.max(1)).max(1);
    let (pw, ph) = (w / factor, h / factor);
    let (pw, ph) = (pw.max(1), ph.max(1));
    // 3. Stretch each channel with its own STF (also neutralises the background).
    let luts: Vec<Vec<u8>> = (0..planes.len())
        .map(|c| {
            let stats = analysis.channels.get(c).unwrap_or(&analysis.channels[0]);
            Stf::auto(stats, analysis.adc_max).lut(analysis.adc_max)
        })
        .collect();
    let n = planes.len();
    let mut pixels = vec![0u8; pw * ph * n];
    for (c, plane) in planes.iter().enumerate() {
        for py in 0..ph {
            for px in 0..pw {
                let mut sum = 0u64;
                let mut count = 0u64;
                for y in py * factor..((py + 1) * factor).min(h) {
                    let row = &plane[y * w..(y + 1) * w];
                    for &v in &row[px * factor..((px + 1) * factor).min(w)] {
                        sum += v as u64;
                        count += 1;
                    }
                }
                let avg = (sum / count.max(1)) as usize;
                pixels[(py * pw + px) * n + c] = luts[c][avg];
            }
        }
    }
    Preview {
        width: pw,
        height: ph,
        channels: n,
        pixels,
    }
}

fn superpixel(img: &Image, cfa: crate::image::Cfa) -> (usize, usize, Vec<Vec<u16>>) {
    let (w, h) = (img.width / 2, img.height / 2);
    let mut planes = vec![vec![0u16; w * h]; 3];
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0u32; 3];
            let mut cnt = [0u32; 3];
            for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let (sx, sy) = (2 * x + dx, 2 * y + dy);
                let c = cfa.color_at(sx, sy);
                acc[c] += img.data[sy * img.width + sx] as u32;
                cnt[c] += 1;
            }
            for c in 0..3 {
                planes[c][y * w + x] = (acc[c] / cnt[c].max(1)) as u16;
            }
        }
    }
    (w, h, planes)
}

pub fn encode_jpeg(p: &Preview, quality: u8) -> Vec<u8> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    let color = if p.channels == 3 {
        image::ExtendedColorType::Rgb8
    } else {
        image::ExtendedColorType::L8
    };
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
        .write_image(&p.pixels, p.width as u32, p.height as u32, color)
        .expect("in-memory JPEG encoding cannot fail for valid dimensions");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Cfa;
    use crate::stats::analyze;

    #[test]
    fn mtf_properties() {
        assert_eq!(mtf(0.3, 0.0), 0.0);
        assert_eq!(mtf(0.3, 1.0), 1.0);
        assert!((mtf(0.3, 0.3) - 0.5).abs() < 1e-12);
        assert!((mtf(0.5, 0.42) - 0.42).abs() < 1e-12);
    }

    #[test]
    fn auto_stretch_puts_background_at_target() {
        // Faint noisy background around 1000 ADU.
        let data: Vec<u16> = (0..10_000u32).map(|i| 990 + (i * 7919 % 21) as u16).collect();
        let img = Image {
            width: 100,
            height: 100,
            channels: 1,
            data,
            cfa: None,
        };
        let a = analyze(&img, None);
        let stf = Stf::auto(&a.channels[0], a.adc_max);
        let lut = stf.lut(a.adc_max);
        let bg = lut[a.channels[0].median as usize] as f64 / 255.0;
        assert!((bg - TARGET_BACKGROUND).abs() < 0.02, "background at {bg}");
    }

    #[test]
    fn preview_size_and_colour() {
        let (w, h) = (400, 200);
        let cfa = Cfa::parse("RGGB", 0, 0).unwrap();
        let data = (0..w * h)
            .map(|i| [1000, 2000, 3000][cfa.color_at(i % w, i / w)] + (i % 13) as u16)
            .collect();
        let img = Image {
            width: w,
            height: h,
            channels: 1,
            data,
            cfa: Some(cfa),
        };
        let a = analyze(&img, None);
        let p = make_preview(&img, &a, 100);
        assert_eq!((p.width, p.height, p.channels), (100, 50, 3));
        assert_eq!(p.pixels.len(), 100 * 50 * 3);
        let jpeg = encode_jpeg(&p, 85);
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    }
}
