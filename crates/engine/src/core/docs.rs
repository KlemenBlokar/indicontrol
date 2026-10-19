//! Shared documents (see `protocol::doc`): edited by all UIs, applied here
//! in arrival order and sent back to everyone.

use std::time::Duration;

use protocol::doc::{self, Ack, DocId, Op};
use protocol::{FocusSettings, GuiderSettings, ImagingSettings, LogLevel, SolverSettings, ToUi};
use serde_json::Value;

use super::{Core, CoreMsg};

/// Edits come in per keystroke or drag step; save them at most this often.
const SAVE_DELAY: Duration = Duration::from_secs(2);

/// Settings as being edited, until someone presses Apply.
#[derive(Debug, Clone, Default)]
pub(super) struct SettingsDrafts {
    imaging: ImagingSettings,
    guider: GuiderSettings,
    solver: SolverSettings,
    focus: FocusSettings,
}

impl Core {
    pub(super) fn init_drafts(&mut self) {
        self.drafts = SettingsDrafts {
            imaging: self.state.imaging.clone(),
            guider: self.state.guider.clone(),
            solver: self.state.solver.clone(),
            focus: self.state.focus.clone(),
        };
    }

    pub(super) fn doc_msg(&self, doc: DocId, ack: Option<Ack>) -> ToUi {
        ToUi::Doc { doc, value: self.doc_value(doc), ack }
    }

    fn doc_value(&self, doc: DocId) -> Value {
        match doc {
            DocId::Capture => to_value(&self.state.capture),
            DocId::Sequence => to_value(&self.state.sequence_draft),
            DocId::Goto => to_value(&self.state.goto),
            DocId::ImagingSettings => to_value(&self.drafts.imaging),
            DocId::GuiderSettings => to_value(&self.drafts.guider),
            DocId::SolverSettings => to_value(&self.drafts.solver),
            DocId::FocusSettings => to_value(&self.drafts.focus),
        }
    }

    pub(super) fn edit_doc(&mut self, session: u64, doc: DocId, seq: u64, ops: Vec<Op>) {
        let mut value = self.doc_value(doc);
        doc::apply(&mut value, &ops);
        if let Err(e) = self.set_doc(doc, value) {
            self.log(None, format!("Ignored an edit of {doc:?}: {e}"), LogLevel::Warning);
        }
        // Also when ignored: the sender learns the edit is done with.
        self.send(self.doc_msg(doc, Some(Ack { session, seq })));
    }

    fn set_doc(&mut self, doc: DocId, value: Value) -> Result<(), serde_json::Error> {
        match doc {
            DocId::Capture => self.state.capture = serde_json::from_value(value)?,
            DocId::Sequence => self.state.sequence_draft = serde_json::from_value(value)?,
            DocId::Goto => self.state.goto = serde_json::from_value(value)?,
            DocId::ImagingSettings => self.drafts.imaging = serde_json::from_value(value)?,
            DocId::GuiderSettings => self.drafts.guider = serde_json::from_value(value)?,
            DocId::SolverSettings => self.drafts.solver = serde_json::from_value(value)?,
            DocId::FocusSettings => self.drafts.focus = serde_json::from_value(value)?,
        }
        // Settings drafts are dropped on restart; the rest is saved.
        if matches!(doc, DocId::Capture | DocId::Sequence | DocId::Goto) {
            self.save_soon();
        }
        Ok(())
    }

    /// Applied settings changed (Apply, profile load): drafts start over from them.
    pub(super) fn reset_drafts(&mut self) {
        let old = self.drafts.clone();
        self.init_drafts();
        let changed = [
            (DocId::ImagingSettings, old.imaging != self.drafts.imaging),
            (DocId::GuiderSettings, old.guider != self.drafts.guider),
            (DocId::SolverSettings, old.solver != self.drafts.solver),
            (DocId::FocusSettings, old.focus != self.drafts.focus),
        ];
        for (doc, changed) in changed {
            if changed {
                self.send(self.doc_msg(doc, None));
            }
        }
    }

    fn save_soon(&mut self) {
        if self.save_pending {
            return;
        }
        self.save_pending = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SAVE_DELAY).await;
            let _ = tx.send(CoreMsg::Save);
        });
    }

    pub(super) fn save_now(&mut self) {
        self.save_pending = false;
        self.save();
    }
}

fn to_value(v: &impl serde::Serialize) -> Value {
    serde_json::to_value(v).expect("documents always serialize")
}
