//! Focuser control and V-curve autofocus.

use std::time::Duration;

use guiding::focus::{self, FocusPoint, Verdict};
use protocol::equipment;
use protocol::{AutofocusStatus, CaptureRequest, FrameType, LogLevel, StarStats, ToUi};

use super::capture::Purpose;
use super::{Core, CoreMsg};

const MOVE_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) struct FocusJob {
    id: u64,
    plan: Vec<i64>,
    index: usize,
    points: Vec<FocusPoint>,
    shifts: u32,
    start: i64,
    stage: Stage,
    epoch: u64,
    from_sequence: bool,
    filter_slot: Option<u32>,
    best: Option<(f64, f64)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    /// Moving to `target`, possibly via an overshoot first.
    Moving { target: i64, then: Option<i64>, purpose: MovePurpose },
    Exposing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MovePurpose {
    Sample,
    Best,
    Restore,
}

impl Core {
    pub(super) fn focusing(&self) -> bool {
        self.focus_job.is_some()
    }

    fn focuser(&self) -> Result<String, String> {
        let name = self.state.roles.focuser.clone().ok_or("assign a focuser in Equipment first")?;
        let cache = self.client.as_ref().ok_or("not connected to an INDI server")?.cache();
        let dev = cache
            .devices
            .get(&name)
            .filter(|d| d.is_connected())
            .ok_or_else(|| format!("focuser '{name}' is not connected"))?;
        if !dev.properties.contains_key("ABS_FOCUS_POSITION") {
            return Err(format!("'{name}' has no absolute position (ABS_FOCUS_POSITION)"));
        }
        Ok(name)
    }

    pub(super) fn focuser_position(&self) -> Option<i64> {
        let cache = self.client.as_ref()?.cache();
        let dev = cache.devices.get(self.state.roles.focuser.as_ref()?)?;
        equipment::number(dev, "ABS_FOCUS_POSITION", "FOCUS_ABSOLUTE_POSITION").map(|n| n.value.round() as i64)
    }

    pub(super) fn focuser_temperature(&self) -> Option<f64> {
        let cache = self.client.as_ref()?.cache();
        let dev = cache.devices.get(self.state.roles.focuser.as_ref()?)?;
        equipment::number(dev, "FOCUS_TEMPERATURE", "TEMPERATURE").map(|n| n.value)
    }

    pub(super) fn focuser_goto(&mut self, position: i64) -> Result<(), String> {
        let name = self.focuser()?;
        let (min, max) = {
            let cache = self.client.as_ref().expect("checked").cache();
            let n = equipment::number(cache.devices.get(&name).expect("checked"), "ABS_FOCUS_POSITION", "FOCUS_ABSOLUTE_POSITION")
                .cloned()
                .ok_or("no focuser position")?;
            (n.min, n.max)
        };
        if (max > min) && !((min..=max).contains(&(position as f64))) {
            return Err(format!("position {position} is outside the focuser range {min:.0}–{max:.0}"));
        }
        self.client
            .as_ref()
            .expect("checked")
            .send_number(&name, "ABS_FOCUS_POSITION", &[("FOCUS_ABSOLUTE_POSITION", position as f64)])
            .map_err(|e| e.to_string())
    }

    pub(super) fn focuser_move(&mut self, steps: i64) -> Result<(), String> {
        if self.focusing() {
            return Err("autofocus is running".into());
        }
        let pos = self.focuser_position().ok_or("focuser position unknown")?;
        self.focuser_goto(pos + steps)
    }

    pub(super) fn focuser_abort(&mut self) -> Result<(), String> {
        let name = self.focuser()?;
        self.client
            .as_ref()
            .expect("checked")
            .send_switch(&name, "FOCUS_ABORT_MOTION", &[("ABORT", true)])
            .map_err(|e| e.to_string())
    }

    pub(super) fn start_autofocus(&mut self, filter_slot: Option<u32>, from_sequence: bool) -> Result<(), String> {
        if self.focus_job.is_some() {
            return Err("autofocus is already running".into());
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
        self.focuser()?;
        let start = self.focuser_position().ok_or("focuser position unknown")?;
        let s = &self.state.focus;
        let plan = focus::plan(start, s.step.max(1) as i64, s.steps_out.max(1));
        self.next_focus_id += 1;
        self.focus_job = Some(FocusJob {
            id: self.next_focus_id,
            plan,
            index: 0,
            points: Vec::new(),
            shifts: 0,
            start,
            stage: Stage::Exposing,
            epoch: 0,
            from_sequence,
            filter_slot: filter_slot.or(self.state.focus.filter_slot),
            best: None,
        });
        self.focus_status = AutofocusStatus {
            running: true,
            message: format!("Autofocus around {start}"),
            ..Default::default()
        };
        self.log(None, format!("Autofocus started at {start}"), LogLevel::Info);
        let first = self.focus_job.as_ref().expect("created").plan[0];
        self.focus_move(first, true, MovePurpose::Sample)
    }

    /// Move the focuser; an outward move overshoots by the backlash first so
    /// the final approach is always inward.
    fn focus_move(&mut self, target: i64, from_outside: bool, purpose: MovePurpose) -> Result<(), String> {
        let backlash = self.state.focus.backlash as i64;
        let current = self.focuser_position().unwrap_or(target);
        let (first, then) = if backlash > 0 && (from_outside || target > current) {
            (target + backlash, Some(target))
        } else {
            (target, None)
        };
        let job = self.focus_job.as_mut().ok_or("autofocus not running")?;
        job.stage = Stage::Moving { target: first, then, purpose };
        job.epoch += 1;
        let (id, epoch, tx) = (job.id, job.epoch, self.tx.clone());
        tokio::spawn(async move {
            tokio::time::sleep(MOVE_TIMEOUT).await;
            let _ = tx.send(CoreMsg::FocusTimeout { id, epoch });
        });
        if current == first {
            self.focus_arrived();
            return Ok(());
        }
        self.focuser_goto(first)
    }

    pub(super) fn focus_timeout(&mut self, id: u64, epoch: u64) {
        if self
            .focus_job
            .as_ref()
            .is_some_and(|j| j.id == id && j.epoch == epoch && matches!(j.stage, Stage::Moving { .. }))
        {
            self.focus_finish(false, "the focuser did not reach its position".into());
        }
    }

    /// Focuser property updates.
    pub(super) fn focus_on_update(&mut self, device: &str, name: &str) {
        if name != "ABS_FOCUS_POSITION" || self.state.roles.focuser.as_deref() != Some(device) {
            return;
        }
        let Some(Stage::Moving { target, .. }) = self.focus_job.as_ref().map(|j| j.stage) else {
            return;
        };
        let Some(prop) = self.property(device, name) else {
            return;
        };
        let at = prop.number("FOCUS_ABSOLUTE_POSITION").map(|v| v.round() as i64);
        match prop.state {
            indi_link::PropertyState::Alert => self.focus_finish(false, "the focuser reported an error".into()),
            indi_link::PropertyState::Busy => {}
            _ if at == Some(target) => self.focus_arrived(),
            _ => {}
        }
    }

    fn focus_arrived(&mut self) {
        let Some(job) = &mut self.focus_job else {
            return;
        };
        let Stage::Moving { then, purpose, .. } = job.stage else {
            return;
        };
        if let Some(next) = then {
            job.stage = Stage::Moving { target: next, then: None, purpose };
            if let Err(e) = self.focuser_goto(next) {
                self.focus_finish(false, e);
            }
            return;
        }
        match purpose {
            MovePurpose::Sample => {
                let s = self.state.focus.clone();
                let job = self.focus_job.as_mut().expect("checked");
                job.stage = Stage::Exposing;
                let req = CaptureRequest {
                    exposure: s.exposure,
                    frame_type: FrameType::Light,
                    binning: s.binning,
                    gain: s.gain,
                    offset: None,
                    filter_slot: job.filter_slot,
                    object: String::new(),
                    save: false,
                    repeat: false,
                };
                if let Err(e) = self.start_capture(req, Purpose::Focus) {
                    self.focus_finish(false, e);
                }
            }
            MovePurpose::Best => {
                let (best, hfr) = self.focus_job.as_ref().and_then(|j| j.best).unwrap_or_default();
                self.focus_finish(true, format!("Focus at {best:.0} (HFR {hfr:.2} px)"));
            }
            MovePurpose::Restore => {
                self.focus_finish(false, self.focus_status.message.clone());
            }
        }
    }

    pub(super) fn focus_frame_done(&mut self, stars: Option<StarStats>) {
        let min_stars = self.state.focus.min_stars;
        let Some(job) = &mut self.focus_job else {
            return;
        };
        let position = job.plan[job.index];
        match stars {
            Some(s) if s.count >= min_stars => {
                job.points.push(FocusPoint { position: position as f64, hfr: s.hfr });
                self.focus_status.points.push((position as f64, s.hfr));
                self.focus_status.message = format!("{position}: HFR {:.2} px ({} stars)", s.hfr, s.count);
            }
            other => {
                self.focus_status.message = format!(
                    "{position}: only {} stars — skipped",
                    other.map_or(0, |s| s.count)
                );
            }
        }
        self.send_focus();
        let job = self.focus_job.as_mut().expect("checked");
        job.index += 1;
        if job.index < job.plan.len() {
            let next = job.plan[job.index];
            if let Err(e) = self.focus_move(next, false, MovePurpose::Sample) {
                self.focus_finish(false, e);
            }
            return;
        }
        self.focus_evaluate();
    }

    fn focus_evaluate(&mut self) {
        let max_shifts = self.state.focus.max_shifts;
        let (step, steps_out) = (self.state.focus.step.max(1) as i64, self.state.focus.steps_out.max(1));
        let job = self.focus_job.as_mut().expect("running");
        let range = (job.plan[0], *job.plan.last().expect("non-empty plan"));
        if let Some(f) = focus::fit(&job.points) {
            let lo = job.points.iter().map(|p| p.position).fold(f64::INFINITY, f64::min);
            let hi = job.points.iter().map(|p| p.position).fold(f64::NEG_INFINITY, f64::max);
            self.focus_status.curve = f.curve(lo, hi, 60);
        }
        let all_range = (
            job.points.iter().map(|p| p.position as i64).max().unwrap_or(range.0),
            job.points.iter().map(|p| p.position as i64).min().unwrap_or(range.1),
        );
        match focus::evaluate(&job.points, all_range) {
            Verdict::Done { best, min_hfr } => {
                job.best = Some((best, min_hfr));
                self.focus_status.best = Some(best);
                self.focus_status.message = format!("Best focus {best:.0}, moving there");
                self.send_focus();
                let target = best.round() as i64;
                if let Err(e) = self.focus_move(target, true, MovePurpose::Best) {
                    self.focus_finish(false, e);
                }
            }
            Verdict::Shift { center } if job.shifts < max_shifts => {
                job.shifts += 1;
                // Sample only the new side, keeping the points already measured.
                let measured: Vec<i64> = job.points.iter().map(|p| p.position as i64).collect();
                job.plan = focus::plan(center, step, steps_out)
                    .into_iter()
                    .filter(|p| !measured.contains(p))
                    .collect();
                job.index = 0;
                let first = job.plan.first().copied();
                self.log(None, format!("Focus is beyond the sampled range — extending around {center}"), LogLevel::Info);
                let Some(first) = first else {
                    return self.focus_restore("no new positions to sample".into());
                };
                if let Err(e) = self.focus_move(first, true, MovePurpose::Sample) {
                    self.focus_finish(false, e);
                }
            }
            Verdict::Shift { .. } => self.focus_restore("focus not found within the search range".into()),
            Verdict::Failed(e) => self.focus_restore(e),
        }
    }

    /// Give up: go back to where autofocus started.
    fn focus_restore(&mut self, reason: String) {
        let start = self.focus_job.as_ref().map(|j| j.start).unwrap_or_default();
        self.focus_status.message = format!("Autofocus failed: {reason}");
        if let Err(e) = self.focus_move(start, true, MovePurpose::Restore) {
            self.focus_finish(false, format!("Autofocus failed: {reason}; {e}"));
        }
    }

    pub(super) fn focus_failed(&mut self, reason: String) {
        if self.focus_job.is_some() {
            self.focus_restore(reason);
        }
    }

    pub(super) fn abort_autofocus(&mut self) {
        if self.focus_job.is_some() {
            if self.job.is_some() {
                self.abort_capture();
            }
            let _ = self.focuser_abort();
            self.focus_finish(false, "Autofocus aborted".into());
        }
    }

    fn focus_finish(&mut self, ok: bool, message: String) {
        let Some(job) = self.focus_job.take() else {
            return;
        };
        self.log(None, message.clone(), if ok { LogLevel::Info } else { LogLevel::Error });
        self.focus_status.running = false;
        self.focus_status.message = message;
        self.send_focus();
        if ok {
            self.last_focus_temperature = self.focuser_temperature();
        }
        if job.from_sequence {
            self.sequence_focus_done(ok);
        }
    }

    pub(super) fn focus_msg(&self) -> ToUi {
        ToUi::Autofocus {
            status: self.focus_status.clone(),
        }
    }

    fn send_focus(&self) {
        self.send(self.focus_msg());
    }
}
