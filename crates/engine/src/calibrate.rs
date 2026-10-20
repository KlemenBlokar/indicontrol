//! Applying masters to a light frame: (light − dark) / normalised flat,
//! where the flat first has its bias or dark flat taken off. Without
//! darks the bias stands in for the dark. Blocking: runs with the frame
//! processing.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use imaging::{Card, Fits, Header, Image};

/// Masters kept in memory (a 26 MP master takes 100 MB).
const KEEP_MASTERS: usize = 4;
const KEEP_FLATS: usize = 2;
/// Flat pixels darker than this fraction of the median are left
/// uncorrected rather than blown up.
const MIN_FLAT: f32 = 0.05;

const PREVIEW_PEDESTAL: f32 = 100.0;

/// The masters (absolute paths) a frame gets, and what to save.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// Master dark, or bias when there are no darks.
    pub dark: Option<PathBuf>,
    pub flat: Option<PathBuf>,
    /// Bias or dark flat subtracted from the flat.
    pub flat_cal: Option<PathBuf>,
    pub save_raw: bool,
    pub save_calibrated: bool,
    /// "dark, flat": what the masters are, for the frame info.
    pub label: String,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.dark.is_none() && self.flat.is_none()
    }
}

struct Master {
    width: usize,
    height: usize,
    data: Vec<f32>,
}

/// A file as it was when read: path and modification time.
type Stamp = (PathBuf, Option<SystemTime>);
/// A flat and the bias or dark flat taken off it.
type FlatKey = (Stamp, Option<Stamp>);

/// Recently used masters and prepared flats, shared by the frame threads.
#[derive(Default)]
pub struct Cache {
    masters: Mutex<Vec<(Stamp, Arc<Master>)>>,
    flats: Mutex<Vec<(FlatKey, Arc<Master>)>>,
}

fn stamp(path: &Path) -> Stamp {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    (path.to_path_buf(), modified)
}

/// Most recently used last; the oldest dropped beyond `keep`.
fn cached<K: PartialEq + Clone>(
    list: &Mutex<Vec<(K, Arc<Master>)>>,
    key: &K,
    keep: usize,
    make: impl FnOnce() -> Result<Master, String>,
) -> Result<Arc<Master>, String> {
    {
        let mut l = list.lock().expect("cache lock");
        if let Some(i) = l.iter().position(|(k, _)| k == key) {
            let entry = l.remove(i);
            let m = entry.1.clone();
            l.push(entry);
            return Ok(m);
        }
    }
    let m = Arc::new(make()?);
    let mut l = list.lock().expect("cache lock");
    l.push((key.clone(), m.clone()));
    let excess = l.len().saturating_sub(keep);
    l.drain(..excess);
    Ok(m)
}

impl Cache {
    fn master(&self, path: &Path) -> Result<Arc<Master>, String> {
        cached(&self.masters, &stamp(path), KEEP_MASTERS, || {
            let (w, h, data) = crate::stack::read_f32(path)?;
            Ok(Master { width: w, height: h, data })
        })
    }

    /// The flat less its bias or dark flat, divided by its median.
    fn flat(&self, flat: &Path, cal: Option<&Path>) -> Result<Arc<Master>, String> {
        let key = (stamp(flat), cal.map(stamp));
        cached(&self.flats, &key, KEEP_FLATS, || {
            let f = self.master(flat)?;
            let mut data = f.data.clone();
            if let Some(cal) = cal {
                let c = self.master(cal)?;
                same_size(&c, f.width, f.height, cal)?;
                data.iter_mut().zip(&c.data).for_each(|(v, c)| *v -= c);
            }
            let level = median(&data);
            if level.is_nan() || level <= 0.0 {
                return Err(format!("{} has no signal left after calibration", flat.display()));
            }
            data.iter_mut().for_each(|v| *v /= level);
            Ok(Master { width: f.width, height: f.height, data })
        })
    }
}

fn same_size(m: &Master, w: usize, h: usize, path: &Path) -> Result<(), String> {
    if (m.width, m.height) == (w, h) {
        Ok(())
    } else {
        Err(format!("{} is {}×{}, the frame {w}×{h}", path.display(), m.width, m.height))
    }
}

/// Median of the values, from a sample of about 100 000.
fn median(data: &[f32]) -> f32 {
    let step = (data.len() / 100_000).max(1);
    let mut sample: Vec<f32> = data.iter().step_by(step).copied().filter(|v| v.is_finite()).collect();
    if sample.is_empty() {
        return 0.0;
    }
    let mid = sample.len() / 2;
    *sample.select_nth_unstable_by(mid, f32::total_cmp).1
}

