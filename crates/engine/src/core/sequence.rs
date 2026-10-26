//! Sequencer: runs a list of steps through the capture machinery, handling
//! sensor temperature, flat auto-exposure and dark flats.

use std::time::Duration;

use protocol::equipment::{self, Control};
use guiding::SettleOutcome;
use protocol::{
    CaptureRequest, Exposure, FlatRecord, FrameType, LogLevel, RunState, Sequence, SequenceOrder, Setup,
    SequenceStatus, ToUi,
};

use protocol::DocId;

use super::{Core, CoreMsg};

const TEMPERATURE_TOLERANCE: f64 = 0.5;
const TEMPERATURE_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Test exposures allowed to find a flat exposure before giving up.
const MAX_FLAT_TESTS: u32 = 15;
/// How long test frames go on while the flat source is too bright or dark.
const FLAT_SOURCE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const FLIP_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Sidereal hours per solar hour.
const SIDEREAL_RATE: f64 = 1.002_737_9;

pub(crate) struct Run {
    id: u64,
    pub(super) seq: Sequence,
    done: Vec<u32>,
    current: Option<usize>,
    wait: Wait,
    pause_requested: bool,
    paused: bool,
    flat: Vec<FlatState>,
    message: String,
    lights_since_dither: u32,
    lights_since_focus: u32,
    focused_once: bool,
    last_focus_filter: Option<u32>,
    focus_after_flip: bool,
    /// Missing masters already reported in this run.
    pub(super) calibration_warned: std::collections::HashSet<String>,
}

#[derive(Debug, Clone, Default)]
struct FlatState {
    exposure: Option<f64>,
    /// The gain the search chose (`None`: the step's).
    gain: Option<f64>,
    calibrated: bool,
    tests: u32,
    /// The previous test frame, to learn how the gain scales the signal.
    last: Option<super::flat::Sample>,
    /// Since when the source has been too bright or dark, and what to do.
    source: Option<(std::time::Instant, String)>,
}

