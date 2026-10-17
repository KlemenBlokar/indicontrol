//! Autoguider: loops the guide camera, tracks a star, calibrates and guides.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use guiding::{
    CalStep, Calibration, Calibrator, DitherRng, Frame, GuideController, GuideStats, SettleOutcome,
    SettleTracker, Star, dither_offset,
};
use imaging::{Fits, Image, analyze, encode_jpeg, make_preview};
use protocol::equipment;
use protocol::preview_frame::{self, Kind};
use protocol::guider::GuideDir;
use protocol::{GuideOutput, GuideStep, GuiderState, GuiderStatus, LogLevel, ToUi};

use super::{Core, CoreMsg, Outgoing};

const PREVIEW_MAX_DIM: usize = 1024;
/// Guide errors kept for the RMS statistics.
const STATS_WINDOW: usize = 100;
const EXPOSURE_TIMEOUT: f64 = 30.0;

#[derive(Default)]
pub(crate) struct GuiderCore {
    mode: Mode,
    camera: Option<String>,
    exposing: bool,
    /// Identifies the current exposure; stale results and timers are ignored.
    seq: u64,
    star: Option<Star>,
    lock: Option<(f64, f64)>,
    frame_size: Option<(usize, usize)>,
    select: Option<Select>,
    lost: u32,
    errors: VecDeque<(f64, f64)>,
    steps: u64,
    started: Option<Instant>,
    message: String,
    guide_after_calibration: bool,
    last_preview: Option<Outgoing>,
    /// Calibration in use (flipped if the mount changed pier side).
    active_calibration: Option<Calibration>,
    settle: Option<SettleTracker>,
    rng: DitherRng,
}

#[derive(Default)]
enum Mode {
    #[default]
    Idle,
    Looping,
    Calibrating(Box<Calibrator>),
    Guiding(GuideController),
}

#[derive(Debug, Clone, Copy)]
enum Select {
    Auto,
    At(f64, f64),
}

pub(crate) struct GuideFrameOut {
    width: usize,
    height: usize,
    star: Option<Star>,
    jpeg: Vec<u8>,
}

fn process_frame(
    blob: Arc<Vec<u8>>,
    format: String,
    select: Option<Select>,
    track: Option<(f64, f64)>,
    radius: f64,
) -> Result<GuideFrameOut, String> {
    if !format.starts_with(".fit") {
        return Err(format!("guide camera sent '{format}', need FITS"));
    }
    let fits = Fits::parse(&blob).map_err(|e| format!("bad guide frame: {e}"))?;
    let img = Image::from_fits(&fits).map_err(|e| format!("bad guide frame: {e}"))?;
    let frame = Frame::new(&img);
    let r = radius.max(3.0) as usize;
    let star = match select {
        Some(Select::Auto) => frame.detect(1).first().copied(),
        Some(Select::At(x, y)) => frame.measure_near(x, y, r.max(20)),
        None => track.and_then(|(x, y)| frame.measure_near(x, y, r)),
    };
    let jpeg = encode_jpeg(&make_preview(&img, &analyze(&img, None), PREVIEW_MAX_DIM), 80);
    Ok(GuideFrameOut {
        width: img.width,
        height: img.height,
        star,
        jpeg,
    })
}

impl Core {
    pub(super) fn guider_loop(&mut self) -> Result<(), String> {
        let camera = self
            .state
            .roles
            .guide_camera
            .clone()
            .ok_or("assign a guide camera in Equipment first")?;
        if self.state.roles.main_camera.as_deref() == Some(camera.as_str()) {
            return Err("the guide camera must differ from the main camera".into());
        }
        let client = self.client.as_ref().ok_or("not connected to an INDI server")?;
        {
            let cache = client.cache();
            let cam = cache
                .devices
                .get(&camera)
                .filter(|d| d.is_connected())
                .ok_or_else(|| format!("guide camera '{camera}' is not connected"))?;
            if !cam.properties.contains_key("CCD_EXPOSURE") {
                return Err(format!("'{camera}' has no CCD_EXPOSURE property"));
            }
            client
                .enable_blob(&camera, None, indi_link::BlobMode::Also)
                .map_err(|e| e.to_string())?;
            if cam
                .properties
                .get("UPLOAD_MODE")
                .is_some_and(|p| p.switch("UPLOAD_CLIENT") == Some(false))
            {
                client
                    .send_switch(&camera, "UPLOAD_MODE", &[("UPLOAD_CLIENT", true)])
                    .map_err(|e| e.to_string())?;
            }
        }
        let g = &mut self.guider;
        if g.camera.as_deref() != Some(camera.as_str()) {
            g.star = None;
            g.lock = None;
        }
        g.camera = Some(camera);
        if matches!(g.mode, Mode::Idle) {
            g.mode = Mode::Looping;
            g.message = "Looping".into();
        }
        self.guide_expose();
        Ok(())
    }

