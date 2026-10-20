//! Turning a downloaded BLOB into a saved FITS file, statistics and a preview.
//! Runs on a blocking thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Duration, Local};
use imaging::{Fits, Image, Value, analyze, encode_jpeg, make_preview};
use protocol::{FrameInfo, ImagingSettings};

use crate::calibrate;
use crate::metadata::FrameContext;

const PREVIEW_MAX_DIM: usize = 1600;
const PREVIEW_QUALITY: u8 = 85;

pub struct ProcessInput {
    pub frame_id: u64,
    pub blob: Arc<Vec<u8>>,
    pub format: String,
    pub ctx: FrameContext,
    pub settings: ImagingSettings,
    pub save: bool,
    /// Aperture (px) for star size measurement.
    pub star_aperture: f64,
    /// Masters to apply (sequence lights).
    pub calibration: Option<(calibrate::Plan, Arc<calibrate::Cache>)>,
}

pub struct ProcessOutput {
    pub info: FrameInfo,
    pub jpeg: Vec<u8>,
    /// Why the masters could not be applied.
    pub warning: Option<String>,
}

pub fn process(input: ProcessInput) -> Result<ProcessOutput, String> {
    let ProcessInput {
        frame_id,
        blob,
        format,
        ctx,
        settings,
        save,
        star_aperture,
        calibration,
    } = input;
    if !format.starts_with(".fit") {
        return Err(format!(
            "unsupported image format '{format}' — set the camera to FITS output"
        ));
    }
    let mut fits = Fits::parse(&blob).map_err(|e| format!("bad FITS from camera: {e}"))?;
    for card in ctx.defaults.iter().cloned() {
        fits.header.set_default(card);
    }
    for card in ctx.cards.iter().cloned() {
        fits.header.set(card);
    }

    let image = Image::from_fits(&fits).map_err(|e| format!("cannot decode image: {e}"))?;
    let mut warning = None;
    let calibrated = calibration.as_ref().and_then(|(plan, cache)| match calibrate::apply(plan, cache, &image) {
        Ok(values) => Some((values, plan)),
        Err(e) => {
            warning = Some(format!("Frame not calibrated: {e}"));
            None
        }
    });
    // Preview and measurements show the calibrated frame.
    let shown = calibrated.as_ref().map(|(values, _)| calibrate::to_image(values, &image));
    let view = shown.as_ref().unwrap_or(&image);
    let bpp = ctx
        .bits_per_pixel
        .or_else(|| (fits.bitpix().ok() == Some(8)).then_some(8));
    let analysis = analyze(view, bpp);
    let stars = star_stats(view, star_aperture);
    let jpeg = encode_jpeg(&make_preview(view, &analysis, PREVIEW_MAX_DIM), PREVIEW_QUALITY);

    // The raw frame is kept unless only the calibrated one is wanted.
    let save_raw = save && calibrated.as_ref().is_none_or(|(_, plan)| plan.save_raw);
    let save_calibrated = save && calibrated.as_ref().is_some_and(|(_, plan)| plan.save_calibrated);
    let (mut path, mut calibrated_path) = (None, None);
    if save_raw || save_calibrated {
        let raw = free_path(&settings, &ctx, |p| !p.exists() && !calibrated_file(p).exists());
        if save_raw {
            write_atomic(&raw, &fits.to_bytes()).map_err(|e| format!("could not write {}: {e}", raw.display()))?;
            path = Some(raw.display().to_string());
        }
        if let (true, Some((values, plan))) = (save_calibrated, &calibrated) {
            let out = calibrated_file(&raw);
            let bytes = calibrate::to_fits(values, &fits.header, plan).to_bytes();
            write_atomic(&out, &bytes).map_err(|e| format!("could not write {}: {e}", out.display()))?;
            calibrated_path = Some(out.display().to_string());
            path.get_or_insert_with(|| out.display().to_string());
        }
    }

    let header = fits
        .header
        .cards
        .iter()
        .filter(|c| c.value != Value::None)
        .map(|c| (c.key.clone(), c.value.display()))
        .collect();
    Ok(ProcessOutput {
        info: FrameInfo {
            id: frame_id,
            camera: ctx.camera.clone(),
            frame_type: ctx.frame_type,
            exposure: ctx.exposure,
            filter: ctx.filter.clone(),
            object: ctx.object.clone(),
            width: image.width,
            height: image.height,
            path,
            calibrated_path,
            calibration: calibrated.map(|(_, plan)| plan.label.clone()),
            analysis,
            stars,
            header,
        },
        jpeg,
        warning,
    })
}