#[derive(Debug, Clone, PartialEq)]
enum Wait {
    None,
    Temperature { step: usize, target: f64 },
    Frame { step: usize, test: bool, exposure: f64, gain: Option<f64> },
    /// Guiding is settling after a dither.
    Settle,
    /// Waiting for the target to reach the flip point.
    Meridian,
    /// Slewing to and centring the target before the first frame.
    CenterStart { restart_guiding: bool },
    /// Autofocus before the next light frame (for this filter).
    Focus { filter: Option<u32> },
    Flip {
        phase: FlipPhase,
        restart_guiding: bool,
        pier_before: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FlipPhase {
    Slewing { seen_busy: bool },
    /// Re-centring the target by plate solving (the slew that flips).
    Centering,
    GuideSettle,
}

impl Core {
    pub(super) fn start_sequence(&mut self, mut seq: Sequence) -> Result<(), String> {
        if self.run.is_some() {
            return Err("a sequence is already running".into());
        }
        if self.job.is_some() {
            return Err("a capture is running".into());
        }
        if self.client.is_none() {
            return Err("not connected to an INDI server".into());
        }
        if self.state.roles.main_camera.is_none() {
            return Err("assign a main camera in Equipment first".into());
        }
        if seq.planned_frames() == 0 {
            return Err("the sequence has no enabled frames".into());
        }
        if seq.setup()? != Some(Setup::Sky) {
            // Calibration frames: nothing to point at.
            seq.target = None;
            seq.center_on_start = false;
            seq.meridian_flip = false;
            seq.dither_every = 0;
            seq.autofocus = Default::default();
        }
        // The engine's draft has the latest progress of the steps it shares.
        let draft = &self.state.sequence_draft.sequence;
        for step in &mut seq.steps {
            if let Some(d) = draft.steps.iter().find(|d| d.id != 0 && d.id == step.id) {
                step.done = d.done;
                step.frames = d.frames.clone();
            }
        }
        seq.assign_step_ids(&mut self.next_step_id);
        if seq.remaining_frames() == 0 {
            return Err("every step is complete; reset the progress to take them again".into());
        }
        if seq.steps.iter().any(|s| s.enabled && s.filter_slot.is_some())
            && self.state.roles.filter_wheel.is_none()
        {
            return Err("steps use filters but no filter wheel is assigned".into());
        }
        self.next_run_id += 1;
        let n = seq.steps.len();
        let name = if seq.name.is_empty() { "Sequence".into() } else { seq.name.clone() };
        let done: Vec<u32> = seq.steps.iter().map(|s| s.done).collect();
        let taken: u32 = seq.steps.iter().map(|s| s.done.min(s.count)).sum();
        let planned = seq.planned_frames();
        self.run = Some(Run {
            id: self.next_run_id,
            seq,
            done,
            current: None,
            wait: Wait::None,
            pause_requested: false,
            paused: false,
            flat: vec![FlatState::default(); n],
            message: String::new(),
            lights_since_dither: 0,
            lights_since_focus: 0,
            focused_once: false,
            last_focus_filter: None,
            focus_after_flip: false,
            calibration_warned: Default::default(),
        });
        let started = if taken > 0 {
            format!("{name} continues: {taken} of {planned} frames already taken")
        } else {
            format!("{name} started")
        };
        self.log(None, started, LogLevel::Info);
        let run = self.run.as_ref().expect("just created");
        if let (Some(t), true) = (run.seq.target, run.seq.center_on_start) {
            let restart_guiding = self.guider_is_guiding();
            self.run.as_mut().expect("created").wait = Wait::CenterStart { restart_guiding };
            if let Err(e) = self.start_solve(Some((t.ra_hours, t.dec_deg)), true) {
                self.run = None;
                self.send_sequence();
                return Err(format!("could not centre the target: {e}"));
            }
            self.set_sequence_message("Centring the target");
            return Ok(());
        }
        self.sequence_next();
        Ok(())
    }

    pub(super) fn pause_sequence(&mut self) {
        let Some(run) = &mut self.run else {
            return;
        };
        if matches!(run.wait, Wait::None | Wait::Temperature { .. } | Wait::Meridian) {
            run.paused = true;
            run.wait = Wait::None;
            run.message = "Paused".into();
        } else {
            run.pause_requested = true;
        }
        self.send_sequence();
    }

    pub(super) fn resume_sequence(&mut self) {
        let Some(run) = &mut self.run else {
            return;
        };
        run.pause_requested = false;
        if std::mem::take(&mut run.paused) {
            self.sequence_next();
        } else {
            self.send_sequence();
        }
    }

    pub(super) fn abort_sequence(&mut self) {
        if self.run.take().is_some() {
            self.abort_solve();
            self.abort_autofocus();
            self.abort_capture();
            self.log(None, "Sequence aborted", LogLevel::Warning);
            self.send_sequence();
        }
    }

    pub(super) fn sequence_failed(&mut self, reason: String) {
        if self.run.take().is_some() {
            self.log(None, format!("Sequence stopped: {reason}"), LogLevel::Error);
            self.send_sequence();
        }
    }

    /// Decide and start the next action.
    fn sequence_next(&mut self) {
        let Some(run) = &mut self.run else {
            return;
        };
        if std::mem::take(&mut run.pause_requested) {
            run.paused = true;
            run.wait = Wait::None;
            run.message = "Paused".into();
            self.send_sequence();
            return;
        }
        let Some(step) = pick_step(run) else {
            let total: u32 = run.done.iter().sum();
            let name = run.seq.name.clone();
            self.run = None;
            self.log(None, format!("{} complete: {total} frames", if name.is_empty() { "Sequence" } else { &name }), LogLevel::Info);
            self.send_sequence();
            return;
        };
        run.current = Some(step);
        let s = run.seq.steps[step].clone();

        // Sensor temperature first.
        if let Some(target) = s.temperature {
            let current = self.camera_temperature();
            if current.is_none_or(|t| (t - target).abs() > TEMPERATURE_TOLERANCE) {
                if let Err(e) = self.set_camera_temperature(target) {
                    return self.sequence_failed(e);
                }
                let run = self.run.as_mut().expect("checked");
                run.wait = Wait::Temperature { step, target };
                run.message = match current {
                    Some(t) => format!("Cooling to {target:.1} °C (now {t:.1} °C)"),
                    None => format!("Waiting for {target:.1} °C"),
                };
                let (id, tx) = (run.id, self.tx.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(TEMPERATURE_TIMEOUT).await;
                    let _ = tx.send(CoreMsg::TemperatureTimeout { run_id: id, step });
                });
                self.send_sequence();
                return;
            }
        }

        if s.frame_type == FrameType::Light && self.before_light(&s) {
            return;
        }

        let (exposure, test, gain) = match self.resolve_exposure(step) {
            Ok(v) => v,
            Err(e) => return self.sequence_failed(e),
        };
        let run = self.run.as_mut().expect("checked");
        run.wait = Wait::None;
        let req = CaptureRequest {
            exposure,
            frame_type: s.frame_type,
            binning: s.binning,
            gain,
            offset: s.offset,
            filter_slot: s.filter_slot,
            object: run.seq.object.clone(),
            save: !test,
            repeat: false,
        };
        run.wait = Wait::Frame { step, test, exposure, gain };
        run.message = if test {
            let at = match gain {
                Some(g) if gain != s.gain => format!(" at gain {g}"),
                _ => String::new(),
            };
            match &run.flat[step].source {
                Some((_, fix)) => format!("Flat step {}: {fix}; test exposure {}{at}", step + 1, fmt_seconds(exposure)),
                None => format!("Flat step {}: test exposure {}{at}", step + 1, fmt_seconds(exposure)),
            }
        } else {
            format!(
                "Step {}: frame {} of {} ({})",
                step + 1,
                run.done[step] + 1,
                s.count,
                fmt_seconds(exposure)
            )
        };
        self.send_sequence();
        if let Err(e) = self.start_capture(req, super::capture::Purpose::Sequence) {
            self.sequence_failed(e);
        }
    }

    /// Exposure for the next frame of `step`, whether it is a test frame,
    /// and the gain to take it with.
    fn resolve_exposure(&self, step: usize) -> Result<(f64, bool, Option<f64>), String> {
        let run = self.run.as_ref().expect("running");
        let s = &run.seq.steps[step];
        match &s.exposure {
            Exposure::Fixed { seconds } => Ok((*seconds, false, s.gain)),
            Exposure::Minimum => Ok((self.camera_min_exposure(), false, s.gain)),
            Exposure::MatchFlat => {
                let key = self.flat_key(step);
                self.flat_record(&key)
                    .map(|r| (r.exposure, false, r.flat_gain.or(s.gain)))
                    .ok_or_else(|| {
                        format!(
                            "no flat exposure known for {} — take flats first",
                            key.filter.as_deref().unwrap_or("this setup")
                        )
                    })
            }
            Exposure::AutoFlat { min, max, .. } => {
                let f = &run.flat[step];
                let record = self.flat_record(&self.flat_key(step));
                let exposure = f.exposure.or(record.map(|r| r.exposure)).unwrap_or(1.0).clamp(*min, *max);
                let gain = f.gain.or(record.and_then(|r| r.flat_gain)).or(s.gain);
                Ok((exposure, !f.calibrated, gain))
            }
        }
    }

    /// What was found for flats like `key` before.
    fn flat_record(&self, key: &FlatRecord) -> Option<&FlatRecord> {
        self.state
            .flat_exposures
            .iter()
            .find(|r| (r.filter.as_ref(), r.binning, r.gain) == (key.filter.as_ref(), key.binning, key.gain))
    }

    /// The camera's gain now and its range, if it has a gain control.
    fn camera_gain(&self) -> Option<(f64, f64, f64)> {
        let cache = self.client.as_ref()?.cache();
        let cam = cache.devices.get(self.state.roles.main_camera.as_ref()?)?;
        equipment::control_value(cam, Control::Gain).map(|n| (n.value, n.min, n.max))
    }

    /// Called by the capture code after each sequence frame.
    /// `brightest` is the highest channel median as a fraction of full scale.
    pub(super) fn sequence_frame_done(&mut self, brightest: f64, path: Option<String>) {
        let Some(run) = &mut self.run else {
            return;
        };
        let Wait::Frame { step, test, exposure, gain } = run.wait else {
            return;
        };
        run.wait = Wait::None;
        if run.seq.steps[step].frame_type == FrameType::Light {
            run.lights_since_dither += 1;
            run.lights_since_focus += 1;
        }
        let Exposure::AutoFlat {
            target,
            tolerance,
            min,
            max,
            window,
        } = run.seq.steps[step].exposure
        else {
            self.count_frame(step, path);
            return self.sequence_next();
        };

        // Flats: keep frames that are close enough, and steer the exposure.
        if !test && (brightest - target).abs() <= 2.0 * tolerance {
            self.count_frame(step, path);
        }
        let camera = self.camera_gain();
        let Some(run) = &mut self.run else {
            return;
        };
        let step_gain = run.seq.steps[step].gain;
        // Frames of a step share one gain, so it changes only before the first.
        let gain_range = match (window, camera) {
            (Some(_), Some((_, lo, hi))) if run.done[step] == 0 => Some((lo, hi)),
            _ => None,
        };
        let limits = super::flat::Limits {
            target,
            tolerance,
            min: min.max(self.camera_min_exposure()),
            max,
            window,
            gain_range,
        };
        let run = self.run.as_mut().expect("checked");
        let now = super::flat::Sample {
            exposure,
            gain: gain.or(camera.map(|c| c.0)),
            brightest,
        };
        let flat = &mut run.flat[step];
        let next = super::flat::next(&limits, now, flat.last);
        flat.last = Some(now);
        let level = format!("{:.0} %", brightest * 100.0);
        let at = |exposure: f64, gain: Option<f64>| match gain {
            Some(g) => format!("{} at gain {g}", fmt_seconds(exposure)),
            None => fmt_seconds(exposure),
        };
        match next {
            super::flat::Next::Done => {
                let found_now = !std::mem::replace(&mut flat.calibrated, true);
                flat.exposure = Some(exposure);
                flat.gain = now.gain;
                flat.source = None;
                let record = self.flat_key(step);
                let flat_gain = now.gain.filter(|g| record.gain != Some(g.round() as i64));
                if let Some(g) = flat_gain.filter(|_| found_now) {
                    let step_gain = step_gain.or(record.gain.map(|g| g as f64)).map_or("unset".into(), |g| g.to_string());
                    self.log(
                        None,
                        format!("Flat step {}: gain {g} (the step's: {step_gain}) keeps the exposure at {}", step + 1, fmt_seconds(exposure)),
                        LogLevel::Info,
                    );
                }
                self.remember_flat(FlatRecord { exposure, flat_gain, ..record });
            }
            super::flat::Next::Try { exposure: e, gain: g } => {
                flat.calibrated = false;
                flat.source = None;
                if test {
                    flat.tests += 1;
                }
                if flat.tests > MAX_FLAT_TESTS {
                    let e = format!("could not find a flat exposure (last: {level} at {})", at(exposure, now.gain));
                    return self.sequence_failed(e);
                }
                flat.exposure = Some(e);
                flat.gain = g;
                let msg = format!("Flat at {} reached {level} (target {:.0} %), next {}", at(exposure, now.gain), target * 100.0, at(e, g));
                self.log(None, msg, LogLevel::Info);
            }
            super::flat::Next::Source { exposure: e, gain: g, brighter } => {
                flat.calibrated = false;
                flat.exposure = Some(e);
                flat.gain = g;
                let fix = if brighter { "too dark — brighten it" } else { "too bright — dim it" };
                let msg = format!("Flat source {fix} ({level} at {})", at(exposure, now.gain));
                match &mut flat.source {
                    None => {
                        flat.source = Some((std::time::Instant::now(), msg.clone()));
                        self.log(None, format!("{msg}; test frames go on until it fits"), LogLevel::Warning);
                    }
                    Some((since, _)) if since.elapsed() > FLAT_SOURCE_TIMEOUT => return self.sequence_failed(msg),
                    Some((_, text)) => *text = msg,
                }
            }
        }
        self.sequence_next();
    }

    /// Meridian flip and dither checks before a light frame.
    /// Returns true if the sequencer is now waiting for something.
    fn before_light(&mut self, step: &protocol::SequenceStep) -> bool {
        let run = self.run.as_ref().expect("running");
        if run.seq.meridian_flip
            && let Some(m) = self.mount_info()
            && m.pier_side.as_deref() == Some("WEST")
            && let Some(ha) = m.hour_angle
        {
            let limit = run.seq.flip_minutes_past / 60.0;
            let exposure_h = step.exposure.seconds().unwrap_or(0.0) / 3600.0;
            if ha >= limit {
                self.start_flip(m);
                return true;
            }
            // Don't start an exposure that would run past the flip point.
            if ha > -1.0 && ha + exposure_h * SIDEREAL_RATE > limit {
                let wait_s = (limit - ha) * 3600.0 / SIDEREAL_RATE;
                let run = self.run.as_mut().expect("running");
                run.wait = Wait::Meridian;
                run.message = format!("Waiting {:.0} min for the meridian flip", (wait_s / 60.0).ceil());
                let (id, tx) = (run.id, self.tx.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs_f64(wait_s.max(1.0))).await;
                    let _ = tx.send(CoreMsg::SequenceRecheck { run_id: id });
                });
                self.send_sequence();
                return true;
            }
        }

