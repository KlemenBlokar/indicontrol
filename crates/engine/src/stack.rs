//! Averaging calibration frames into a master: a winsorized κσ-clipped mean
//! per pixel,
//! read from the files a band of rows at a time so memory stays small.
//! Blocking: call from `spawn_blocking`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use imaging::{Card, Header};
use protocol::MasterKind;

/// Values further than this many σ from the median are left out.
const KAPPA: f32 = 3.0;
/// Rows read from every file at once.
const BAND_ROWS: usize = 64;

pub struct Stacked {
    pub path: PathBuf,
    pub frames: usize,
    /// Fraction of values rejected by the clipping.
    pub rejected: f64,
}

struct Input {
    file: File,
    data_start: u64,
    bitpix: i64,
    bscale: f32,
    bzero: f32,
}

impl Input {
    fn open(path: &Path) -> Result<(Input, Header), String> {
        let name = path.display();
        let (header, data_start) = crate::masters::read_header(path).map_err(|e| format!("{name}: {e}"))?;
        let bitpix = header.get_i64("BITPIX").unwrap_or(0);
        if ![8, 16, 32, -32, -64].contains(&bitpix) {
            return Err(format!("{name}: BITPIX {bitpix} is not supported"));
        }
        if header.get_i64("NAXIS") != Some(2) {
            return Err(format!("{name}: only single-plane (mono or raw colour) frames can be stacked"));
        }
        let file = File::open(path).map_err(|e| format!("{name}: {e}"))?;
        Ok((
            Input {
                file,
                data_start: data_start as u64,
                bitpix,
                bscale: header.get_f64("BSCALE").unwrap_or(1.0) as f32,
                bzero: header.get_f64("BZERO").unwrap_or(0.0) as f32,
            },
            header,
        ))
    }

    /// `out.len()` values starting at pixel `first`.
    fn read(&mut self, first: usize, out: &mut [f32], buf: &mut Vec<u8>) -> std::io::Result<()> {
        let size = (self.bitpix.unsigned_abs() / 8) as usize;
        self.file.seek(SeekFrom::Start(self.data_start + (first * size) as u64))?;
        buf.resize(out.len() * size, 0);
        self.file.read_exact(buf)?;
        let (s, z) = (self.bscale, self.bzero);
        let put = |v: f32| v * s + z;
        match self.bitpix {
            8 => buf.iter().zip(out.iter_mut()).for_each(|(b, o)| *o = put(*b as f32)),
            16 => {
                for (c, o) in buf.as_chunks::<2>().0.iter().zip(out.iter_mut()) {
                    *o = put(i16::from_be_bytes(*c) as f32);
                }
            }
            32 => {
                for (c, o) in buf.as_chunks::<4>().0.iter().zip(out.iter_mut()) {
                    *o = put(i32::from_be_bytes(*c) as f32);
                }
            }
            -32 => {
                for (c, o) in buf.as_chunks::<4>().0.iter().zip(out.iter_mut()) {
                    *o = put(f32::from_be_bytes(*c));
                }
            }
            _ => {
                for (c, o) in buf.as_chunks::<8>().0.iter().zip(out.iter_mut()) {
                    *o = put(f64::from_be_bytes(*c) as f32);
                }
            }
        }
        Ok(())
    }
}

