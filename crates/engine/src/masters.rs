//! Reads the masters folder: the header of every FITS file in it (and its
//! subfolders), kept if it is a stacked dark, flat or bias.

use std::io::Read;
use std::path::Path;

use imaging::fits::{self, Header};
use protocol::{MasterFrame, MasterKind, MastersStatus};

const EXTENSIONS: [&str; 3] = ["fit", "fits", "fts"];
/// Folders deeper than this are not searched.
const MAX_DEPTH: usize = 4;
/// Headers longer than this are not believed to be FITS.
const MAX_HEADER: usize = 64 * 2880;

/// Blocking: call from `spawn_blocking`.
pub fn scan(folder: &Path) -> MastersStatus {
    let mut status = MastersStatus {
        folder: folder.display().to_string(),
        ..Default::default()
    };
    if !folder.is_dir() {
        status.error = Some("the folder does not exist yet".into());
        return status;
    }
    let mut files = Vec::new();
    collect(folder, 0, &mut files);
    files.sort();
    for path in files {
        let rel = path.strip_prefix(folder).unwrap_or(&path).display().to_string();
        match read(&path, &rel) {
            Ok(m) => status.masters.push(m),
            Err(why) => status.skipped.push((rel, why)),
        }
    }
    status
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            if depth < MAX_DEPTH {
                collect(&path, depth + 1, out);
            }
        } else if path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| EXTENSIONS.contains(&x.to_lowercase().as_str()))
        {
            out.push(path);
        }
    }
}

fn read_header(path: &Path) -> Result<Header, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    let mut block = vec![0u8; 2880];
    while bytes.len() < MAX_HEADER {
        file.read_exact(&mut block).map_err(|_| "not a FITS file (header ends early)".to_string())?;
        bytes.extend_from_slice(&block);
        match fits::read_header(&bytes) {
            Ok((h, _)) => return Ok(h),
            Err(fits::FitsError::NoEnd) => continue,
            Err(_) => return Err("not a FITS file".into()),
        }
    }
    Err("not a FITS file (no END in the header)".into())
}

fn read(path: &Path, rel: &str) -> Result<MasterFrame, String> {
    let h = read_header(path)?;
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let typed = ["IMAGETYP", "FRAME"].iter().find_map(|k| h.get_str(k)).map(str::to_owned);
    let kind = typed
        .as_deref()
        .and_then(MasterKind::recognise)
        .or_else(|| MasterKind::recognise(file_name))
        .ok_or_else(|| match &typed {
            Some(t) => format!("not a dark, flat or bias ({t})"),
            None => "not a dark, flat or bias (no IMAGETYP, nothing in the name)".into(),
        })?;
    let frames = ["STACKCNT", "NCOMBINE"].iter().find_map(|k| h.get_i64(k)).map(|n| n.max(0) as u32);
    let float = matches!(h.get_i64("BITPIX"), Some(-32 | -64));
    let lower = file_name.to_lowercase();
    let named = ["master", "stack", "mdark", "mflat", "mbias"].iter().any(|w| lower.contains(w));
    if frames.is_none() && !float && !named {
        return Err(format!("a single {} frame, not a master", kind.label()));
    }
    let dim = |k| h.get_i64(k).filter(|&v| v > 0).map(|v| v as u32);
    let (Some(width), Some(height)) = (dim("NAXIS1"), dim("NAXIS2")) else {
        return Err("no image size in the header".into());
    };
    let text = |k| h.get_str(k).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    let date = text("DATE-OBS").unwrap_or_else(|| {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(|t| chrono::DateTime::<chrono::Utc>::from(t).format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default()
    });
    Ok(MasterFrame {
        path: rel.to_owned(),
        kind,
        camera: text("INSTRUME"),
        width,
        height,
        binning: h.get_i64("XBINNING").map(|b| b.max(1) as u32),
        exposure: h.get_f64("EXPTIME").or_else(|| h.get_f64("EXPOSURE")),
        gain: h.get_f64("GAIN"),
        offset: h.get_f64("OFFSET"),
        temperature: h.get_f64("CCD-TEMP"),
        filter: text("FILTER"),
        frames,
        date,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use imaging::{Card, Fits};

    fn write(path: &Path, cards: &[Card], bitpix: i64) {
        let mut h = Header::default();
        h.set(Card::new("SIMPLE", true, ""));
        h.set(Card::new("BITPIX", bitpix, ""));
        h.set(Card::new("NAXIS", 2i64, ""));
        h.set(Card::new("NAXIS1", 4i64, ""));
        h.set(Card::new("NAXIS2", 2i64, ""));
        for c in cards {
            h.set(c.clone());
        }
        let data = vec![0u8; 8 * (bitpix.unsigned_abs() as usize / 8)];
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, Fits { header: h, data }.to_bytes()).unwrap();
    }

    #[test]
    fn finds_masters_and_explains_the_rest() {
        let dir = std::env::temp_dir().join(format!("indicontrol-masters-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Siril style: float, type from the frames, stack count.
        write(
            &dir.join("darks/dark_stacked.fit"),
            &[
                Card::new("IMAGETYP", "Dark Frame", ""),
                Card::new("EXPTIME", 120.0, ""),
                Card::new("GAIN", 100i64, ""),
                Card::new("CCD-TEMP", -10.1, ""),
                Card::new("XBINNING", 1i64, ""),
                Card::new("STACKCNT", 30i64, ""),
                Card::new("INSTRUME", "ZWO ASI2600MM", ""),
                Card::new("DATE-OBS", "2026-10-01T20:00:00", ""),
            ],
            -32,
        );
        // Only the name says what it is.
        write(&dir.join("master-flat_Ha.fits"), &[Card::new("FILTER", "Ha ", "")], 16);
        // A raw frame and a light: skipped, with the reason.
        write(&dir.join("raw/Dark_120s_0001.fits"), &[Card::new("IMAGETYP", "Dark Frame", "")], 16);
        write(&dir.join("M31_stacked.fits"), &[Card::new("IMAGETYP", "Light Frame", "")], -32);
        std::fs::write(dir.join("notes.fits"), b"hello").unwrap();

        let s = scan(&dir);
        assert_eq!(s.error, None);
        assert_eq!(s.masters.len(), 2, "{:?}", s.skipped);
        let dark = s.masters.iter().find(|m| m.kind == MasterKind::Dark).unwrap();
        assert_eq!(dark.path, "darks/dark_stacked.fit");
        assert_eq!((dark.exposure, dark.gain, dark.temperature), (Some(120.0), Some(100.0), Some(-10.1)));
        assert_eq!((dark.frames, dark.width, dark.height), (Some(30), 4, 2));
        assert_eq!(dark.date, "2026-10-01T20:00:00");
        let flat = s.masters.iter().find(|m| m.kind == MasterKind::Flat).unwrap();
        assert_eq!(flat.filter.as_deref(), Some("Ha"));
        assert!(!flat.date.is_empty(), "file time when there is no DATE-OBS");

        let why = |name: &str| s.skipped.iter().find(|(p, _)| p.ends_with(name)).map(|(_, w)| w.clone()).unwrap();
        assert_eq!(why("Dark_120s_0001.fits"), "a single dark frame, not a master");
        assert!(why("M31_stacked.fits").contains("Light Frame"));
        assert!(why("notes.fits").starts_with("not a FITS file"));

        assert!(scan(&dir.join("missing")).error.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
