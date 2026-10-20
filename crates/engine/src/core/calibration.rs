//! Calibration masters: the masters folder is read in the background (at
//! start, every minute, when the settings change and on request) and the
//! result sent to all UIs.

use std::path::PathBuf;
use std::time::Duration;

use protocol::{MastersStatus, ToUi};

use super::{Core, CoreMsg};

/// Re-read the folder this often, to notice masters added by hand.
pub(super) const RESCAN_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub(super) struct MastersScan {
    pub status: MastersStatus,
    running: bool,
    /// Asked for again while running.
    again: bool,
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
        ToUi::Masters(Box::new(self.masters.status.clone()))
    }
}