        // Autofocus.
        let run = self.run.as_ref().expect("running");
        let af = &run.seq.autofocus;
        let temperature_drift = af.temperature_delta.is_some_and(|d| {
            match (self.focuser_temperature(), self.last_focus_temperature) {
                (Some(now), Some(then)) => (now - then).abs() >= d,
                _ => false,
            }
        });
        let reason = if af.on_start && !run.focused_once {
            Some("start")
        } else if run.focus_after_flip {
            Some("meridian flip")
        } else if af.every_frames > 0 && run.lights_since_focus >= af.every_frames {
            Some("frame count")
        } else if af.on_filter_change && step.filter_slot.is_some() && step.filter_slot != run.last_focus_filter {
            Some("filter change")
        } else if temperature_drift {
            Some("temperature change")
        } else {
            None
        };
        if let Some(reason) = reason
            && self.state.roles.focuser.is_some()
        {
            let filter = step.filter_slot;
            let run = self.run.as_mut().expect("running");
            run.focus_after_flip = false;
            run.wait = Wait::Focus { filter };
            self.log(None, format!("Autofocus ({reason})"), LogLevel::Info);
            match self.start_autofocus(filter, true) {
                Ok(()) => {
                    self.set_sequence_message("Autofocus");
                    return true;
                }
                Err(e) => {
                    self.run.as_mut().expect("running").wait = Wait::None;
                    self.log(None, format!("Autofocus skipped: {e}"), LogLevel::Warning);
                }
            }
        }

