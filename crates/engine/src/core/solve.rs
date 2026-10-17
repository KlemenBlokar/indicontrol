//! Plate solving jobs: solve & sync, and slew-solve-sync-repeat centring.

use std::sync::Arc;
use std::time::Duration;

use imaging::{Fits, astro};
use protocol::equipment;
use protocol::{CaptureRequest, FrameType, LogLevel, SolveResult, ToUi};

use super::capture::Purpose;
use super::{Core, CoreMsg};
use crate::solver::{self, SolveRequest, separation_arcmin};

const SLEW_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// If a goto never reports Busy (already there), treat it as done after this.
const SLEW_NOTICE: Duration = Duration::from_secs(5);
/// Let the mount settle after a slew before exposing.
const SLEW_SETTLE: Duration = Duration::from_secs(1);

pub(crate) struct SolveJob {
    id: u64,
    mode: Mode,
    stage: Stage,
    attempt: u32,
    /// Restart guiding when done (user-started jobs only).
    restart_guiding: bool,
    from_sequence: bool,
    /// Bumped on every stage change, so stale timers are ignored.
    epoch: u64,
    /// A solver result that arrived while the solve frame was still being
    /// processed; handled once the capture has finished.
    pending: Option<Result<SolveResult, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Sync,
    Center { ra: f64, dec: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    Slewing { seen_busy: bool },
    Settling,
    Exposing,
    Solving,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SolveTimer {
    SlewNotice,
    SlewTimeout,
    Settled,
}

impl Core {
    pub(super) fn solving(&self) -> bool {
        self.solve_job.is_some()
    }

    pub(super) fn start_solve(&mut self, target: Option<(f64, f64)>, from_sequence: bool) -> Result<(), String> {
        if self.solve_job.is_some() {
            return Err("plate solving is already running".into());
        }
        if self.job.is_some() {
            return Err("a capture is running".into());
        }
        if !from_sequence && self.run.is_some() {
            return Err("a sequence is running".into());
        }
        if self.state.roles.main_camera.is_none() {
            return Err("assign a main camera in Equipment first".into());
        }
        if self.state.roles.mount.is_none() {
            return Err("plate solving needs a mount to sync".into());
        }
        let restart_guiding = !from_sequence && self.guider_is_guiding();
        if target.is_some() {
            self.guider_stop();
        }
        self.next_solve_id += 1;
        self.solve_job = Some(SolveJob {
            id: self.next_solve_id,
            mode: match target {
                Some((ra, dec)) => Mode::Center { ra, dec },
                None => Mode::Sync,
            },
            stage: Stage::Exposing,
            attempt: 0,
            restart_guiding,
            from_sequence,
            epoch: 0,
            pending: None,
        });
        match target {
            Some(_) => self.solve_slew(),
            None => self.solve_expose(),
        }
    }

    fn solve_slew(&mut self) -> Result<(), String> {
        let Some(Mode::Center { ra, dec }) = self.solve_job.as_ref().map(|j| j.mode) else {
            return Err("not centring".into());
        };
        self.mount_goto(ra, dec)?;
        self.solve_stage(Stage::Slewing { seen_busy: false }, "Slewing to target");
        self.solve_timer(SolveTimer::SlewNotice, SLEW_NOTICE);
        self.solve_timer(SolveTimer::SlewTimeout, SLEW_TIMEOUT);
        Ok(())
    }

    fn solve_expose(&mut self) -> Result<(), String> {
        let s = self.state.solver.clone();
        let req = CaptureRequest {
            exposure: s.exposure,
            frame_type: FrameType::Light,
            binning: s.binning,
            gain: s.gain,
            offset: None,
            filter_slot: s.filter_slot,
            object: String::new(),
            save: false,
            repeat: false,
        };
        self.solve_stage(Stage::Exposing, "Taking solve frame");
        self.start_capture(req, Purpose::Solve)
    }

    fn solve_stage(&mut self, stage: Stage, message: &str) {
        if let Some(job) = &mut self.solve_job {
            job.stage = stage;
            job.epoch += 1;
            let attempt = match job.mode {
                Mode::Center { .. } => format!(" (attempt {})", job.attempt + 1),
                Mode::Sync => String::new(),
            };
            self.solve_status.busy = true;
            self.solve_status.message = format!("{message}{attempt}");
        }
        self.send_solve();
    }

    fn solve_timer(&self, timer: SolveTimer, after: Duration) {
        let Some(job) = &self.solve_job else {
            return;
        };
        let (id, epoch, tx) = (job.id, job.epoch, self.tx.clone());
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let _ = tx.send(CoreMsg::SolveTimer { id, epoch, timer });
        });
    }

