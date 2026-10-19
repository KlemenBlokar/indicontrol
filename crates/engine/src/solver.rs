//! Running an external plate solver (ASTAP or astrometry.net) and reading its WCS.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use imaging::{Fits, Header};
use protocol::{SolveResult, SolverKind, SolverSettings};

/// What we know about the frame before solving.
#[derive(Debug, Clone)]
pub struct SolveRequest {
    pub file: PathBuf,
    pub width: usize,
    pub height: usize,
    /// Mount position (J2000 hours, degrees).
    pub hint: Option<(f64, f64)>,
    /// Expected arcseconds per pixel.
    pub scale: Option<f64>,
}

/// TAN-projection WCS.
#[derive(Debug, Clone, PartialEq)]
pub struct Wcs {
    pub crval: (f64, f64),
    pub crpix: (f64, f64),
    /// CD matrix, degrees per pixel.
    pub cd: [[f64; 2]; 2],
}

impl Wcs {
    fn from_lookup(get: impl Fn(&str) -> Option<f64>) -> Result<Wcs, String> {
        let need = |k: &str| get(k).ok_or_else(|| format!("solution has no {k}"));
        let crval = (need("CRVAL1")?, need("CRVAL2")?);
        let crpix = (need("CRPIX1")?, need("CRPIX2")?);
        let cd = match (get("CD1_1"), get("CD1_2"), get("CD2_1"), get("CD2_2")) {
            (Some(a), Some(b), Some(c), Some(d)) => [[a, b], [c, d]],
            _ => {
                // Older convention: CDELT + CROTA2.
                let (c1, c2) = (need("CDELT1")?, need("CDELT2")?);
                let rot = get("CROTA2").unwrap_or(0.0).to_radians();
                [[c1 * rot.cos(), -c2 * rot.sin()], [c1 * rot.sin(), c2 * rot.cos()]]
            }
        };
        Ok(Wcs { crval, crpix, cd })
    }

    pub fn from_header(h: &Header) -> Result<Wcs, String> {
        Wcs::from_lookup(|k| h.get_f64(k))
    }

    /// Sky position (degrees) of a 1-based FITS pixel.
    pub fn pixel_to_sky(&self, x: f64, y: f64) -> (f64, f64) {
        let (dx, dy) = (x - self.crpix.0, y - self.crpix.1);
        let xi = (self.cd[0][0] * dx + self.cd[0][1] * dy).to_radians();
        let eta = (self.cd[1][0] * dx + self.cd[1][1] * dy).to_radians();
        let (a0, d0) = (self.crval.0.to_radians(), self.crval.1.to_radians());
        let den = d0.cos() - eta * d0.sin();
        let ra = a0 + xi.atan2(den);
        let dec = (d0.sin() + eta * d0.cos()).atan2(xi.hypot(den));
        (ra.to_degrees().rem_euclid(360.0), dec.to_degrees())
    }

    pub fn pixel_scale(&self) -> f64 {
        let det = self.cd[0][0] * self.cd[1][1] - self.cd[0][1] * self.cd[1][0];
        det.abs().sqrt() * 3600.0
    }

    /// Image rotation in the FITS CROTA2 sense (what ASTAP and most tools
    /// display): CD = [[Δ1 cos ρ, −Δ2 sin ρ], [Δ1 sin ρ, Δ2 cos ρ]].
    pub fn rotation(&self) -> f64 {
        (-self.cd[0][1]).atan2(self.cd[1][1]).to_degrees().rem_euclid(360.0)
    }

    /// A sky image seen through a telescope (no mirror) has east 90° counter-
    /// clockwise from north, which gives a negative CD determinant.
    pub fn mirrored(&self) -> bool {
        self.cd[0][0] * self.cd[1][1] - self.cd[0][1] * self.cd[1][0] > 0.0
    }
}