        let run = self.run.as_ref().expect("running");
        if run.seq.dither_every > 0 && run.lights_since_dither >= run.seq.dither_every && self.guider_is_guiding() {
            self.run.as_mut().expect("running").lights_since_dither = 0;
            match self.guider_dither() {
                Ok(()) => {
                    let run = self.run.as_mut().expect("running");
                    run.wait = Wait::Settle;
                    run.message = "Dithering — waiting for guiding to settle".into();
                    self.send_sequence();
                    return true;
                }
                Err(e) => self.log(None, format!("Dither skipped: {e}"), LogLevel::Warning),
            }
        }
        false
    }

    fn start_flip(&mut self, m: protocol::MountInfo) {
        let restart_guiding = self.guider_is_guiding();
        self.log(
            None,
            format!(
                "Meridian flip: target is {:.0} min past the meridian",
                m.hour_angle.unwrap_or(0.0) * 60.0 / SIDEREAL_RATE
            ),
            LogLevel::Info,
        );
        let run = self.run.as_mut().expect("running");
        let target = run.seq.target;
        let center = run.seq.center_after_flip && target.is_some();
        run.wait = Wait::Flip {
            phase: if center {
                FlipPhase::Centering
            } else {
                FlipPhase::Slewing { seen_busy: false }
            },
            restart_guiding,
            pier_before: m.pier_side.clone(),
        };
        run.message = if center {
            "Meridian flip: slewing and centring".into()
        } else {
            "Meridian flip: slewing".into()
        };
        let (id, tx) = (run.id, self.tx.clone());
        self.guider_stop();
        // Slewing to the target (or the current position) makes a GEM flip.
        let (ra, dec) = target.map_or((m.ra_j2000, m.dec_j2000), |t| (t.ra_hours, t.dec_deg));
        let started = if center {
            self.start_solve(Some((ra, dec)), true)
        } else {
            self.mount_goto(ra, dec)
        };
        if let Err(e) = started {
            return self.sequence_failed(format!("meridian flip failed: {e}"));
        }
        tokio::spawn(async move {
            tokio::time::sleep(FLIP_TIMEOUT).await;
            let _ = tx.send(CoreMsg::FlipTimeout { run_id: id });
        });
        self.send_sequence();
    }

    fn flip_slew_done(&mut self) {
        let Some(Run {
            wait: Wait::Flip {
                restart_guiding,
                pier_before,
                ..
            },
            ..
        }) = &self.run
        else {
            return;
        };
        let (restart, before) = (*restart_guiding, pier_before.clone());
        let after = self.mount_info().and_then(|m| m.pier_side);
        if after == before {
            self.log(
                None,
                "Meridian flip: the mount did not change pier side — check its meridian limits",
                LogLevel::Warning,
            );
        }
        if restart {
            match self.guider_restart_after_flip() {
                Ok(()) => {
                    let run = self.run.as_mut().expect("running");
                    if let Wait::Flip { phase, .. } = &mut run.wait {
                        *phase = FlipPhase::GuideSettle;
                    }
                    run.message = "Meridian flip: restarting guiding".into();
                    self.send_sequence();
                    return;
                }
                Err(e) => self.log(None, format!("Could not restart guiding after the flip: {e}"), LogLevel::Warning),
            }
        }
        self.finish_flip();
    }

    fn finish_flip(&mut self) {
        if let Some(run) = &mut self.run {
            run.wait = Wait::None;
            run.focus_after_flip = run.seq.autofocus.after_flip;
        }
        self.log(None, "Meridian flip complete", LogLevel::Info);
        self.sequence_next();
    }

    /// Plate-solve centring requested by the sequencer finished.
    pub(super) fn sequence_center_done(&mut self, ok: bool) {
        let Some(run) = &mut self.run else {
            return;
        };
        match run.wait.clone() {
            Wait::CenterStart { restart_guiding } => {
                if !ok {
                    return self.sequence_failed("could not centre the target".into());
                }
                run.wait = Wait::None;
                if restart_guiding {
                    match self.guider_restart_after_flip() {
                        Ok(()) => {
                            let run = self.run.as_mut().expect("running");
                            run.wait = Wait::Settle;
                            run.message = "Restarting guiding".into();
                            self.send_sequence();
                            return;
                        }
                        Err(e) => self.log(None, format!("Could not restart guiding: {e}"), LogLevel::Warning),
                    }
                }
                self.sequence_next();
            }
            Wait::Flip {
                phase: FlipPhase::Centering,
                ..
            } => {
                if !ok {
                    self.log(None, "Re-centring after the flip failed — continuing", LogLevel::Warning);
                }
                self.flip_slew_done();
            }
            _ => {}
        }
    }

    /// Autofocus requested by the sequencer finished.
    pub(super) fn sequence_focus_done(&mut self, ok: bool) {
        let Some(run) = &mut self.run else {
            return;
        };
        let Wait::Focus { filter } = run.wait else {
            return;
        };
        run.wait = Wait::None;
        run.focused_once = true;
        run.lights_since_focus = 0;
        run.last_focus_filter = filter;
        if !ok {
            self.log(None, "Autofocus failed — continuing with the current focus", LogLevel::Warning);
        }
        self.sequence_next();
    }

    fn set_sequence_message(&mut self, message: &str) {
        if let Some(run) = &mut self.run {
            run.message = message.into();
        }
        self.send_sequence();
    }

    pub(super) fn sequence_flipping(&self) -> bool {
        self.run.as_ref().is_some_and(|r| matches!(r.wait, Wait::Flip { .. }))
    }

    /// The guider finished settling (after a dither or restart).
    pub(super) fn sequence_on_settled(&mut self, _outcome: SettleOutcome) {
        let Some(run) = &mut self.run else {
            return;
        };
        match run.wait {
            Wait::Settle => {
                run.wait = Wait::None;
                self.sequence_next();
            }
            Wait::Flip {
                phase: FlipPhase::GuideSettle,
                ..
            } => self.finish_flip(),
            _ => {}
        }
    }

    pub(super) fn sequence_recheck(&mut self, run_id: u64) {
        if let Some(run) = &mut self.run
            && run.id == run_id
            && run.wait == Wait::Meridian
        {
            run.wait = Wait::None;
            self.sequence_next();
        }
    }

    pub(super) fn flip_timeout(&mut self, run_id: u64) {
        if self.run.as_ref().is_some_and(|r| r.id == run_id && matches!(r.wait, Wait::Flip { .. })) {
            self.sequence_failed("meridian flip did not complete in 10 minutes".into());
        }
    }

    /// Property updates relevant to the sequencer (sensor temperature, flip slews).
    pub(super) fn sequence_on_update(&mut self, device: &str, name: &str) {
        let Some(run) = &self.run else {
            return;
        };
        if matches!(run.wait, Wait::Flip { phase: FlipPhase::Slewing { .. }, .. }) {
            if self.state.roles.mount.as_deref() == Some(device) && name == "EQUATORIAL_EOD_COORD" {
                let slewing = self.mount_info().is_some_and(|m| m.slewing);
                let mut done = false;
                if let Some(Run {
                    wait:
                        Wait::Flip {
                            phase: FlipPhase::Slewing { seen_busy },
                            ..
                        },
                    ..
                }) = &mut self.run
                {
                    if slewing {
                        *seen_busy = true;
                    } else if *seen_busy {
                        done = true;
                    }
                }
                if done {
                    self.flip_slew_done();
                }
            }
            return;
        }
        let run = self.run.as_ref().expect("checked");
        let Wait::Temperature { target, .. } = run.wait else {
            return;
        };
        if name != "CCD_TEMPERATURE" || self.state.roles.main_camera.as_deref() != Some(device) {
            return;
        }
        let Some(t) = self.camera_temperature() else {
            return;
        };
        let run = self.run.as_mut().expect("checked");
        if (t - target).abs() <= TEMPERATURE_TOLERANCE {
            run.wait = Wait::None;
            self.log(None, format!("Sensor at {t:.1} °C"), LogLevel::Info);
            self.sequence_next();
        } else {
            let msg = format!("Cooling to {target:.1} °C (now {t:.1} °C)");
            if run.message != msg {
                run.message = msg;
                self.send_sequence();
            }
        }
    }

    pub(super) fn temperature_timeout(&mut self, run_id: u64, step: usize) {
        if let Some(run) = &self.run
            && run.id == run_id
            && matches!(run.wait, Wait::Temperature { step: s, .. } if s == step)
        {
            self.sequence_failed("sensor did not reach the target temperature in 20 minutes".into());
        }
    }

    fn camera_temperature(&self) -> Option<f64> {
        let cache = self.client.as_ref()?.cache();
        let cam = cache.devices.get(self.state.roles.main_camera.as_ref()?)?;
        equipment::number(cam, "CCD_TEMPERATURE", "CCD_TEMPERATURE_VALUE").map(|n| n.value)
    }

    fn set_camera_temperature(&self, target: f64) -> Result<(), String> {
        let client = self.client.as_ref().ok_or("not connected")?;
        let camera = self.state.roles.main_camera.as_deref().ok_or("no main camera")?;
        let has_cooler = {
            let cache = client.cache();
            let cam = cache.devices.get(camera).ok_or("camera offline")?;
            if !cam.properties.contains_key("CCD_TEMPERATURE") {
                return Err(format!("'{camera}' has no temperature control"));
            }
            cam.properties.contains_key("CCD_COOLER")
        };
        if has_cooler {
            client
                .send_switch(camera, "CCD_COOLER", &[("COOLER_ON", true)])
                .map_err(|e| e.to_string())?;
        }
        client
            .send_number(camera, "CCD_TEMPERATURE", &[("CCD_TEMPERATURE_VALUE", target)])
            .map_err(|e| e.to_string())
    }

    fn camera_min_exposure(&self) -> f64 {
        let min = (|| {
            let cache = self.client.as_ref()?.cache();
            let cam = cache.devices.get(self.state.roles.main_camera.as_ref()?)?;
            equipment::number(cam, "CCD_EXPOSURE", "CCD_EXPOSURE_VALUE").map(|n| n.min)
        })();
        min.unwrap_or(0.0).max(0.0)
    }

    /// Filter/binning/gain identifying flats for `step` (exposure left at 0).
    fn flat_key(&self, step: usize) -> FlatRecord {
        let run = self.run.as_ref().expect("running");
        let s = &run.seq.steps[step];
        let (filter, cam_bin, cam_gain) = (|| {
            let cache = self.client.as_ref()?.cache();
            let filter = s.filter_slot.and_then(|slot| {
                let wheel = cache.devices.get(self.state.roles.filter_wheel.as_ref()?)?;
                equipment::filter_name(wheel, slot)
            });
            let cam = self.state.roles.main_camera.as_ref().and_then(|c| cache.devices.get(c));
            let bin = cam.and_then(|c| equipment::number(c, "CCD_BINNING", "HOR_BIN")).map(|n| n.value as u32);
            let gain = cam.and_then(|c| equipment::control_value(c, Control::Gain)).map(|n| n.value);
            Some((filter, bin, gain))
        })()
        .unwrap_or((None, None, None));
        FlatRecord {
            filter,
            binning: s.binning.or(cam_bin).unwrap_or(1),
            gain: s.gain.or(cam_gain).map(|g| g.round() as i64),
            exposure: 0.0,
            flat_gain: None,
        }
    }

    fn remember_flat(&mut self, record: FlatRecord) {
        let list = &mut self.state.flat_exposures;
        list.retain(|r| (&r.filter, r.binning, r.gain) != (&record.filter, record.binning, record.gain));
        list.push(record);
        self.save();
    }

    pub(super) fn sequence_msg(&self) -> ToUi {
        ToUi::Sequence {
            status: self.run.as_ref().map(|r| {
            Box::new(SequenceStatus {
                sequence: r.seq.clone(),
                state: if r.paused {
                    RunState::Paused
                } else if r.pause_requested {
                    RunState::Pausing
                } else {
                    RunState::Running
                },
                current_step: r.current,
                done: r.done.clone(),
                message: r.message.clone(),
            })
            }),
        }
    }

    /// A frame of `step` is kept: count it in the run and in the draft
    /// step it came from, so the progress survives aborts and restarts.
    fn count_frame(&mut self, step: usize, path: Option<String>) {
        let Some(run) = &mut self.run else {
            return;
        };
        let s = &mut run.seq.steps[step];
        let calibration = s.frame_type != FrameType::Light;
        if let (true, Some(path)) = (calibration, path) {
            s.frames.push(path);
        }
        run.done[step] += 1;
        let done = run.done[step];
        s.done = done;
        let s = s.clone();
        let draft = &mut self.state.sequence_draft.sequence;
        if let Some(d) = draft.steps.iter_mut().find(|d| d.id == s.id) {
            d.done = done;
            d.frames = s.frames.clone();
            self.send(self.doc_msg(DocId::Sequence, None));
            self.save_soon();
        }
        if calibration && done == s.count {
            self.stack_master(&s);
        }
    }

    fn send_sequence(&self) {
        self.send(self.sequence_msg());
    }
}