    pub(super) fn solve_on_timer(&mut self, id: u64, epoch: u64, timer: SolveTimer) {
        let Some(job) = &self.solve_job else {
            return;
        };
        if job.id != id || job.epoch != epoch {
            return;
        }
        match (timer, job.stage) {
            (SolveTimer::SlewNotice, Stage::Slewing { seen_busy: false }) => {
                if !self.mount_info().is_some_and(|m| m.slewing) {
                    self.solve_slew_done();
                }
            }
            (SolveTimer::SlewTimeout, Stage::Slewing { .. }) => self.solve_failed("the slew did not finish".into()),
            (SolveTimer::Settled, Stage::Settling) => {
                if let Err(e) = self.solve_expose() {
                    self.solve_failed(e);
                }
            }
            _ => {}
        }
    }

    /// Mount updates: detect the end of our slew.
    pub(super) fn solve_on_update(&mut self, device: &str, name: &str) {
        let Some(job) = &self.solve_job else {
            return;
        };
        if self.state.roles.mount.as_deref() != Some(device) || name != "EQUATORIAL_EOD_COORD" {
            return;
        }
        let Stage::Slewing { seen_busy } = job.stage else {
            return;
        };
        let slewing = self.mount_info().is_some_and(|m| m.slewing);
        if slewing && !seen_busy {
            if let Some(job) = &mut self.solve_job {
                job.stage = Stage::Slewing { seen_busy: true };
            }
        } else if !slewing && seen_busy {
            self.solve_slew_done();
        }
    }

    fn solve_slew_done(&mut self) {
        self.solve_stage(Stage::Settling, "Settling after slew");
        self.solve_timer(SolveTimer::Settled, SLEW_SETTLE);
    }

