//! Documents that all UIs edit together: forms and drafts that live on the
//! engine instead of in each UI.
//!
//! A UI edits its copy freely and sends the fields it changed as
//! [`ToEngine::EditDoc`](crate::ToEngine::EditDoc). The engine applies edits
//! in arrival order and sends the new document to every UI
//! ([`ToUi::Doc`](crate::ToUi::Doc)), saying which edit it includes. Each UI
//! shows the engine's document with its own not yet included edits on top
//! ([`SharedDoc`]), so typing never jumps back, and two people changing
//! different fields at once both keep their change.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{CatalogObject, FrameType, Sequence};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DocId {
    /// Imaging tab capture form.
    Capture,
    /// The sequence being edited in the Sequence tab.
    Sequence,
    /// Mount tab goto target.
    Goto,
    // Settings drafts, applied to the engine with their Apply button.
    ImagingSettings,
    GuiderSettings,
    SolverSettings,
    FocusSettings,
}

impl DocId {
    pub const ALL: [DocId; 7] = [
        DocId::Capture,
        DocId::Sequence,
        DocId::Goto,
        DocId::ImagingSettings,
        DocId::GuiderSettings,
        DocId::SolverSettings,
        DocId::FocusSettings,
    ];
}

/// Set the value at a JSON pointer (RFC 6901), e.g. `/steps/2/count`.
pub type Op = (String, Value);

/// Edit `seq` of UI session `session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub session: u64,
    pub seq: u64,
}

/// Operations turning `old` into `new`, as deep as possible: objects with the
/// same fields and arrays of the same length are compared element by element,
/// anything else is replaced whole.
pub fn diff(old: &Value, new: &Value) -> Vec<Op> {
    let mut ops = Vec::new();
    diff_at(old, new, &mut String::new(), &mut ops);
    ops
}

fn diff_at(old: &Value, new: &Value, path: &mut String, ops: &mut Vec<Op>) {
    if old == new {
        return;
    }
    let len = path.len();
    match (old, new) {
        (Value::Object(a), Value::Object(b)) if a.len() == b.len() && a.keys().all(|k| b.contains_key(k)) => {
            for (k, v) in b {
                path.push('/');
                path.push_str(&k.replace('~', "~0").replace('/', "~1"));
                diff_at(&a[k], v, path, ops);
                path.truncate(len);
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                path.push_str(&format!("/{i}"));
                diff_at(x, y, path, ops);
                path.truncate(len);
            }
        }
        _ => ops.push((path.clone(), new.clone())),
    }
}

/// Apply operations; those whose path no longer exists (someone removed
/// that sequence step meanwhile) are skipped.
pub fn apply(target: &mut Value, ops: &[Op]) {
    for (path, value) in ops {
        if let Some(slot) = target.pointer_mut(path) {
            *slot = value.clone();
        }
    }
}

/// The Imaging tab's capture form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureForm {
    pub exposure: f64,
    pub frame_type: FrameType,
    pub binning: u32,
    pub set_gain: bool,
    pub gain: f64,
    pub set_offset: bool,
    pub offset: f64,
    pub filter_slot: Option<u32>,
    pub object: String,
    pub save: bool,
    pub repeat: bool,
}

impl Default for CaptureForm {
    fn default() -> Self {
        Self {
            exposure: 1.0,
            frame_type: FrameType::Light,
            binning: 1,
            set_gain: false,
            gain: 0.0,
            set_offset: false,
            offset: 0.0,
            filter_slot: None,
            object: String::new(),
            save: true,
            repeat: false,
        }
    }
}

/// The sequence being edited, with its target as typed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SequenceDraft {
    pub sequence: Sequence,
    pub target_ra: String,
    pub target_dec: String,
}

/// The Mount tab's goto target, as typed or found.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GotoDraft {
    pub ra: String,
    pub dec: String,
    /// Last object found with the search.
    pub object: Option<CatalogObject>,
}

/// A UI's copy of a shared document.
#[derive(Debug, Clone)]
pub struct SharedDoc<T> {
    /// What the UI shows and edits.
    pub value: T,
    /// `value` when last synced, to find new local edits.
    synced: Value,
    /// Sent edits the engine has not included yet.
    pending: Vec<(u64, Vec<Op>)>,
    next_seq: u64,
    session: Option<u64>,
}

impl<T: Default + Serialize> Default for SharedDoc<T> {
    fn default() -> Self {
        let value = T::default();
        Self {
            synced: to_value(&value),
            value,
            pending: Vec::new(),
            next_seq: 0,
            session: None,
        }
    }
}

fn to_value(v: &impl Serialize) -> Value {
    serde_json::to_value(v).expect("documents always serialize")
}

/// Type-erased [`SharedDoc`], so a UI can sync all its documents alike.
pub trait Synced {
    /// A (re)connection to the engine as UI `session`: edits sent before are
    /// either in the documents the engine sends next, or lost.
    fn connected(&mut self, session: u64);
    fn on_server(&mut self, value: Value, ack: Option<Ack>);
    /// Local changes since the last call, to send as edit `seq`.
    fn edits(&mut self) -> Option<(u64, Vec<Op>)>;
}