/// A whole single-plane file as values: (width, height, values).
pub fn read_f32(path: &Path) -> Result<(usize, usize, Vec<f32>), String> {
    let (mut input, header) = Input::open(path)?;
    let w = header.get_i64("NAXIS1").unwrap_or(0).max(0) as usize;
    let h = header.get_i64("NAXIS2").unwrap_or(0).max(0) as usize;
    let mut out = vec![0.0; w * h];
    input.read(0, &mut out, &mut Vec::new()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((w, h, out))
}

/// Average `frames` into a `kind` master in `folder` (32-bit float, in the
/// frames' ADU, header from the first frame), named after its settings.
pub fn stack(frames: &[PathBuf], kind: MasterKind, folder: &Path) -> Result<Stacked, String> {
    if frames.is_empty() {
        return Err("no frames to stack".into());
    }
    let mut inputs = Vec::with_capacity(frames.len());
    let mut first_header = None;
    let mut size = None;
    for path in frames {
        let (input, header) = Input::open(path)?;
        let dims = (header.get_i64("NAXIS1").unwrap_or(0), header.get_i64("NAXIS2").unwrap_or(0));
        match size {
            None => size = Some(dims),
            Some(s) if s != dims => {
                return Err(format!("{} is {}×{}, the others {}×{}", path.display(), dims.0, dims.1, s.0, s.1));
            }
            _ => {}
        }
        first_header.get_or_insert(header);
        inputs.push(input);
    }
    let (w, h) = size.expect("at least one frame");
    let (w, h) = (w.max(0) as usize, h.max(0) as usize);

    let mut header = first_header.expect("at least one frame");
    for key in ["BZERO", "BSCALE", "CHECKSUM", "DATASUM"] {
        header.remove(key);
    }
    let n = frames.len() as i64;
    let label = kind.label();
    let capital = format!("{}{}", label[..1].to_uppercase(), &label[1..]);
    header.set(Card::new("BITPIX", -32i64, "32-bit float"));
    header.set(Card::new("IMAGETYP", format!("Master {capital}").as_str(), "Type of image"));
    header.set(Card::new("FRAME", format!("Master {capital}").as_str(), ""));
    header.set(Card::new("STACKCNT", n, "Frames averaged"));
    header.set(Card::new("NCOMBINE", n, "Frames averaged"));
    header.set(Card::new(
        "HISTORY",
        format!("{n} frames, winsorized sigma-clipped mean (kappa {KAPPA})").as_str(),
        "",
    ));

    let out = folder.join(file_name(kind, &header, chrono::Local::now()));
    let out = out.as_path();
    let tmp = out.with_extension("fits.part");
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let write_err = |e: std::io::Error| format!("could not write {}: {e}", tmp.display());
    let mut file = std::io::BufWriter::new(File::create(&tmp).map_err(write_err)?);
    file.write_all(&header.to_bytes()).map_err(write_err)?;

    let band = BAND_ROWS * w;
    let mut values: Vec<Vec<f32>> = vec![vec![0.0; band]; inputs.len()];
    let mut buf = Vec::new();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut out_bytes = Vec::with_capacity(band * 4);
    let mut rejected = 0u64;
    let mut row = 0;
    while row < h {
        let rows = BAND_ROWS.min(h - row);
        let len = rows * w;
        for (input, v) in inputs.iter_mut().zip(&mut values) {
            input
                .read(row * w, &mut v[..len], &mut buf)
                .map_err(|e| format!("could not read a frame: {e}"))?;
        }
        // Each core takes a share of the band's pixels.
        out_bytes.resize(len * 4, 0);
        let share = len.div_ceil(threads);
        let values = &values;
        rejected += std::thread::scope(|scope| {
            let workers: Vec<_> = out_bytes
                .chunks_mut(share * 4)
                .enumerate()
                .map(|(t, out)| {
                    scope.spawn(move || {
                        let mut pixel = Vec::with_capacity(values.len());
                        let mut scratch = Vec::with_capacity(values.len());
                        let mut dropped = 0u64;
                        for (k, o) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                            let i = t * share + k;
                            pixel.clear();
                            pixel.extend(values.iter().map(|v| v[i]));
                            let (mean, d) = clipped_mean(&mut pixel, &mut scratch);
                            dropped += d as u64;
                            *o = mean.to_be_bytes();
                        }
                        dropped
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().expect("stacking thread")).sum::<u64>()
        });
        file.write_all(&out_bytes).map_err(write_err)?;
        row += rows;
    }
    let data = w * h * 4;
    let padding = data.div_ceil(2880) * 2880 - data;
    file.write_all(&vec![0; padding]).map_err(write_err)?;
    file.into_inner().map_err(|e| write_err(e.into_error()))?.sync_all().map_err(write_err)?;
    std::fs::rename(&tmp, out).map_err(|e| format!("could not write {}: {e}", out.display()))?;
    Ok(Stacked {
        path: out.to_owned(),
        frames: frames.len(),
        rejected: rejected as f64 / (w * h * frames.len()).max(1) as f64,
    })
}

/// "master_dark_120s_g100_o50_-10C_bin1_20261020-211300.fits": what it
/// matches, readable in a file list.
fn file_name(kind: MasterKind, h: &Header, now: chrono::DateTime<chrono::Local>) -> String {
    let num = |v: f64| {
        let r = (v * 1000.0).round() / 1000.0;
        r.to_string()
    };
    let mut parts = vec!["master".to_owned(), kind.label().to_owned()];
    if kind == MasterKind::Flat
        && let Some(f) = h.get_str("FILTER")
    {
        let f: String = f.trim().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        if !f.is_empty() {
            parts.push(f);
        }
    }
    if kind == MasterKind::Dark
        && let Some(e) = h.get_f64("EXPTIME").or_else(|| h.get_f64("EXPOSURE"))
    {
        parts.push(format!("{}s", num(e)));
    }
    if kind != MasterKind::Flat {
        if let Some(g) = h.get_f64("GAIN") {
            parts.push(format!("g{}", num(g)));
        }
        if let Some(o) = h.get_f64("OFFSET") {
            parts.push(format!("o{}", num(o)));
        }
        if let Some(t) = h.get_f64("CCD-TEMP") {
            parts.push(format!("{}C", t.round()));
        }
    }
    parts.push(format!("bin{}", h.get_i64("XBINNING").unwrap_or(1)));
    parts.push(now.format("%Y%m%d-%H%M%S").to_string());
    format!("{}.fits", parts.join("_"))
}

/// Mean of `v` without values more than κσ from the median, and how many
/// were left out. σ is winsorized (values beyond 1.5σ pulled in while it is
/// estimated), so one cosmic ray cannot inflate it, nor does a handful of
/// frames make it so small that ordinary noise gets thrown away.
fn clipped_mean(v: &mut [f32], winsorized: &mut Vec<f32>) -> (f32, usize) {
    let mean = |v: &[f32]| v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64;
    let std = |v: &[f32]| {
        let m = mean(v);
        (v.iter().map(|&x| (x as f64 - m).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt()
    };
    if v.len() < 3 {
        return (mean(v) as f32, 0);
    }
    v.sort_unstable_by(f32::total_cmp);
    let median = middle(v) as f64;
    let mut sigma = std(v);
    for _ in 0..10 {
        let (lo, hi) = (median - 1.5 * sigma, median + 1.5 * sigma);
        winsorized.clear();
        winsorized.extend(v.iter().map(|&x| (x as f64).clamp(lo, hi) as f32));
        let next = 1.134 * std(winsorized);
        let converged = (next - sigma).abs() <= 1e-4 * sigma;
        sigma = next;
        if converged {
            break;
        }
    }
    let limit = KAPPA as f64 * sigma;
    let kept: Vec<f32> = v.iter().copied().filter(|&x| (x as f64 - median).abs() <= limit).collect();
    if kept.is_empty() {
        return (median as f32, v.len());
    }
    (mean(&kept) as f32, v.len() - kept.len())
}

fn middle(sorted: &[f32]) -> f32 {
    let n = sorted.len();
    if n % 2 == 1 { sorted[n / 2] } else { (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use imaging::{Fits, Image};

    fn frame(path: &Path, values: &[u16], exposure: f64) {
        let mut h = Header::default();
        for c in [
            Card::new("SIMPLE", true, ""),
            Card::new("BITPIX", 16i64, ""),
            Card::new("NAXIS", 2i64, ""),
            Card::new("NAXIS1", values.len() as i64 / 2, ""),
            Card::new("NAXIS2", 2i64, ""),
            Card::new("BZERO", 32768i64, ""),
            Card::new("IMAGETYP", "Dark Frame", ""),
            Card::new("EXPTIME", exposure, ""),
        ] {
            h.set(c);
        }
        let data = values.iter().flat_map(|&v| ((v as i32 - 32768) as i16).to_be_bytes()).collect();
        std::fs::write(path, Fits { header: h, data }.to_bytes()).unwrap();
    }

    #[test]
    fn noise_is_kept_and_cosmic_rays_dropped() {
        // A fixed pseudo-random sequence: roughly normal noise, σ ≈ 10.
        let mut seed = 12345u64;
        let mut noise = || {
            (0..12)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (seed >> 40) as f32 / (1u64 << 24) as f32
                })
                .sum::<f32>()
                * 10.0
                - 60.0
        };
        let mut scratch = Vec::new();
        let (mut rejected, mut total) = (0, 0);
        for _ in 0..2000 {
            let mut v: Vec<f32> = (0..10).map(|_| 1000.0 + noise()).collect();
            rejected += clipped_mean(&mut v, &mut scratch).1;
            total += v.len();
            v[3] = 1100.0; // a 10σ hit
            let (mean, dropped) = clipped_mean(&mut v, &mut scratch);
            assert!(dropped >= 1 && (mean - 1000.0).abs() < 15.0, "{v:?}");
        }
        let rate = rejected as f64 / total as f64;
        assert!(rate < 0.015, "{:.2} % of plain noise rejected", rate * 100.0);
    }

    #[test]
    fn averages_and_drops_outliers() {
        let dir = std::env::temp_dir().join(format!("indicontrol-stack-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths = Vec::new();
        for i in 0..5u16 {
            let p = dir.join(format!("d{i}.fits"));
            // Pixel 1 has a cosmic ray in the last frame.
            let hot = if i == 4 { 60000 } else { 200 + i };
            frame(&p, &[100 + i, hot, 1000, 1000, 50000, 7], 120.0);
            paths.push(p);
        }
        let s = stack(&paths, MasterKind::Dark, &dir.join("masters")).unwrap();
        let out = s.path.clone();
        let name = out.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("master_dark_120s_bin1_"), "{name}");
        assert_eq!(s.frames, 5);
        assert!(s.rejected > 0.0);

        let fits = Fits::parse(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(fits.header.get_str("IMAGETYP"), Some("Master Dark"));
        assert_eq!(fits.header.get_i64("STACKCNT"), Some(5));
        assert_eq!(fits.header.get_f64("EXPTIME"), Some(120.0));
        assert_eq!(fits.header.get_i64("BZERO"), None);
        let px: Vec<f32> = fits.data.as_chunks::<4>().0.iter().map(|c| f32::from_be_bytes(*c)).collect();
        assert_eq!(px[0], 102.0);
        assert_eq!(px[1], 201.5, "the 60000 is left out");
        assert_eq!(&px[2..], &[1000.0, 1000.0, 50000.0, 7.0]);
        assert!(Image::from_fits(&fits).is_ok());

        // And the masters folder takes it.
        let found = crate::masters::scan(&dir.join("masters"));
        assert_eq!(found.masters.len(), 1, "{:?}", found.skipped);
        assert_eq!(found.masters[0].frames, Some(5));

        frame(&paths[0], &[1, 2], 120.0);
        assert!(stack(&paths, MasterKind::Dark, &dir).is_err(), "different size");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