/// Star count and HFR in original pixels. Colour (CFA) frames are measured
/// on 2×2 superpixels so the Bayer pattern does not break up the stars.
fn star_stats(img: &Image, aperture: f64) -> Option<protocol::StarStats> {
    const MAX_STARS: usize = 200;
    match img.cfa {
        None => guiding::star_stats(img, aperture, MAX_STARS),
        Some(_) => {
            let (w, h) = (img.width / 2, img.height / 2);
            let data = (0..w * h)
                .map(|i| {
                    let (x, y) = (2 * (i % w), 2 * (i / w));
                    let px = |dx: usize, dy: usize| img.data[(y + dy) * img.width + x + dx] as u32;
                    ((px(0, 0) + px(1, 0) + px(0, 1) + px(1, 1)) / 4) as u16
                })
                .collect();
            let mono = Image {
                width: w,
                height: h,
                channels: 1,
                data,
                cfa: None,
            };
            guiding::star_stats(&mono, aperture / 2.0, MAX_STARS).map(|s| protocol::StarStats {
                hfr: s.hfr * 2.0,
                ..s
            })
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("fits.part");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// The calibrated copy of `raw`: in a `calibrated` folder beside it.
pub fn calibrated_file(raw: &Path) -> PathBuf {
    let dir = raw.parent().unwrap_or(Path::new("")).join("calibrated");
    dir.join(raw.file_name().unwrap_or_default())
}

/// First free path for this frame according to the file template.
#[cfg(test)]
pub fn output_path(settings: &ImagingSettings, ctx: &FrameContext) -> PathBuf {
    free_path(settings, ctx, |p| !p.exists())
}

/// First path from the file template that `free` accepts.
fn free_path(settings: &ImagingSettings, ctx: &FrameContext, free: impl Fn(&Path) -> bool) -> PathBuf {
    let root = PathBuf::from(&settings.output_dir);
    let template = if settings.file_template.contains("{seq") {
        settings.file_template.clone()
    } else {
        // Never overwrite: fall back to a numbered suffix.
        format!("{}_{{seq:04}}", settings.file_template)
    };
    for seq in 1..1_000_000 {
        let path = root.join(format!("{}.fits", render(&template, ctx, seq)));
        if free(&path) {
            return path;
        }
    }
    root.join(format!("{}.fits", render(&template, ctx, 0)))
}

/// Expand `{token}` / `{token:0N}` placeholders. Unknown tokens stay as written.
pub fn render(template: &str, ctx: &FrameContext, seq: u32) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let token = &after[..close];
        let (name, width) = match token.split_once(':') {
            Some((n, w)) => (n, w.trim_start_matches('0').parse::<usize>().ok()),
            None => (token, None),
        };
        match value(name, ctx, seq) {
            Some(v) => match width {
                Some(w) if name == "seq" => out.push_str(&format!("{seq:0w$}")),
                _ => out.push_str(&sanitize(&v)),
            },
            None => {
                out.push('{');
                out.push_str(token);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

fn value(name: &str, ctx: &FrameContext, seq: u32) -> Option<String> {
    let num = |v: Option<f64>| v.map_or("NA".to_owned(), compact);
    Some(match name {
        "object" if ctx.object.is_empty() => ctx.frame_type.label().to_owned(),
        "object" => ctx.object.clone(),
        "type" => ctx.frame_type.label().to_owned(),
        "filter" => ctx.filter.clone().unwrap_or_else(|| "NoFilter".into()),
        "exposure" => compact(ctx.exposure),
        "gain" => num(ctx.gain),
        "offset" => num(ctx.offset),
        "temp" => ctx
            .temperature
            .map_or("NA".into(), |t| format!("{}", t.round() as i64)),
        "bin" => match ctx.binning {
            Some((h, v)) if h == v => h.to_string(),
            Some((h, v)) => format!("{h}x{v}"),
            None => "1".into(),
        },
        "camera" => ctx.camera.clone(),
        // The night a session belongs to: local date 12 h before.
        "date" => (ctx.start.with_timezone(&Local) - Duration::hours(12))
            .format("%Y-%m-%d")
            .to_string(),
        "time" => ctx.start.format("%H%M%S").to_string(),
        "seq" => seq.to_string(),
        _ => return None,
    })
}

/// 300.0 → "300", 0.25 → "0.25"
fn compact(v: f64) -> String {
    let s = format!("{:.3}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Make a value safe as (part of) a single path component.
fn sanitize(v: &str) -> String {
    let s: String = v
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.+".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.chars().all(|c| c == '.') {
        "_".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use protocol::FrameType;

    fn ctx() -> FrameContext {
        FrameContext {
            camera: "ZWO ASI2600MM".into(),
            frame_type: FrameType::Light,
            exposure: 300.0,
            object: "M 31/../x".into(),
            filter: Some("Ha".into()),
            start: Utc.with_ymd_and_hms(2026, 10, 18, 1, 30, 0).unwrap(),
            gain: Some(100.0),
            offset: Some(50.0),
            temperature: Some(-9.8),
            binning: Some((1, 1)),
            bits_per_pixel: Some(16),
            cards: vec![],
            defaults: vec![],
        }
    }

    #[test]
    fn renders_template() {
        let c = ctx();
        let date = (c.start.with_timezone(&Local) - Duration::hours(12))
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(
            render(protocol::DEFAULT_FILE_TEMPLATE, &c, 7),
            format!("M_31_.._x/{date}/Light/Ha/M_31_.._x_Light_Ha_300s_G100_O50_-10C_bin1_0007")
        );
        assert_eq!(render("{type}_{unknown}_{seq}", &c, 3), "Light_{unknown}_3");
        let mut dark = c.clone();
        dark.frame_type = FrameType::Dark;
        dark.object.clear();
        dark.filter = None;
        dark.gain = None;
        dark.exposure = 0.5;
        assert_eq!(render("{object}_{filter}_{exposure}s_G{gain}", &dark, 1), "Dark_NoFilter_0.5s_GNA");
    }

    #[test]
    fn never_overwrites() {
        let dir = std::env::temp_dir().join(format!("indicontrol-frame-{}", std::process::id()));
        let settings = ImagingSettings {
            output_dir: dir.display().to_string(),
            file_template: "{type}/{filter}".into(),
            ..Default::default()
        };
        let first = output_path(&settings, &ctx());
        assert!(first.ends_with("Light/Ha_0001.fits"), "{}", first.display());
        write_atomic(&first, b"x").unwrap();
        let second = output_path(&settings, &ctx());
        assert!(second.ends_with("Light/Ha_0002.fits"), "{}", second.display());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