    fn guide_expose(&mut self) {
        let g = &mut self.guider;
        if g.exposing || matches!(g.mode, Mode::Idle) {
            return;
        }
        let (Some(client), Some(camera)) = (&self.client, &g.camera) else {
            return;
        };
        let exposure = self.state.guider.exposure;
        if let Err(e) = client.send_number(camera, "CCD_EXPOSURE", &[("CCD_EXPOSURE_VALUE", exposure)]) {
            self.guider_halt(format!("guide exposure failed: {e}"));
            return;
        }
        g.exposing = true;
        g.seq += 1;
        let (seq, tx) = (g.seq, self.tx.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(exposure + EXPOSURE_TIMEOUT)).await;
            let _ = tx.send(CoreMsg::GuideTimeout { seq });
        });
    }

    pub(super) fn guider_on_blob(&mut self, device: &str, format: String, blob: Arc<Vec<u8>>) -> bool {
        let g = &mut self.guider;
        if g.camera.as_deref() != Some(device) || !g.exposing {
            return false;
        }
        let (seq, select, radius) = (g.seq, g.select, self.state.guider.search_radius);
        let track = g.star.map(|s| (s.x, s.y)).or(g.lock);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || process_frame(blob, format, select, track, radius))
                .await
                .unwrap_or_else(|e| Err(format!("guide frame processing crashed: {e}")));
            let _ = tx.send(CoreMsg::GuideFrame {
                seq,
                result: Box::new(result),
            });
        });
        true
    }

    pub(super) fn guider_frame(&mut self, seq: u64, result: Result<GuideFrameOut, String>) {
        let g = &mut self.guider;
        if seq != g.seq || matches!(g.mode, Mode::Idle) {
            return;
        }
        g.exposing = false;
        let out = match result {
            Ok(out) => out,
            Err(e) => {
                self.log(None, e, LogLevel::Warning);
                self.guide_next_after(1000.0);
                return;
            }
        };
        g.frame_size = Some((out.width, out.height));
        let preview = Outgoing::Binary(Bytes::from(preview_frame::encode(Kind::Guide, seq, &out.jpeg)));
        g.last_preview = Some(preview.clone());
        self.broadcast(preview);

        let g = &mut self.guider;
        if g.select.take().is_some() {
            match out.star {
                Some(s) => {
                    g.star = Some(s);
                    g.lock = Some((s.x, s.y));
                    g.lost = 0;
                    let text = format!("Guide star at ({:.1}, {:.1}), SNR {:.0}", s.x, s.y, s.snr);
                    self.log(None, text, LogLevel::Info);
                }
                None => {
                    g.message = "No suitable guide star found".into();
                    if !matches!(g.mode, Mode::Looping) {
                        let text = "No guide star found — stopping".to_owned();
                        g.mode = Mode::Looping;
                        self.log(None, text, LogLevel::Error);
                    }
                    self.send_guider();
                    self.guide_expose();
                    return;
                }
            }
        } else {
            match out.star {
                Some(s) => {
                    g.star = Some(s);
                    g.lost = 0;
                }
                None => g.lost += 1,
            }
        }

        let max_lost = self.state.guider.max_lost_frames;
        let declination = self.mount_declination();
        let calibration = self.guider.active_calibration.clone();
        let g = &mut self.guider;
        match &mut g.mode {
            Mode::Idle => {}
            Mode::Looping => {
                g.message = match (&g.star, g.lost) {
                    (Some(s), 0) => format!("Looping — star SNR {:.0}, HFD {:.1} px", s.snr, s.hfd),
                    (Some(_), n) => format!("Looping — star not found in {n} frames"),
                    (None, _) => "Looping — select a guide star".into(),
                };
                self.send_guider();
                self.guide_expose();
            }
            Mode::Calibrating(cal) => {
                if out.star.is_none() {
                    if g.lost >= max_lost {
                        return self.guider_fail_to_loop("Guide star lost during calibration".into());
                    }
                    self.send_guider();
                    return self.guide_expose();
                }
                let s = g.star.expect("tracked");
                match cal.step((s.x, s.y)) {
                    CalStep::Pulse(dir, ms) => {
                        g.message = cal.describe();
                        self.send_guider();
                        match self.guide_pulse(&[(dir, ms)]) {
                            Ok(()) => self.guide_next_after(ms),
                            Err(e) => self.guider_fail_to_loop(e),
                        }
                    }
                    CalStep::Done(c) => {
                        let text = format!(
                            "Calibrated: RA {:.0}° at {:.2} px/s, Dec {:.0}° at {:.2} px/s, \
                             orthogonality error {:.1}°",
                            c.ra_angle.to_degrees(),
                            c.ra_rate * 1000.0,
                            c.dec_angle.to_degrees(),
                            c.dec_rate * 1000.0,
                            c.orthogonality_error()
                        );
                        let level = if c.orthogonality_error() > 10.0 {
                            LogLevel::Warning
                        } else {
                            LogLevel::Info
                        };
                        self.state.guider_calibration = Some(c.clone());
                        self.guider.active_calibration = Some(c);
                        self.save();
                        self.log(None, text, level);
                        let g = &mut self.guider;
                        g.lock = Some((s.x, s.y));
                        if std::mem::take(&mut g.guide_after_calibration) {
                            self.guider_start_guiding();
                        } else {
                            g.mode = Mode::Looping;
                            g.message = "Calibrated".into();
                            self.send_guider();
                        }
                        self.guide_expose();
                    }
                    CalStep::Failed(e) => self.guider_fail_to_loop(format!("Calibration failed: {e}")),
                }
            }
            Mode::Guiding(ctl) => {
                let elapsed = g.started.map_or(0.0, |t| t.elapsed().as_secs_f64());
                g.steps += 1;
                let Some(s) = out.star else {
                    let step = GuideStep {
                        frame: g.steps,
                        time: elapsed,
                        ra_px: 0.0,
                        dec_px: 0.0,
                        ra_pulse: None,
                        dec_pulse: None,
                        snr: 0.0,
                        star_lost: true,
                    };
                    let lost = g.lost;
                    g.message = format!("Star lost ({lost} frames)");
                    let settled = g.settle.as_mut().and_then(|t| t.update(elapsed, None));
                    self.send(ToUi::GuideStep(step));
                    if let Some(outcome) = settled {
                        self.guider_settled(outcome);
                    }
                    if lost >= max_lost {
                        return self.guider_fail_to_loop("Guide star lost".into());
                    }
                    self.send_guider();
                    return self.guide_expose();
                };
                let lock = *g.lock.get_or_insert((s.x, s.y));
                let Some(cal) = calibration else {
                    return self.guider_fail_to_loop("no calibration".into());
                };
                let c = ctl.compute(&cal, declination, lock, (s.x, s.y));
                g.errors.push_back((c.ra_px, c.dec_px));
                if g.errors.len() > STATS_WINDOW {
                    g.errors.pop_front();
                }
                let stats = GuideStats::from_errors(g.errors.iter().copied());
                g.message = format!("Guiding — RMS {:.2} px", stats.total_rms);
                let settled = g
                    .settle
                    .as_mut()
                    .and_then(|t| t.update(elapsed, Some(c.ra_px.hypot(c.dec_px))));
                if g.settle.is_some() {
                    g.message = format!("Settling — error {:.2} px", c.ra_px.hypot(c.dec_px));
                }
                let step = GuideStep {
                    frame: g.steps,
                    time: elapsed,
                    ra_px: c.ra_px,
                    dec_px: c.dec_px,
                    ra_pulse: c.ra,
                    dec_pulse: c.dec,
                    snr: s.snr,
                    star_lost: false,
                };
                let pulses: Vec<(GuideDir, f64)> = [c.ra, c.dec].into_iter().flatten().collect();
                let longest = pulses.iter().map(|p| p.1).fold(0.0, f64::max);
                self.send(ToUi::GuideStep(step));
                if let Some(outcome) = settled {
                    self.guider_settled(outcome);
                }
                self.send_guider();
                match self.guide_pulse(&pulses) {
                    Ok(()) => self.guide_next_after(longest),
                    Err(e) => self.guider_fail_to_loop(e),
                }
            }
        }
    }

    /// Wait for pulses to finish (plus settling), then expose again.
    fn guide_next_after(&mut self, pulse_ms: f64) {
        let delay = Duration::from_secs_f64((pulse_ms + self.state.guider.settle_ms).max(0.0) / 1000.0);
        let (seq, tx) = (self.guider.seq, self.tx.clone());
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(CoreMsg::GuideNext { seq });
        });
    }

    pub(super) fn guider_next(&mut self, seq: u64) {
        if seq == self.guider.seq {
            self.guide_expose();
        }
    }

    pub(super) fn guider_timeout(&mut self, seq: u64) {
        let g = &mut self.guider;
        if seq == g.seq && g.exposing {
            g.exposing = false;
            self.log(None, "Guide camera did not deliver a frame; retrying", LogLevel::Warning);
            self.guide_expose();
        }
    }

    fn guide_pulse(&self, pulses: &[(GuideDir, f64)]) -> Result<(), String> {
        if pulses.is_empty() {
            return Ok(());
        }
        let client = self.client.as_ref().ok_or("not connected")?;
        let device = match self.state.guider.output {
            GuideOutput::Mount => self.state.roles.mount.clone().ok_or("no mount assigned for guide pulses")?,
            GuideOutput::GuideCamera => self.guider.camera.clone().ok_or("no guide camera")?,
        };
        let mut ns = [0.0, 0.0];
        let mut we = [0.0, 0.0];
        for &(dir, ms) in pulses {
            let ms = ms.round();
            match dir {
                GuideDir::North => ns[0] = ms,
                GuideDir::South => ns[1] = ms,
                GuideDir::West => we[0] = ms,
                GuideDir::East => we[1] = ms,
            }
        }
        let send = |prop: &str, els: [(&str, f64); 2]| {
            client
                .send_number(&device, prop, &els)
                .map_err(|e| format!("guide pulse failed: {e}"))
        };
        {
            let cache = client.cache();
            let dev = cache.devices.get(&device).ok_or_else(|| format!("'{device}' is offline"))?;
            if !dev.properties.contains_key("TELESCOPE_TIMED_GUIDE_NS") {
                return Err(format!("'{device}' does not support pulse guiding"));
            }
        }
        if ns != [0.0, 0.0] {
            send("TELESCOPE_TIMED_GUIDE_NS", [("TIMED_GUIDE_N", ns[0]), ("TIMED_GUIDE_S", ns[1])])?;
        }
        if we != [0.0, 0.0] {
            send("TELESCOPE_TIMED_GUIDE_WE", [("TIMED_GUIDE_W", we[0]), ("TIMED_GUIDE_E", we[1])])?;
        }
        Ok(())
    }

    pub(super) fn guider_select(&mut self, at: Option<(f64, f64)>) -> Result<(), String> {
        if matches!(self.guider.mode, Mode::Calibrating(_) | Mode::Guiding(_)) {
            return Err("stop guiding before choosing another star".into());
        }
        self.guider.select = Some(match at {
            Some((x, y)) => Select::At(x, y),
            None => Select::Auto,
        });
        self.guider_loop()
    }

    pub(super) fn guider_calibrate(&mut self, then_guide: bool) -> Result<(), String> {
        if self.guider.star.is_none() {
            self.guider.select = Some(Select::Auto);
        }
        self.guider_loop()?;
        let (dec, pier) = (self.mount_declination(), self.mount_pier_side());
        let g = &mut self.guider;
        g.guide_after_calibration = then_guide;
        g.mode = Mode::Calibrating(Box::new(Calibrator::new(
            self.state.guider.calibration.clone(),
            dec,
            pier,
        )));
        g.message = "Calibrating".into();
        self.send_guider();
        Ok(())
    }

    pub(super) fn guider_guide(&mut self) -> Result<(), String> {
        if self.state.guider_calibration.is_none() {
            self.log(None, "No calibration yet — calibrating first", LogLevel::Info);
            return self.guider_calibrate(true);
        }
        if self.guider.star.is_none() {
            self.guider.select = Some(Select::Auto);
        }
        self.guider_loop()?;
        self.guider_start_guiding();
        Ok(())
    }

    fn guider_start_guiding(&mut self) {
        let active = self.calibration_for_pier_side();
        let settings = self.state.guider.clone();
        let g = &mut self.guider;
        g.active_calibration = active;
        g.settle = Some(SettleTracker::new(settings.settle_px, settings.settle_time, settings.settle_timeout, 0.0));
        g.mode = Mode::Guiding(GuideController::new(self.state.guider.params.clone()));
        g.lock = g.star.map(|s| (s.x, s.y)).or(g.lock);
        g.errors.clear();
        g.steps = 0;
        g.lost = 0;
        g.started = Some(Instant::now());
        g.message = "Guiding".into();
        self.log(None, "Guiding started", LogLevel::Info);
        self.send_guider();
    }

    /// After a meridian flip the field has rotated: pick a star afresh.
    pub(super) fn guider_restart_after_flip(&mut self) -> Result<(), String> {
        self.guider.star = None;
        self.guider.lock = None;
        self.guider_guide()
    }

    /// The stored calibration, turned 180° if the mount is now on the other
    /// side of the pier.
    fn calibration_for_pier_side(&mut self) -> Option<Calibration> {
        let cal = self.state.guider_calibration.clone()?;
        let (Some(side), Some(now)) = (cal.pier_side.clone(), self.mount_pier_side()) else {
            return Some(cal);
        };
        if side == now {
            return Some(cal);
        }
        if self.state.guider.flip_calibration {
            self.log(
                None,
                format!("Calibrated on the {side} side, now {now}: using the flipped calibration"),
                LogLevel::Info,
            );
            Some(cal.flipped(self.state.guider.flip_dec))
        } else {
            self.log(
                None,
                format!("Calibrated on the {side} side, mount is now {now} — recalibrate"),
                LogLevel::Warning,
            );
            Some(cal)
        }
    }

    /// Move the lock position by a random offset and wait for guiding to settle.
    pub(super) fn guider_dither(&mut self) -> Result<(), String> {
        let settings = self.state.guider.clone();
        let g = &mut self.guider;
        if !matches!(g.mode, Mode::Guiding(_)) {
            return Err("dithering needs active guiding".into());
        }
        let (Some(cal), Some((lx, ly))) = (&g.active_calibration, g.lock) else {
            return Err("no calibration or lock position".into());
        };
        let (dx, dy) = dither_offset(cal, settings.dither_px, settings.dither_ra_only, &mut g.rng);
        g.lock = Some((lx + dx, ly + dy));
        let now = g.started.map_or(0.0, |t| t.elapsed().as_secs_f64());
        g.settle = Some(SettleTracker::new(settings.settle_px, settings.settle_time, settings.settle_timeout, now));
        g.message = "Dithering".into();
        self.log(None, format!("Dither by ({dx:+.1}, {dy:+.1}) px"), LogLevel::Info);
        self.send_guider();
        Ok(())
    }

    fn guider_settled(&mut self, outcome: SettleOutcome) {
        self.guider.settle = None;
        match outcome {
            SettleOutcome::Settled => self.log(None, "Guiding settled", LogLevel::Info),
            SettleOutcome::TimedOut => {
                self.log(None, "Guiding did not settle in time — continuing", LogLevel::Warning)
            }
        }
        self.sequence_on_settled(outcome);
    }

    pub(super) fn guider_is_guiding(&self) -> bool {
        matches!(self.guider.mode, Mode::Guiding(_))
    }

    pub(super) fn guider_stop(&mut self) {
        let g = &mut self.guider;
        if matches!(g.mode, Mode::Idle) {
            return;
        }
        let was_guiding = matches!(g.mode, Mode::Guiding(_));
        g.mode = Mode::Idle;
        g.settle = None;
        g.seq += 1;
        if std::mem::take(&mut g.exposing)
            && let (Some(c), Some(cam)) = (&self.client, &g.camera)
        {
            let _ = c.send_switch(cam, "CCD_ABORT_EXPOSURE", &[("ABORT", true)]);
        }
        g.message = "Stopped".into();
        if was_guiding {
            self.log(None, "Guiding stopped", LogLevel::Info);
        }
        self.send_guider();
    }

    /// Stop everything (e.g. INDI lost).
    pub(super) fn guider_halt(&mut self, reason: String) {
        if !matches!(self.guider.mode, Mode::Idle) {
            self.guider_stop();
            self.guider.message = reason.clone();
            self.log(None, reason, LogLevel::Error);
            self.send_guider();
        }
    }

    /// Drop back to looping after a calibration/guiding failure.
    fn guider_fail_to_loop(&mut self, reason: String) {
        let was_settling = self.guider.settle.take().is_some();
        let g = &mut self.guider;
        g.mode = Mode::Looping;
        g.guide_after_calibration = false;
        g.message = reason.clone();
        self.log(None, reason, LogLevel::Error);
        self.send_guider();
        self.guide_expose();
        if was_settling {
            self.sequence_on_settled(SettleOutcome::TimedOut);
        }
    }

    fn mount_declination(&self) -> Option<f64> {
        let cache = self.client.as_ref()?.cache();
        let mount = cache.devices.get(self.state.roles.mount.as_ref()?)?;
        equipment::number(mount, "EQUATORIAL_EOD_COORD", "DEC").map(|n| n.value)
    }

    fn mount_pier_side(&self) -> Option<String> {
        let cache = self.client.as_ref()?.cache();
        let mount = cache.devices.get(self.state.roles.mount.as_ref()?)?;
        match mount.properties.get("TELESCOPE_PIER_SIDE")?.active_switch()? {
            "PIER_WEST" => Some("WEST".into()),
            "PIER_EAST" => Some("EAST".into()),
            _ => None,
        }
    }

    /// Arcseconds per guide pixel from settings or INDI.
    fn guide_pixel_scale(&self) -> Option<f64> {
        let s = &self.state.guider;
        let cache = self.client.as_ref().map(|c| c.cache());
        let cam = cache
            .as_ref()
            .and_then(|c| c.devices.get(self.state.roles.guide_camera.as_ref()?));
        let mount = cache.as_ref().and_then(|c| c.devices.get(self.state.roles.mount.as_ref()?));
        let pixel = s
            .pixel_size
            .or_else(|| cam.and_then(|c| equipment::number(c, "CCD_INFO", "CCD_PIXEL_SIZE")).map(|n| n.value))
            .filter(|p| *p > 0.0)?;
        let focal = s
            .focal_length
            .or_else(|| cam.and_then(|c| equipment::number(c, "SCOPE_INFO", "FOCAL_LENGTH")).map(|n| n.value).filter(|f| *f > 0.0))
            .or_else(|| mount.and_then(|m| equipment::number(m, "TELESCOPE_INFO", "GUIDER_FOCAL_LENGTH")).map(|n| n.value))
            .filter(|f| *f > 0.0)?;
        let bin = cam
            .and_then(|c| equipment::number(c, "CCD_BINNING", "HOR_BIN"))
            .map_or(1.0, |n| n.value.max(1.0));
        Some(206.265 * pixel * bin / focal)
    }

    pub(super) fn guider_msg(&self) -> ToUi {
        let g = &self.guider;
        ToUi::Guider {
            status: Box::new(GuiderStatus {
                state: match g.mode {
                    Mode::Idle => GuiderState::Idle,
                    Mode::Looping => GuiderState::Looping,
                    Mode::Calibrating(_) => GuiderState::Calibrating,
                    Mode::Guiding(_) => GuiderState::Guiding,
                },
                camera: g.camera.clone().or_else(|| self.state.roles.guide_camera.clone()),
                frame_size: g.frame_size,
                star: g.star,
                lock: g.lock,
                calibration: g.active_calibration.clone().or_else(|| self.state.guider_calibration.clone()),
                pixel_scale: self.guide_pixel_scale(),
                stats: GuideStats::from_errors(g.errors.iter().copied()),
                settling: g.settle.is_some(),
                message: g.message.clone(),
            }),
        }
    }

    pub(super) fn guider_last_preview(&self) -> Option<Outgoing> {
        self.guider.last_preview.clone()
    }

    pub(super) fn send_guider(&self) {
        self.send(self.guider_msg());
    }
}
