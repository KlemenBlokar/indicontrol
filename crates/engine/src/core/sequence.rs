//! Sequencer: runs a list of steps through the capture machinery, handling
//! sensor temperature, flat auto-exposure and dark flats.

use std::time::Duration;

use protocol::equipment::{self, Control};
use protocol::{
    CaptureRequest, Exposure, FlatRecord, LogLevel, RunState, Sequence, SequenceOrder,
    SequenceStatus, ToUi,
};

use super::{Core, CoreMsg};

const TEMPERATURE_TOLERANCE: f64 = 0.5;
const TEMPERATURE_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Test exposures allowed to find a flat exposure before giving up.
const MAX_FLAT_TESTS: u32 = 10;

pub(crate) struct Run {
    id: u64,
    seq: Sequence,
    done: Vec<u32>,
    current: Option<usize>,
    wait: Wait,
    pause_requested: bool,
    paused: bool,
    flat: Vec<FlatState>,
    message: String,
}

#[derive(Debug, Clone, Default)]
struct FlatState {
    exposure: Option<f64>,
    calibrated: bool,
    tests: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Wait {
    None,
    Temperature { step: usize, target: f64 },
    Frame { step: usize, test: bool, exposure: f64 },
}

impl Core {
    pub(super) fn start_sequence(&mut self, seq: Sequence) -> Result<(), String> {
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
        if seq.steps.iter().any(|s| s.enabled && s.filter_slot.is_some())
            && self.state.roles.filter_wheel.is_none()
        {
            return Err("steps use filters but no filter wheel is assigned".into());
        }
        self.next_run_id += 1;
        let n = seq.steps.len();
        let name = if seq.name.is_empty() { "Sequence".into() } else { seq.name.clone() };
        self.run = Some(Run {
            id: self.next_run_id,
            seq,
            done: vec![0; n],
            current: None,
            wait: Wait::None,
            pause_requested: false,
            paused: false,
            flat: vec![FlatState::default(); n],
            message: String::new(),
        });
        self.log(None, format!("{name} started"), LogLevel::Info);
        self.sequence_next();
        Ok(())
    }

    pub(super) fn pause_sequence(&mut self) {
        let Some(run) = &mut self.run else {
            return;
        };
        if run.wait == Wait::None || matches!(run.wait, Wait::Temperature { .. }) {
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

        let (exposure, test) = match self.resolve_exposure(step) {
            Ok(v) => v,
            Err(e) => return self.sequence_failed(e),
        };
        let run = self.run.as_mut().expect("checked");
        let req = CaptureRequest {
            exposure,
            frame_type: s.frame_type,
            binning: s.binning,
            gain: s.gain,
            offset: s.offset,
            filter_slot: s.filter_slot,
            object: run.seq.object.clone(),
            save: !test,
            repeat: false,
        };
        run.wait = Wait::Frame { step, test, exposure };
        run.message = if test {
            format!("Flat step {}: test exposure {}", step + 1, fmt_seconds(exposure))
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
        if let Err(e) = self.start_capture(req, true) {
            self.sequence_failed(e);
        }
    }

    /// Exposure for the next frame of `step`, and whether it is a test frame.
    fn resolve_exposure(&self, step: usize) -> Result<(f64, bool), String> {
        let run = self.run.as_ref().expect("running");
        let s = &run.seq.steps[step];
        match &s.exposure {
            Exposure::Fixed { seconds } => Ok((*seconds, false)),
            Exposure::Minimum => Ok((self.camera_min_exposure(), false)),
            Exposure::MatchFlat => {
                let key = self.flat_key(step);
                self.state
                    .flat_exposures
                    .iter()
                    .find(|r| (r.filter.as_ref(), r.binning, r.gain) == (key.filter.as_ref(), key.binning, key.gain))
                    .map(|r| (r.exposure, false))
                    .ok_or_else(|| {
                        format!(
                            "no flat exposure known for {} — take flats first",
                            key.filter.as_deref().unwrap_or("this setup")
                        )
                    })
            }
            Exposure::AutoFlat { min, max, .. } => {
                let f = &run.flat[step];
                let exposure = f
                    .exposure
                    .or_else(|| {
                        let key = self.flat_key(step);
                        self.state
                            .flat_exposures
                            .iter()
                            .find(|r| (r.filter.as_ref(), r.binning, r.gain) == (key.filter.as_ref(), key.binning, key.gain))
                            .map(|r| r.exposure)
                    })
                    .unwrap_or(1.0)
                    .clamp(*min, *max);
                Ok((exposure, !f.calibrated))
            }
        }
    }

    /// Called by the capture code after each sequence frame.
    /// `brightest` is the highest channel median as a fraction of full scale.
    pub(super) fn sequence_frame_done(&mut self, brightest: f64) {
        let Some(run) = &mut self.run else {
            return;
        };
        let Wait::Frame { step, test, exposure } = run.wait else {
            return;
        };
        run.wait = Wait::None;
        let Exposure::AutoFlat {
            target,
            tolerance,
            min,
            max,
        } = run.seq.steps[step].exposure
        else {
            run.done[step] += 1;
            return self.sequence_next();
        };

        // Flats: keep frames that are close enough, and steer the exposure.
        let error = (brightest - target).abs();
        if !test && error <= 2.0 * tolerance {
            run.done[step] += 1;
        }
        let flat = &mut run.flat[step];
        if error <= tolerance {
            flat.calibrated = true;
            flat.exposure = Some(exposure);
            let record = self.flat_key(step);
            self.remember_flat(FlatRecord { exposure, ..record });
            return self.sequence_next();
        }
        flat.calibrated = false;
        if test {
            flat.tests += 1;
        }
        let next = if brightest >= 0.95 {
            exposure / 4.0
        } else if brightest <= 0.01 {
            exposure * 8.0
        } else {
            exposure * target / brightest
        };
        let level = format!("{:.0} %", brightest * 100.0);
        let failure = if next < min && exposure <= min {
            Some(format!("flat source too bright ({level} at {}) — dim it", fmt_seconds(min)))
        } else if next > max && exposure >= max {
            Some(format!("flat source too dark ({level} at {}) — brighten it", fmt_seconds(max)))
        } else if flat.tests > MAX_FLAT_TESTS {
            Some(format!("could not find a flat exposure (last: {level} at {})", fmt_seconds(exposure)))
        } else {
            None
        };
        if let Some(e) = failure {
            return self.sequence_failed(e);
        }
        flat.exposure = Some(next.clamp(min, max));
        self.log(
            None,
            format!(
                "Flat at {} reached {level} (target {:.0} %), next {}",
                fmt_seconds(exposure),
                target * 100.0,
                fmt_seconds(next.clamp(min, max))
            ),
            LogLevel::Info,
        );
        self.sequence_next();
    }

    /// Property updates relevant to the sequencer (sensor temperature).
    pub(super) fn sequence_on_update(&mut self, device: &str, name: &str) {
        let Some(run) = &self.run else {
            return;
        };
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

fn fmt_seconds(s: f64) -> String {
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