    /// The solve frame arrived: hand it to the solver.
    pub(super) fn solve_frame(&mut self, blob: Arc<Vec<u8>>) {
        let Some(job) = &self.solve_job else {
            return;
        };
        let id = job.id;
        let settings = self.state.solver.clone();
        let hint = self.mount_info().map(|m| (m.ra_j2000, m.dec_j2000));
        let scale = self.main_pixel_scale();
        self.solve_stage(Stage::Solving, &format!("Solving with {}", settings.kind.label()));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = run_solver(id, blob, settings, hint, scale).await;
            let _ = tx.send(CoreMsg::SolveDone {
                id,
                result: Box::new(result),
            });
        });
    }

    pub(super) fn solve_done(&mut self, id: u64, result: Result<SolveResult, String>) {
        if self.solve_job.as_ref().is_none_or(|j| j.id != id) {
            return;
        }
        // The solve frame may still be processing (preview); finish that first.
        if self.job.is_some() {
            self.solve_job.as_mut().expect("checked").pending = Some(result);
            return;
        }
        let r = match result {
            Ok(r) => r,
            Err(e) => return self.solve_failed(e),
        };
        let mut text = format!(
            "Solved in {:.1} s: {} {}, {:.2}\"/px, rotation {:.1}°",
            r.seconds,
            astro::format_ra(r.ra_hours),
            astro::format_dec(r.dec_deg),
            r.pixel_scale,
            r.rotation
        );
        if let Some(e) = r.pointing_error {
            text += &format!(", mount was off by {e:.1}'");
        }
        self.log(None, text, LogLevel::Info);
        self.solve_status.last = Some(r.clone());
        if let Err(e) = self.mount_sync(r.ra_hours, r.dec_deg) {
            return self.solve_failed(format!("sync failed: {e}"));
        }
        let job = self.solve_job.as_mut().expect("checked");
        match job.mode {
            Mode::Sync => self.solve_finish(true, "Mount synced".into()),
            Mode::Center { ra, dec } => {
                let error = separation_arcmin(ra, dec, r.ra_hours, r.dec_deg);
                let tolerance = self.state.solver.center_tolerance;
                job.attempt += 1;
                if error <= tolerance {
                    self.solve_finish(true, format!("Centred within {error:.1}'"));
                } else if job.attempt >= self.state.solver.center_attempts {
                    self.solve_finish(
                        true,
                        format!("Could not centre within {tolerance}' (still {error:.1}' off) — continuing"),
                    );
                } else {
                    self.log(None, format!("{error:.1}' from target — slewing again"), LogLevel::Info);
                    if let Err(e) = self.solve_slew() {
                        self.solve_failed(e);
                    }
                }
            }
        }
    }

    /// The solve frame's capture job has finished.
    pub(super) fn solve_capture_done(&mut self) {
        if let Some(job) = &mut self.solve_job
            && let Some(result) = job.pending.take()
        {
            let id = job.id;
            self.solve_done(id, result);
        }
    }

    pub(super) fn solve_failed(&mut self, reason: String) {
        if self.solve_job.is_some() {
            self.solve_finish(false, format!("Plate solving failed: {reason}"));
        }
    }

    pub(super) fn abort_solve(&mut self) {
        if self.solve_job.is_some() {
            if self.job.is_some() {
                self.abort_capture();
            }
            self.solve_finish(false, "Plate solving aborted".into());
        }
    }

    fn solve_finish(&mut self, ok: bool, message: String) {
        let Some(job) = self.solve_job.take() else {
            return;
        };
        self.log(None, message.clone(), if ok { LogLevel::Info } else { LogLevel::Error });
        self.solve_status.busy = false;
        self.solve_status.message = message;
        self.send_solve();
        if job.restart_guiding
            && let Err(e) = self.guider_restart_after_flip()
        {
            self.log(None, format!("Could not restart guiding: {e}"), LogLevel::Warning);
        }
        if job.from_sequence {
            self.sequence_center_done(ok);
        }
    }

    /// Tell the mount where it really points (J2000 in).
    fn mount_sync(&mut self, ra_hours: f64, dec_deg: f64) -> Result<(), String> {
        let client = self.client.as_ref().ok_or("not connected")?;
        let name = self.state.roles.mount.clone().ok_or("no mount")?;
        let has_j2000 = client
            .cache()
            .devices
            .get(&name)
            .is_some_and(|m| m.properties.contains_key("EQUATORIAL_COORD"));
        let e = |r: Result<(), indi_link::ClientError>| r.map_err(|e| e.to_string());
        e(client.send_switch(&name, "ON_COORD_SET", &[("SYNC", true)]))?;
        if has_j2000 {
            e(client.send_number(&name, "EQUATORIAL_COORD", &[("RA", ra_hours), ("DEC", dec_deg)]))?;
        } else {
            let jd = astro::julian_day(chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
            let (ra, dec) = astro::precess(ra_hours, dec_deg, astro::J2000, jd);
            e(client.send_number(&name, "EQUATORIAL_EOD_COORD", &[("RA", ra), ("DEC", dec)]))?;
        }
        e(client.send_switch(&name, "ON_COORD_SET", &[("TRACK", true)]))
    }

    /// Arcseconds per main-camera pixel (with the solve binning), if known.
    fn main_pixel_scale(&self) -> Option<f64> {
        let cache = self.client.as_ref()?.cache();
        let cam = cache.devices.get(self.state.roles.main_camera.as_ref()?)?;
        let mount = self.state.roles.mount.as_ref().and_then(|m| cache.devices.get(m));
        let pixel = equipment::number(cam, "CCD_INFO", "CCD_PIXEL_SIZE").map(|n| n.value).filter(|p| *p > 0.0)?;
        let focal = equipment::number(cam, "SCOPE_INFO", "FOCAL_LENGTH")
            .map(|n| n.value)
            .filter(|f| *f > 0.0)
            .or_else(|| mount.and_then(|m| equipment::number(m, "TELESCOPE_INFO", "TELESCOPE_FOCAL_LENGTH")).map(|n| n.value))
            .filter(|f| *f > 0.0)?;
        let bin = self.state.solver.binning.unwrap_or(1).max(1) as f64;
        Some(206.265 * pixel * bin / focal)
    }

    pub(super) fn solve_msg(&self) -> ToUi {
        ToUi::Solve {
            status: self.solve_status.clone(),
        }
    }

    fn send_solve(&self) {
        self.send(self.solve_msg());
    }
}

async fn run_solver(
    id: u64,
    blob: Arc<Vec<u8>>,
    settings: protocol::SolverSettings,
    hint: Option<(f64, f64)>,
    scale: Option<f64>,
) -> Result<SolveResult, String> {
    // Unique per process *and* engine instance (several engines may share a process).
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("indicontrol-solve-{}", std::process::id()));
    let stem = format!("solve_{id}_{unique}");
    let file = dir.join(format!("{stem}.fits"));
    let (width, height) = {
        let fits = Fits::parse(&blob).map_err(|e| format!("bad solve frame: {e}"))?;
        let (w, h, _) = fits.dims().map_err(|e| e.to_string())?;
        (w, h)
    };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(&file, blob.as_slice()).map_err(|e| e.to_string())?;
    let req = SolveRequest {
        file: file.clone(),
        width,
        height,
        hint,
        scale,
    };
    let result = solver::solve(&settings, &req).await;
    // Clean up the frame and the solver's side files.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(&format!("{stem}.")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    result
}
