//! Calibration masters: the masters folder is read in the background (at
//! start, every minute, when the settings change and on request) and the
//! result sent to all UIs. Missing masters are made by running a one-step
//! sequence and averaging the frames it kept.

use std::path::PathBuf;
use std::time::Duration;

use protocol::{Exposure, FrameType, LogLevel, MasterKind, MastersStatus, Sequence, SequenceStep, ToUi};

use super::{Core, CoreMsg};

/// Re-read the folder this often, to notice masters added by hand.
pub(super) const RESCAN_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub(super) struct MastersScan {
    pub status: MastersStatus,
    running: bool,
    /// Asked for again while running.
    again: bool,
    /// What is being made, shown with the status.
    creating: Option<String>,
}

/// A sequence run taking a master's frames.
#[derive(Debug)]
pub(crate) struct MasterJob {
    kind: MasterKind,
    /// The kept frames, as saved.
    pub frames: Vec<PathBuf>,
}

/// Frames to average, at most.
const MAX_FRAMES: u32 = 500;

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

    pub(super) fn create_master(&mut self, kind: MasterKind, step: SequenceStep) -> Result<(), String> {
        let wanted = match kind {
            MasterKind::Dark => FrameType::Dark,
            MasterKind::Flat => FrameType::Flat,
            MasterKind::Bias => FrameType::Bias,
        };
        if step.frame_type != wanted {
            return Err(format!("a master {} needs {} frames", kind.label(), wanted.label().to_lowercase()));
        }
        if step.count < 1 || step.count > MAX_FRAMES {
            return Err(format!("take between 1 and {MAX_FRAMES} frames"));
        }
        if self.masters.creating.is_some() {
            return Err("a master is still being stacked; wait for it".into());
        }
        let what = match (&step.exposure, kind) {
            (Exposure::Fixed { seconds }, MasterKind::Dark) => format!("dark {}", super::sequence::fmt_seconds(*seconds)),
            _ => kind.label().to_owned(),
        };
        let seq = Sequence {
            name: format!("Master {what}"),
            steps: vec![SequenceStep {
                enabled: true,
                id: 0,
                done: 0,
                ..step
            }],
            target: None,
            center_on_start: false,
            meridian_flip: false,
            dither_every: 0,
            autofocus: Default::default(),
            ..Default::default()
        };
        self.start_sequence(seq)?;
        if let Some(run) = &mut self.run {
            run.master = Some(MasterJob { kind, frames: Vec::new() });
        }
        self.set_creating(Some(format!("Taking frames for the master {what}")));
        Ok(())
    }

    /// The master's frames are in: average them in the background.
    pub(super) fn stack_master(&mut self, job: MasterJob) {
        let MasterJob { kind, frames } = job;
        let n = frames.len();
        let text = format!("Stacking {n} {} frames", kind.label());
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
        self.set_creating(None);
        self.rescan_masters();
    }

    /// The master run ended early: nothing to stack.
    pub(super) fn master_abandoned(&mut self) {
        if self.masters.creating.is_some() {
            self.set_creating(None);
        }
    }
}