/// ASTAP writes `<file>.ini` with `KEY=value` lines.
pub fn parse_astap_ini(text: &str) -> Result<Wcs, String> {
    let get = |key: &str| {
        text.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().to_owned())
        })
    };
    if get("PLTSOLVD").as_deref() != Some("T") {
        let why = get("ERROR").or_else(|| get("WARNING")).unwrap_or_else(|| "no solution".into());
        return Err(format!("ASTAP: {}", if why.is_empty() { "no solution" } else { &why }));
    }
    Wcs::from_lookup(|k| get(k).and_then(|v| v.parse().ok()))
}

/// Program and arguments for the configured solver.
pub fn command(settings: &SolverSettings, req: &SolveRequest, workdir: &Path) -> (String, Vec<String>) {
    let mut args: Vec<String> = Vec::new();
    let file = req.file.display().to_string();
    let program = match settings.kind {
        SolverKind::Astap => {
            args.extend(["-f".into(), file]);
            args.extend(["-r".into(), format!("{}", settings.search_radius.clamp(1.0, 180.0))]);
            if settings.downsample > 0 {
                args.extend(["-z".into(), settings.downsample.to_string()]);
            }
            if let Some((ra, dec)) = req.hint {
                args.extend(["-ra".into(), format!("{ra:.6}"), "-spd".into(), format!("{:.6}", dec + 90.0)]);
            }
            if let Some(scale) = req.scale {
                // ASTAP wants the field height in degrees.
                args.extend(["-fov".into(), format!("{:.4}", scale * req.height as f64 / 3600.0)]);
            }
            settings.astap_path.clone()
        }
        SolverKind::AstrometryNet => {
            args.push(file);
            args.extend(
                ["--overwrite", "--no-plots", "--new-fits", "none", "--corr", "none", "--match", "none"]
                    .map(String::from),
            );
            args.extend(["--dir".into(), workdir.display().to_string()]);
            args.extend(["--cpulimit".into(), format!("{:.0}", settings.timeout)]);
            if settings.downsample > 0 {
                args.extend(["--downsample".into(), settings.downsample.to_string()]);
            }
            if let Some((ra, dec)) = req.hint {
                args.extend([
                    "--ra".into(),
                    format!("{:.6}", ra * 15.0),
                    "--dec".into(),
                    format!("{dec:.6}"),
                    "--radius".into(),
                    format!("{}", settings.search_radius),
                ]);
            }
            if let Some(scale) = req.scale {
                args.extend([
                    "--scale-units".into(),
                    "arcsecperpix".into(),
                    "--scale-low".into(),
                    format!("{:.4}", scale * 0.8),
                    "--scale-high".into(),
                    format!("{:.4}", scale * 1.2),
                ]);
            }
            settings.astrometry_path.clone()
        }
    };
    args.extend(settings.extra_args.split_whitespace().map(String::from));
    (program, args)
}

/// Where the solver leaves its WCS for `file`.
fn output_path(kind: SolverKind, file: &Path, workdir: &Path) -> PathBuf {
    match kind {
        SolverKind::Astap => file.with_extension("ini"),
        SolverKind::AstrometryNet => workdir.join(file.with_extension("wcs").file_name().expect("file name")),
    }
}