impl<T: Serialize + DeserializeOwned> Synced for SharedDoc<T> {
    fn connected(&mut self, session: u64) {
        self.session = Some(session);
        self.pending.clear();
    }

    fn on_server(&mut self, value: Value, ack: Option<Ack>) {
        if let Some(ack) = ack
            && Some(ack.session) == self.session
        {
            self.pending.retain(|(seq, _)| *seq > ack.seq);
        }
        let mut shown = value;
        for (_, ops) in &self.pending {
            apply(&mut shown, ops);
        }
        // Unreadable (a newer engine?): keep what we have.
        if let Ok(v) = serde_json::from_value(shown.clone()) {
            self.value = v;
            self.synced = shown;
        }
    }

    fn edits(&mut self) -> Option<(u64, Vec<Op>)> {
        self.session?;
        let now = to_value(&self.value);
        let ops = diff(&self.synced, &now);
        if ops.is_empty() {
            return None;
        }
        self.next_seq += 1;
        self.pending.push((self.next_seq, ops.clone()));
        self.synced = now;
        Some((self.next_seq, ops))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diff_is_field_level() {
        let old = json!({"a": 1, "b": {"c": "x", "d": [1, 2]}, "e": {"Some": 1}});
        let new = json!({"a": 1, "b": {"c": "y", "d": [1, 3]}, "e": null});
        let ops = diff(&old, &new);
        assert_eq!(
            ops,
            [("/b/c".into(), json!("y")), ("/b/d/1".into(), json!(3)), ("/e".into(), json!(null))]
        );
        let mut v = old.clone();
        apply(&mut v, &ops);
        assert_eq!(v, new);
        // Different lengths or fields: replaced whole.
        assert_eq!(diff(&json!({"s": [1]}), &json!({"s": [1, 2]})), [("/s".into(), json!([1, 2]))]);
        assert_eq!(diff(&json!(1), &json!(2)), [(String::new(), json!(2))]);
        assert_eq!(diff(&json!({"a/b": 1}), &json!({"a/b": 2})), [("/a~1b".into(), json!(2))]);
    }

    #[test]
    fn missing_paths_are_skipped() {
        let mut v = json!({"steps": [{"n": 1}]});
        apply(&mut v, &[("/steps/3/n".into(), json!(5)), ("/steps/0/n".into(), json!(2))]);
        assert_eq!(v, json!({"steps": [{"n": 2}]}));
    }

    /// Two UIs and an engine that applies edits in order.
    struct Engine {
        doc: Value,
    }

    impl Engine {
        fn edit(&mut self, session: u64, (seq, ops): (u64, Vec<Op>)) -> (Value, Option<Ack>) {
            apply(&mut self.doc, &ops);
            (self.doc.clone(), Some(Ack { session, seq }))
        }
    }

    fn ui(session: u64) -> SharedDoc<CaptureForm> {
        let mut d = SharedDoc::<CaptureForm>::default();
        d.connected(session);
        d
    }

    #[test]
    fn concurrent_edits_of_different_fields_both_stick() {
        let mut engine = Engine { doc: to_value(&CaptureForm::default()) };
        let (mut a, mut b) = (ui(1), ui(2));
        a.value.exposure = 30.0;
        b.value.object = "M 31".into();
        let ea = a.edits().unwrap();
        let eb = b.edits().unwrap();
        assert_eq!(ea.1, [("/exposure".into(), json!(30.0))]);
        let ra = engine.edit(1, ea);
        let rb = engine.edit(2, eb);
        for (value, ack) in [ra, rb] {
            a.on_server(value.clone(), ack);
            b.on_server(value, ack);
        }
        for d in [&a, &b] {
            assert_eq!(d.value.exposure, 30.0);
            assert_eq!(d.value.object, "M 31");
        }
        assert!(a.edits().is_none() && b.edits().is_none());
    }

    #[test]
    fn own_unconfirmed_edits_survive_older_updates() {
        let mut engine = Engine { doc: to_value(&CaptureForm::default()) };
        let (mut a, mut b) = (ui(1), ui(2));
        // A types "M 3", then "M 31"; B's unrelated edit arrives in between.
        a.value.object = "M 3".into();
        let e1 = a.edits().unwrap();
        a.value.object = "M 31".into();
        let e2 = a.edits().unwrap();
        b.value.gain = 100.0;
        let rb = engine.edit(2, b.edits().unwrap());
        a.on_server(rb.0, rb.1);
        assert_eq!(a.value.object, "M 31", "pending edits stay on top");
        assert_eq!(a.value.gain, 100.0);
        let r1 = engine.edit(1, e1);
        a.on_server(r1.0, r1.1);
        assert_eq!(a.value.object, "M 31", "no jump back to the first keystroke");
        let r2 = engine.edit(1, e2);
        a.on_server(r2.0.clone(), r2.1);
        b.on_server(r2.0, r2.1);
        assert_eq!(b.value.object, "M 31");
        assert!(a.pending.is_empty());
    }

    #[test]
    fn nothing_is_sent_before_connecting() {
        let mut d = SharedDoc::<GotoDraft>::default();
        d.value.ra = "5:35".into();
        assert!(d.edits().is_none());
        d.connected(7);
        assert_eq!(d.edits().unwrap().1, [("/ra".into(), json!("5:35"))]);
    }
}