/// `image` (one plane, ADU) with the plan's masters applied.
pub fn apply(plan: &Plan, cache: &Cache, image: &Image) -> Result<Vec<f32>, String> {
    if image.channels != 1 {
        return Err("only single-plane (mono or raw colour) frames can be calibrated".into());
    }
    let (w, h) = (image.width, image.height);
    let mut out: Vec<f32> = image.data.iter().map(|&v| v as f32).collect();
    if let Some(path) = &plan.dark {
        let d = cache.master(path)?;
        same_size(&d, w, h, path)?;
        out.iter_mut().zip(&d.data).for_each(|(v, d)| *v -= d);
    }
    if let Some(path) = &plan.flat {
        let f = cache.flat(path, plan.flat_cal.as_deref())?;
        same_size(&f, w, h, path)?;
        for (v, &f) in out.iter_mut().zip(&f.data) {
            if f >= MIN_FLAT {
                *v /= f;
            }
        }
    }
    Ok(out)
}

/// The values in 16 bits for the preview and measurements, lifted by a
/// pedestal so the noise of a dark-subtracted background is not clipped.
pub fn to_image(values: &[f32], like: &Image) -> Image {
    Image {
        width: like.width,
        height: like.height,
        channels: like.channels,
        data: values.iter().map(|v| (v + PREVIEW_PEDESTAL).round().clamp(0.0, 65535.0) as u16).collect(),
        cfa: like.cfa,
    }
}

/// The calibrated frame as 32-bit float FITS, with `header` (the raw
/// frame's) noting what was applied.
pub fn to_fits(values: &[f32], header: &Header, plan: &Plan) -> Fits {
    let mut header = header.clone();
    for key in ["BZERO", "BSCALE", "CHECKSUM", "DATASUM"] {
        header.remove(key);
    }
    header.set(Card::new("BITPIX", -32i64, "32-bit float"));
    // MaxIm DL style: which of bias, dark, flat were applied.
    let mut stat = String::new();
    if plan.dark.is_some() {
        stat.push(if plan.label.starts_with("bias") { 'B' } else { 'D' });
    }
    if plan.flat.is_some() {
        stat.push('F');
    }
    header.set(Card::new("CALSTAT", stat.as_str(), "Calibration applied"));
    for (what, path) in [("dark", &plan.dark), ("flat", &plan.flat), ("flat calibration", &plan.flat_cal)] {
        if let Some(p) = path {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            header.cards.push(Card::new("HISTORY", format!("Calibrated: {what} {name}").as_str(), ""));
        }
    }
    let mut data = Vec::with_capacity(values.len() * 4);
    for v in values {
        data.extend_from_slice(&v.to_be_bytes());
    }
    Fits { header, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, w: usize, values: &[f32]) -> PathBuf {
        let mut header = Header::default();
        header.set(Card::new("SIMPLE", true, ""));
        header.set(Card::new("BITPIX", -32i64, ""));
        header.set(Card::new("NAXIS", 2i64, ""));
        header.set(Card::new("NAXIS1", w as i64, ""));
        header.set(Card::new("NAXIS2", (values.len() / w) as i64, ""));
        let fits = to_fits(values, &header, &Plan::default());
        let path = dir.join(name);
        std::fs::write(&path, fits.to_bytes()).unwrap();
        path
    }

    #[test]
    fn subtracts_the_dark_and_divides_by_the_flat() {
        let dir = std::env::temp_dir().join(format!("indicontrol-calibrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A vignetted flat (100 % in the middle, 50 % at the edge) on a
        // 100 ADU bias; a dark of 20 ADU with one hot pixel.
        let flat = write(&dir, "flat.fits", 4, &[1100.0, 2100.0, 2100.0, 1100.0, 1100.0, 2100.0, 2100.0, 1100.0]);
        let bias = write(&dir, "bias.fits", 4, &[100.0; 8]);
        let dark = write(&dir, "dark.fits", 4, &[20.0, 20.0, 520.0, 20.0, 20.0, 20.0, 20.0, 20.0]);
        let plan = Plan {
            dark: Some(dark),
            flat: Some(flat),
            flat_cal: Some(bias),
            ..Default::default()
        };
        // Even sky of 400 ADU, vignetted like the flat, plus the dark.
        let light = Image {
            width: 4,
            height: 2,
            channels: 1,
            data: vec![220, 420, 920, 220, 220, 420, 420, 220],
            cfa: None,
        };
        let cache = Cache::default();
        let out = apply(&plan, &cache, &light).unwrap();
        // The flat's median is 2000 after the bias: the edges are 0.5.
        for v in &out {
            assert!((v - 400.0).abs() < 1e-3, "{out:?}");
        }
        // Cached: the same answer again.
        assert_eq!(apply(&plan, &cache, &light).unwrap(), out);

        let small = Image { width: 2, height: 4, ..light.clone() };
        assert!(apply(&plan, &cache, &small).unwrap_err().contains("4×2"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