pub async fn solve(settings: &SolverSettings, req: &SolveRequest) -> Result<SolveResult, String> {
    let workdir = req.file.parent().unwrap_or(Path::new(".")).to_path_buf();
    let (program, args) = command(settings, req, &workdir);
    let started = Instant::now();
    let child = tokio::process::Command::new(&program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!(
                "{} not found ('{program}') — install it or set its path in the solver settings",
                settings.kind.label()
            ),
            _ => format!("could not start {program}: {e}"),
        })?;
    let output = tokio::time::timeout(Duration::from_secs_f64(settings.timeout + 5.0), child.wait_with_output())
        .await
        .map_err(|_| format!("{} timed out after {:.0} s", settings.kind.label(), settings.timeout))?
        .map_err(|e| format!("{program} failed: {e}"))?;

    let out_file = output_path(settings.kind, &req.file, &workdir);
    let wcs = match settings.kind {
        SolverKind::Astap => {
            let text = std::fs::read_to_string(&out_file)
                .map_err(|_| failure_text(settings.kind, &output))?;
            parse_astap_ini(&text)?
        }
        SolverKind::AstrometryNet => {
            let bytes = std::fs::read(&out_file).map_err(|_| failure_text(settings.kind, &output))?;
            let fits = Fits::parse(&bytes).map_err(|e| format!("bad WCS file: {e}"))?;
            Wcs::from_header(&fits.header)?
        }
    };
    let (ra, dec) = wcs.pixel_to_sky((req.width as f64 + 1.0) / 2.0, (req.height as f64 + 1.0) / 2.0);
    let scale = wcs.pixel_scale();
    Ok(SolveResult {
        ra_hours: ra / 15.0,
        dec_deg: dec,
        pixel_scale: scale,
        rotation: wcs.rotation(),
        mirrored: wcs.mirrored(),
        width_deg: scale * req.width as f64 / 3600.0,
        height_deg: scale * req.height as f64 / 3600.0,
        solver: settings.kind,
        seconds: started.elapsed().as_secs_f64(),
        pointing_error: req.hint.map(|(hra, hdec)| separation_arcmin(hra, hdec, ra / 15.0, dec)),
    })
}

fn failure_text(kind: SolverKind, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let last = stderr
        .lines()
        .chain(stdout.lines()).rfind(|l| !l.trim().is_empty())
        .unwrap_or("");
    format!("{} found no solution{}", kind.label(), if last.is_empty() { String::new() } else { format!(" ({last})") })
}

pub use protocol::mount::separation_arcmin;

#[cfg(test)]
mod tests {
    use super::*;
    use imaging::Card;

    /// Real ASTAP output for an INDI CCD simulator frame (Pleiades, camera
    /// rotated by 30°, 5.2 µm pixels at 500 mm).
    const ASTAP_SIM_ROTATED: &str = "PLTSOLVD=T\nCRPIX1= 6.4050000000000000E+002\nCRPIX2= 5.1250000000000000E+002\n\
        CRVAL1= 5.6742522965195235E+001\nCRVAL2= 2.4113337591358043E+001\n\
        CDELT1= 5.9585080177927967E-004\nCDELT2= 5.9559141360341868E-004\n\
        CROTA1= 1.5000189054107602E+002\nCROTA2= 1.4997474871878060E+002\n\
        CD1_1=-5.1603176132138052E-004\nCD1_2=-2.9790837398194128E-004\n\
        CD2_1= 2.9802299902436399E-004\nCD2_2=-5.1566600044083044E-004\nCMDLINE=astap_cli -f rot_001.fits\n";

    #[test]
    fn astap_ini() {
        let wcs = parse_astap_ini(ASTAP_SIM_ROTATED).unwrap();
        assert_eq!(wcs.crval, (56.742522965195235, 24.113337591358043));
        // 206.265 · 5.2 µm / 500 mm = 2.145"/px
        assert!((wcs.pixel_scale() - 2.145).abs() < 0.002, "{}", wcs.pixel_scale());
        // Matches ASTAP's own CROTA2.
        assert!((wcs.rotation() - 149.97).abs() < 0.05, "{}", wcs.rotation());
        // CDELT1 > 0: the simulator's images are mirrored.
        assert!(wcs.mirrored());
        // CRPIX is the centre for ASTAP.
        let (ra, dec) = wcs.pixel_to_sky(640.5, 512.5);
        assert!((ra - 56.742522965195235).abs() < 1e-9 && (dec - 24.113337591358043).abs() < 1e-9);

        let err = parse_astap_ini("PLTSOLVD=F\nERROR=Not enough stars\n").unwrap_err();
        assert!(err.contains("Not enough stars"), "{err}");
    }

