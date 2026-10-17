//! Decoded camera frame (16-bit, planar).

use serde::{Deserialize, Serialize};

use crate::fits::{Fits, FitsError};

/// Colour filter array: `pattern[y % 2][x % 2]` is 0 = R, 1 = G, 2 = B.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cfa {
    pub pattern: [[u8; 2]; 2],
}

impl Cfa {
    /// From FITS `BAYERPAT` (e.g. "RGGB") and optional `XBAYROFF`/`YBAYROFF`.
    pub fn parse(pattern: &str, x_offset: i64, y_offset: i64) -> Option<Cfa> {
        let colors: Vec<u8> = pattern
            .trim()
            .chars()
            .map(|c| match c.to_ascii_uppercase() {
                'R' => Some(0),
                'G' => Some(1),
                'B' => Some(2),
                _ => None,
            })
            .collect::<Option<_>>()?;
        if colors.len() != 4 {
            return None;
        }
        let base = [[colors[0], colors[1]], [colors[2], colors[3]]];
        let (dx, dy) = (x_offset.rem_euclid(2) as usize, y_offset.rem_euclid(2) as usize);
        let mut pattern = [[0; 2]; 2];
        for (y, row) in pattern.iter_mut().enumerate() {
            for (x, c) in row.iter_mut().enumerate() {
                *c = base[(y + dy) % 2][(x + dx) % 2];
            }
        }
        Some(Cfa { pattern })
    }

    pub fn color_at(&self, x: usize, y: usize) -> usize {
        self.pattern[y & 1][x & 1] as usize
    }
}

#[derive(Debug, Clone)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// 1 (mono or CFA) or 3 (RGB planes).
    pub channels: usize,
    /// Planar, row-major, top row first as stored in the file.
    pub data: Vec<u16>,
    pub cfa: Option<Cfa>,
}

impl Image {
    pub fn plane(&self, c: usize) -> &[u16] {
        let n = self.width * self.height;
        &self.data[c * n..(c + 1) * n]
    }

    pub fn from_fits(fits: &Fits) -> Result<Image, FitsError> {
        let (width, height, depth) = fits.dims()?;
        if depth != 1 && depth != 3 {
            return Err(FitsError::Unsupported(format!("{depth} image planes")));
        }
        let bitpix = fits.bitpix()?;
        let bzero = fits.header.get_f64("BZERO").unwrap_or(0.0);
        let bscale = fits.header.get_f64("BSCALE").unwrap_or(1.0);
        let raw = &fits.data;
        let clamp = |v: f64| v.round().clamp(0.0, 65535.0) as u16;
        let data: Vec<u16> = match bitpix {
            8 => raw.iter().map(|&b| clamp(b as f64 * bscale + bzero)).collect(),
            16 if bscale == 1.0 && bzero == 32768.0 => raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&b| (i16::from_be_bytes(b) as i32 + 32768) as u16)
                .collect(),
            16 => raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&b| clamp(i16::from_be_bytes(b) as f64 * bscale + bzero))
                .collect(),
            32 => raw
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&b| clamp(i32::from_be_bytes(b) as f64 * bscale + bzero))
                .collect(),
            -32 | -64 => {
                let values: Vec<f64> = if bitpix == -32 {
                    raw.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|&b| f32::from_be_bytes(b) as f64)
                        .collect()
                } else {
                    raw.as_chunks::<8>().0.iter().map(|&b| f64::from_be_bytes(b))
                        .collect()
                };
                let values: Vec<f64> = values.iter().map(|v| v * bscale + bzero).collect();
                // Normalised [0, 1] data is common for float images.
                let max = values.iter().copied().filter(|v| v.is_finite()).fold(0.0, f64::max);
                let k = if max <= 1.0 { 65535.0 } else { 1.0 };
                values
                    .iter()
                    .map(|v| if v.is_finite() { clamp(v * k) } else { 0 })
                    .collect()
            }
            other => return Err(FitsError::Unsupported(format!("BITPIX = {other}"))),
        };
        let cfa = if depth == 1 {
            fits.header.get_str("BAYERPAT").and_then(|p| {
                Cfa::parse(
                    p,
                    fits.header.get_i64("XBAYROFF").unwrap_or(0),
                    fits.header.get_i64("YBAYROFF").unwrap_or(0),
                )
            })
        } else {
            None
        };
        Ok(Image {
            width,
            height,
            channels: depth,
            data,
            cfa,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fits::{Card, Header};

    #[test]
    fn cfa_offsets() {
        let rggb = Cfa::parse("RGGB", 0, 0).unwrap();
        assert_eq!((rggb.color_at(0, 0), rggb.color_at(1, 0), rggb.color_at(1, 1)), (0, 1, 2));
        // Shifting RGGB by one column gives GRBG.
        assert_eq!(Cfa::parse("RGGB", 1, 0), Cfa::parse("GRBG", 0, 0));
        assert_eq!(Cfa::parse("RGGB", 0, 1), Cfa::parse("GBRG", 0, 0));
        assert_eq!(Cfa::parse("RGGB", 1, 1), Cfa::parse("BGGR", 0, 0));
        assert!(Cfa::parse("RGB", 0, 0).is_none());
    }

    #[test]
    fn decodes_bitpix_variants() {
        let pixels = [0u16, 1, 1000, 65535];
        let fits = Fits::parse(&crate::fits::sample_fits(2, 2, &pixels)).unwrap();
        let img = Image::from_fits(&fits).unwrap();
        assert_eq!(img.data, pixels);
        assert!(img.cfa.is_none());

        let mut h = Header::default();
        for c in [
            Card::new("SIMPLE", true, ""),
            Card::new("BITPIX", -32i64, ""),
            Card::new("NAXIS", 2i64, ""),
            Card::new("NAXIS1", 2i64, ""),
            Card::new("NAXIS2", 1i64, ""),
            Card::new("BAYERPAT", "RGGB", ""),
        ] {
            h.set(c);
        }
        let data = [0.5f32, 1.0].iter().flat_map(|v| v.to_be_bytes()).collect();
        let img = Image::from_fits(&Fits { header: h, data }).unwrap();
        assert_eq!(img.data, [32768, 65535]);
        assert_eq!(img.cfa, Cfa::parse("RGGB", 0, 0));
    }
}