/// Next step to work on, or `None` when everything is done.
fn pick_step(run: &Run) -> Option<usize> {
    let steps = &run.seq.steps;
    let remaining = |i: usize| steps[i].enabled && run.done[i] < steps[i].count;
    // Finish finding a flat exposure before moving on, even when interleaving.
    if let Some(c) = run.current
        && remaining(c)
        && matches!(steps[c].exposure, Exposure::AutoFlat { .. })
        && !run.flat[c].calibrated
    {
        return Some(c);
    }
    match run.seq.order {
        SequenceOrder::ByStep => (0..steps.len()).find(|&i| remaining(i)),
        SequenceOrder::Interleaved => {
            let start = run.current.map_or(0, |c| c + 1);
            (0..steps.len())
                .map(|k| (start + k) % steps.len())
                .find(|&i| remaining(i))
        }
    }
}

pub(super) fn fmt_seconds(s: f64) -> String {
    if s >= 1.0 {
        format!("{:.1} s", s)
    } else {
        format!("{:.0} ms", s * 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SequenceStep;

    fn run(order: SequenceOrder, counts: &[u32]) -> Run {
        let steps: Vec<SequenceStep> = counts
            .iter()
            .map(|&count| SequenceStep {
                count,
                ..Default::default()
            })
            .collect();
        let n = steps.len();
        Run {
            id: 1,
            seq: Sequence {
                order,
                steps,
                ..Default::default()
            },
            done: vec![0; n],
            current: None,
            wait: Wait::None,
            pause_requested: false,
            paused: false,
            flat: vec![FlatState::default(); n],
            message: String::new(),
            lights_since_dither: 0,
            lights_since_focus: 0,
            focused_once: false,
            last_focus_filter: None,
            focus_after_flip: false,
            calibration_warned: Default::default(),
        }
    }

    /// Simulate picking and completing frames until done.
    fn order_of(mut r: Run) -> Vec<usize> {
        let mut out = Vec::new();
        while let Some(i) = pick_step(&r) {
            out.push(i);
            r.current = Some(i);
            r.done[i] += 1;
        }
        out
    }

    #[test]
    fn by_step_order() {
        assert_eq!(order_of(run(SequenceOrder::ByStep, &[2, 0, 1])), vec![0, 0, 2]);
    }

    #[test]
    fn interleaved_order() {
        assert_eq!(
            order_of(run(SequenceOrder::Interleaved, &[2, 1, 3])),
            vec![0, 1, 2, 0, 2, 2]
        );
    }

    #[test]
    fn disabled_steps_are_skipped() {
        let mut r = run(SequenceOrder::ByStep, &[1, 1]);
        r.seq.steps[0].enabled = false;
        assert_eq!(order_of(r), vec![1]);
    }

    #[test]
    fn uncalibrated_flat_keeps_focus_when_interleaving() {
        let mut r = run(SequenceOrder::Interleaved, &[2, 2]);
        r.seq.steps[0].exposure = Exposure::DEFAULT_FLAT;
        r.current = Some(0);
        assert_eq!(pick_step(&r), Some(0));
        r.flat[0].calibrated = true;
        assert_eq!(pick_step(&r), Some(1));
    }
}