    #[test]
    fn wcs_off_centre_reference_pixel() {
        // astrometry.net style: CRPIX far from the centre.
        let mut h = Header::default();
        for c in [
            Card::new("CRVAL1", 10.0, ""),
            Card::new("CRVAL2", 60.0, ""),
            Card::new("CRPIX1", 100.0, ""),
            Card::new("CRPIX2", 200.0, ""),
            Card::new("CD1_1", -0.001, ""),
            Card::new("CD1_2", 0.0, ""),
            Card::new("CD2_1", 0.0, ""),
            Card::new("CD2_2", 0.001, ""),
        ] {
            h.set(c);
        }
        let wcs = Wcs::from_header(&h).unwrap();
        // 1000 px "up" from the reference pixel is 1° north.
        let (ra, dec) = wcs.pixel_to_sky(100.0, 1200.0);
        assert!((dec - 61.0).abs() < 1e-3 && (ra - 10.0).abs() < 1e-6, "{ra} {dec}");
        // 1000 px left (x decreasing, CD1_1 < 0) is east: RA grows by 1°/cos(dec).
        let (ra, dec) = wcs.pixel_to_sky(-900.0, 200.0);
        assert!((ra - 10.0 - 1.0 / 60f64.to_radians().cos()).abs() < 0.01, "{ra}");
        // The tangent plane bends towards the equator: 1° east at Dec 60° is at ~59.985°.
        assert!((dec - 60.0).abs() < 0.02, "{dec}");
        assert!((wcs.pixel_scale() - 3.6).abs() < 1e-9);
        assert_eq!(wcs.rotation(), 0.0);
    }

    #[test]
    fn command_lines() {
        let req = SolveRequest {
            file: "/tmp/s/f.fits".into(),
            width: 3000,
            height: 2000,
            hint: Some((5.5, -5.0)),
            scale: Some(1.8),
        };
        let astap = SolverSettings::default();
        let (prog, args) = command(&astap, &req, Path::new("/tmp/s"));
        assert_eq!(prog, "astap");
        assert_eq!(
            args.join(" "),
            "-f /tmp/s/f.fits -r 10 -z 2 -ra 5.500000 -spd 85.000000 -fov 1.0000"
        );
        let anet = SolverSettings {
            kind: SolverKind::AstrometryNet,
            extra_args: "--no-verify".into(),
            ..Default::default()
        };
        let (prog, args) = command(&anet, &req, Path::new("/tmp/s"));
        assert_eq!(prog, "solve-field");
        let line = args.join(" ");
        assert!(line.starts_with("/tmp/s/f.fits --overwrite --no-plots"), "{line}");
        assert!(line.contains("--ra 82.500000 --dec -5.000000 --radius 10"));
        assert!(line.contains("--scale-units arcsecperpix --scale-low 1.4400 --scale-high 2.1600"));
        assert!(line.ends_with("--no-verify"));
        assert_eq!(
            output_path(SolverKind::AstrometryNet, Path::new("/tmp/s/f.fits"), Path::new("/w")),
            PathBuf::from("/w/f.wcs")
        );
    }

    #[test]
    fn separations() {
        assert!((separation_arcmin(5.0, 10.0, 5.0, 11.0) - 60.0).abs() < 1e-9);
        assert!((separation_arcmin(0.0, 60.0, 1.0, 60.0) - 15.0 * 60.0 * 0.5).abs() < 2.0);
        assert!(separation_arcmin(23.99, 0.0, 0.01, 0.0) < 20.0);
    }

    #[tokio::test]
    async fn missing_solver_is_reported() {
        let req = SolveRequest {
            file: std::env::temp_dir().join("nope.fits"),
            width: 10,
            height: 10,
            hint: None,
            scale: None,
        };
        let s = SolverSettings {
            astap_path: "/nonexistent/astap".into(),
            ..Default::default()
        };
        let err = solve(&s, &req).await.unwrap_err();
        assert!(err.contains("ASTAP not found"), "{err}");
    }
}
