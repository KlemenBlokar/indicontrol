//! Calibration masters: the masters folder is read in the background (at
//! start, every minute, when the settings change and on request) and the
//! result sent to all UIs. Each completed dark, bias or flat step of a
//! sequence is averaged into a master there.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use protocol::{FlatCalibration, FrameType, LightSpec, LogLevel, MasterKind, MastersStatus, SequenceStep, ToUi};

use crate::calibrate;
use crate::metadata::FrameContext;

use super::{Core, CoreMsg};

/// Re-read the folder this often, to notice masters added by hand.
pub(super) const RESCAN_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub(super) struct MastersScan {
    pub status: MastersStatus,
    running: bool,
    /// Asked for again while running.
    again: bool,
    /// What is being stacked, shown with the status.
    creating: Option<String>,
    /// Completed steps waiting for their turn.
    queue: VecDeque<(MasterKind, Vec<PathBuf>)>,
}

impl Core {
    pub(super) fn masters_folder(&self) -> PathBuf {
        let s = &self.state.imaging;
        if s.masters_dir.trim().is_empty() {
            PathBuf::from(&s.output_dir).join("masters")
        } else {
            PathBuf::from(s.masters_dir.trim())
        }
    }

    pub(super) fn rescan_masters(&mut self) {
        if self.masters.running {
            self.masters.again = true;
            return;
        }
        self.masters.running = true;
        let folder = self.masters_folder();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let status = crate::masters::scan(&folder);
            let _ = tx.send(CoreMsg::MastersScanned(Box::new(status)));
        });
    }

    pub(super) fn masters_scanned(&mut self, status: MastersStatus) {
        self.masters.running = false;
        if status != self.masters.status {
            self.masters.status = status;
            self.send(self.masters_msg());
        }
        if std::mem::take(&mut self.masters.again) {
            self.rescan_masters();
        }
    }

    pub(super) fn masters_msg(&self) -> ToUi {
        ToUi::Masters(Box::new(MastersStatus {
            creating: self.masters.creating.clone(),
            ..self.masters.status.clone()
        }))
    }

    fn set_creating(&mut self, what: Option<String>) {
        self.masters.creating = what;
        self.send(self.masters_msg());
    }

    /// `step` has all its frames: average them into a master, after the
    /// ones already waiting.
    pub(super) fn stack_master(&mut self, step: &SequenceStep) {
        let kind = match step.frame_type {
            FrameType::Dark => MasterKind::Dark,
            FrameType::Bias => MasterKind::Bias,
            FrameType::Flat => MasterKind::Flat,
            FrameType::Light => return,
        };
        let frames: Vec<PathBuf> = step.frames.iter().map(PathBuf::from).collect();
        if frames.is_empty() {
            return;
        }
        if (frames.len() as u32) < step.count {
            let text = format!(
                "Only {} of the {} {} frames are known (taken before they were recorded); stacking those",
                frames.len(),
                step.count,
                kind.label()
            );
            self.log(None, text, LogLevel::Warning);
        }
        self.masters.queue.push_back((kind, frames));
        if self.masters.creating.is_none() {
            self.stack_next();
        }
    }

    fn stack_next(&mut self) {
        let Some((kind, frames)) = self.masters.queue.pop_front() else {
            self.set_creating(None);
            return;
        };
        let text = format!("Stacking {} {} frames", frames.len(), kind.label());
        self.log(None, format!("{text} into a master"), LogLevel::Info);
        self.set_creating(Some(text));
        let folder = self.masters_folder();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = crate::stack::stack(&frames, kind, &folder);
            let _ = tx.send(CoreMsg::MasterStacked(Box::new(result)));
        });
    }

    pub(super) fn master_stacked(&mut self, result: Result<crate::stack::Stacked, String>) {
        match result {
            Ok(s) => {
                let text = format!(
                    "Master saved: {} ({} frames, {:.2} % of values rejected)",
                    s.path.display(),
                    s.frames,
                    s.rejected * 100.0
                );
                self.log(None, text, LogLevel::Info);
            }
            Err(e) => self.log(None, format!("Could not make the master: {e}"), LogLevel::Error),
        }
        self.rescan_masters();
        self.stack_next();
    }

    /// The masters a sequence light taken as `ctx` gets under the running
    /// sequence's plan; reports missing ones once per run. `None` when it
    /// gets none.
    pub(super) fn calibration_for(&mut self, ctx: &FrameContext) -> Option<calibrate::Plan> {
        let plan = self.run.as_ref()?.seq.calibration;
        if !plan.darks && !plan.flats && !plan.save_calibrated {
            return None;
        }
        let light = LightSpec {
            camera: Some(ctx.camera.clone()),
            size: None,
            binning: ctx.binning.map_or(1, |(h, _)| h.max(1)),
            exposure: ctx.exposure,
            gain: ctx.gain,
            offset: ctx.offset,
            temperature: ctx.temperature,
            filter: ctx.filter.clone(),
        };
        let folder = self.masters_folder();
        let chosen = protocol::masters::choose(&light, plan, &self.masters.status.masters);
        let path = |m: Option<&protocol::MasterFrame>| m.map(|m| folder.join(&m.path));
        let mut label = Vec::new();
        if chosen.lights.is_some() {
            label.push(if plan.darks { "dark" } else { "bias" }.to_owned());
        }
        if chosen.flat.is_some() {
            label.push(match (chosen.flat_cal.is_some(), plan.flats_with) {
                (false, _) => "flat".to_owned(),
                (true, FlatCalibration::Bias) => "flat (less bias)".to_owned(),
                (true, FlatCalibration::DarkFlats) => "flat (less dark flat)".to_owned(),
            });
        }
        let out = calibrate::Plan {
            dark: path(chosen.lights),
            flat: path(chosen.flat),
            flat_cal: path(chosen.flat_cal),
            save_raw: plan.save_raw,
            save_calibrated: plan.save_calibrated,
            label: label.join(", "),
        };
        let missing = chosen.missing;
        let fresh: Vec<String> = {
            let run = self.run.as_mut()?;
            missing.iter().filter(|m| run.calibration_warned.insert((*m).clone())).cloned().collect()
        };
        for m in fresh {
            self.log(None, format!("No master {m}: the lights are not corrected for it"), LogLevel::Warning);
        }
        (!out.is_empty()).then_some(out)
    }
}
