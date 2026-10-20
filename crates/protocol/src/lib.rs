//! Messages exchanged between the indicontrol engine and its UIs.
//!
//! Transport is a WebSocket carrying one JSON message per text frame.
//! (Binary frames are reserved for image previews.) On connect, the engine
//! sends [`ToUi::Hello`] followed by a full state dump; afterwards it sends
//! incremental updates. UIs mirror state with [`UiState::apply`].

use std::collections::{HashMap, VecDeque};

use indi_link::{Cache, Device, Property};
use serde::{Deserialize, Serialize};

pub mod catalog;
pub mod doc;
pub mod equipment;
pub mod focus;
pub mod guider;
pub mod masters;
pub mod mount;
pub mod sequence;
pub mod solver;

pub use catalog::CatalogObject;
pub use doc::{Ack, CaptureForm, DocId, GotoDraft, SequenceDraft, SharedDoc, Synced};
pub use guider::{GuideOutput, GuideStep, GuiderSettings, GuiderState, GuiderStatus};
pub use imaging::{Analysis, ChannelStats};
pub use focus::{AutofocusStatus, AutofocusTriggers, FocusSettings, StarStats};
pub use masters::{CalibrationPlan, Chosen, FlatCalibration, LightSpec, MasterFrame, MasterKind, MastersStatus, Need, Purpose};
pub use mount::MountInfo;
pub use solver::{SolveResult, SolveStatus, SolverKind, SolverSettings, Target};
pub use sequence::{
    Exposure, FlatRecord, RunState, Sequence, SequenceOrder, SequenceStatus, SequenceStep, Setup,
};
pub use indi_link;

/// Bump on any incompatible message change.
pub const PROTOCOL_VERSION: u32 = 7;
pub const DEFAULT_ENGINE_PORT: u16 = 7700;
pub const WS_PATH: &str = "/ws";

/// `url` with this UI's protocol version (`?protocol=N`), which the engine
/// requires: it turns away UIs that speak another version.
pub fn versioned_url(url: &str) -> String {
    if url.contains("protocol=") {
        return url.to_owned();
    }
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}protocol={PROTOCOL_VERSION}")
}

/// Parse `host`, `host:port` or `[v6]:port`.
pub fn parse_host_port(s: &str, default_port: u16) -> (String, u16) {
    let s = s.trim();
    if let Some((host, port)) = s.rsplit_once(':')
        && let Ok(port) = port.parse()
        && !host.ends_with(':')
    {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        return (host.to_owned(), port);
    }
    (s.to_owned(), default_port)
}

/// Turn user input (`pi.local`, `pi.local:7701`, `ws://…`) into an engine WebSocket URL.
pub fn engine_url(input: &str) -> String {
    let input = input.trim();
    if input.starts_with("ws://") || input.starts_with("wss://") {
        return input.to_owned();
    }
    let (host, port) = parse_host_port(input, DEFAULT_ENGINE_PORT);
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    format!("ws://{host}:{port}{WS_PATH}")
}

/// Telescope optics. INDI 2.x keeps these on each camera (`SCOPE_INFO`); the
/// engine pushes them there so metadata, plate solving and guiding know them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Optics {
    /// Main telescope focal length (mm), including reducers/barlows.
    pub focal_length: Option<f64>,
    pub aperture: Option<f64>,
    pub guide_focal_length: Option<f64>,
    pub guide_aperture: Option<f64>,
}

/// Which INDI device fills each equipment role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Roles {
    pub main_camera: Option<String>,
    pub guide_camera: Option<String>,
    pub mount: Option<String>,
    pub filter_wheel: Option<String>,
    pub focuser: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum IndiStatus {
    Disconnected { error: Option<String> },
    Connecting,
    Connected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Local time at the engine, `HH:MM:SS`.
    pub time: String,
    pub device: Option<String>,
    pub text: String,
    pub level: LogLevel,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FrameType {
    #[default]
    Light,
    Dark,
    Bias,
    Flat,
}

impl FrameType {
    pub const ALL: [FrameType; 4] = [Self::Light, Self::Dark, Self::Bias, Self::Flat];

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::Bias => "Bias",
            Self::Flat => "Flat",
        }
    }

    /// FITS `IMAGETYP` value understood by Siril, PixInsight, etc.
    pub fn imagetyp(self) -> &'static str {
        match self {
            Self::Light => "Light Frame",
            Self::Dark => "Dark Frame",
            Self::Bias => "Bias Frame",
            Self::Flat => "Flat Frame",
        }
    }

    /// Element of the INDI `CCD_FRAME_TYPE` switch.
    pub fn indi_switch(self) -> &'static str {
        match self {
            Self::Light => "FRAME_LIGHT",
            Self::Dark => "FRAME_DARK",
            Self::Bias => "FRAME_BIAS",
            Self::Flat => "FRAME_FLAT",
        }
    }
}

/// One exposure (or a loop of them) on the main camera.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureRequest {
    pub exposure: f64,
    pub frame_type: FrameType,
    /// `None` leaves the camera setting unchanged.
    pub binning: Option<u32>,
    pub gain: Option<f64>,
    pub offset: Option<f64>,
    /// 1-based filter wheel slot.
    pub filter_slot: Option<u32>,
    pub object: String,
    pub save: bool,
    /// Keep exposing until aborted (framing/focusing).
    pub repeat: bool,
}

impl Default for CaptureRequest {
    fn default() -> Self {
        Self {
            exposure: 1.0,
            frame_type: FrameType::Light,
            binning: None,
            gain: None,
            offset: None,
            filter_slot: None,
            object: String::new(),
            save: true,
            repeat: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum CaptureState {
    Idle,
    /// Waiting for equipment (e.g. the filter wheel) before exposing.
    Preparing { what: String },
    Exposing { total: f64, remaining: f64 },
    Downloading,
    Processing,
}

pub const DEFAULT_FILE_TEMPLATE: &str =
    "{object}/{date}/{type}/{filter}/{object}_{type}_{filter}_{exposure}s_G{gain}_O{offset}_{temp}C_bin{bin}_{seq:04}";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImagingSettings {
    /// Root directory for saved frames (on the engine's machine).
    pub output_dir: String,
    /// Path below `output_dir`, without extension. Tokens: {object} {type}
    /// {filter} {exposure} {gain} {offset} {temp} {bin} {camera} {date}
    /// (night of the session) {time} (UTC) {seq} / {seq:04}.
    pub file_template: String,
    /// Folder with calibration masters (on the engine's machine); empty:
    /// `masters` in `output_dir`.
    pub masters_dir: String,
}

impl Default for ImagingSettings {
    fn default() -> Self {
        Self {
            output_dir: String::new(),
            file_template: DEFAULT_FILE_TEMPLATE.into(),
            masters_dir: String::new(),
        }
    }
}

/// A processed frame. The stretched preview follows as a binary message
/// (see [`preview_frame`]) with the same `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameInfo {
    pub id: u64,
    pub camera: String,
    pub frame_type: FrameType,
    pub exposure: f64,
    pub filter: Option<String>,
    pub object: String,
    pub width: usize,
    pub height: usize,
    /// Where the FITS file was written, if saved (the calibrated copy when
    /// only that is kept).
    pub path: Option<String>,
    /// The calibrated copy, if written.
    #[serde(default)]
    pub calibrated_path: Option<String>,
    /// Masters applied to the frame (and its preview): "dark, flat".
    #[serde(default)]
    pub calibration: Option<String>,
    pub analysis: Analysis,
    /// Detected stars and their median size.
    pub stars: Option<StarStats>,
    /// Final FITS header as (keyword, value) pairs.
    pub header: Vec<(String, String)>,
}

/// Binary preview messages: 4-byte magic (`ICPV` main camera, `ICGV` guide
/// camera), frame id (u64 LE), JPEG bytes.
pub mod preview_frame {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Kind {
        Main,
        Guide,
    }

    impl Kind {
        fn magic(self) -> &'static [u8; 4] {
            match self {
                Kind::Main => b"ICPV",
                Kind::Guide => b"ICGV",
            }
        }
    }

    pub fn encode(kind: Kind, frame_id: u64, jpeg: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + jpeg.len());
        out.extend_from_slice(kind.magic());
        out.extend_from_slice(&frame_id.to_le_bytes());
        out.extend_from_slice(jpeg);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<(Kind, u64, &[u8])> {
        let kind = [Kind::Main, Kind::Guide]
            .into_iter()
            .find(|k| bytes.starts_with(k.magic()))?;
        let rest = &bytes[4..];
        let id = u64::from_le_bytes(rest.get(..8)?.try_into().ok()?);
        Some((kind, id, &rest[8..]))
    }
}

/// Engine → UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToUi {
    Hello {
        protocol: u32,
        engine_version: String,
        /// This UI's session, as named in [`Ack`]s.
        session: u64,
    },
    IndiStatus {
        host: String,
        port: u16,
        status: IndiStatus,
    },
    Roles(Roles),
    /// Full replacement of all device state.
    Devices(Cache),
    PropertyDefined(Property),
    PropertyUpdated(Property),
    PropertyDeleted {
        device: String,
        name: Option<String>,
    },
    Log(LogEntry),
    Capture {
        state: CaptureState,
        /// Frames completed in the current capture run.
        frames: u32,
    },
    Frame(Box<FrameInfo>),
    ImagingSettings(ImagingSettings),
    /// Current sequence run, if any.
    Sequence { status: Option<Box<SequenceStatus>> },
    /// Sequences saved on the engine.
    SequenceLibrary { sequences: Vec<Sequence> },
    Guider { status: Box<GuiderStatus> },
    GuideStep(GuideStep),
    GuiderSettings(GuiderSettings),
    Mount { info: Option<Box<MountInfo>> },
    Solve { status: SolveStatus },
    SolverSettings(SolverSettings),
    Autofocus { status: AutofocusStatus },
    FocusSettings(FocusSettings),
    Optics(Optics),
    /// Saved equipment profiles and the one last loaded or saved.
    Profiles { names: Vec<String>, active: Option<String> },
    /// A shared document, including edit `ack` (none: changed by the engine).
    Doc {
        doc: DocId,
        value: serde_json::Value,
        ack: Option<Ack>,
    },
    /// Calibration masters found in the masters folder.
    Masters(Box<MastersStatus>),
    /// Answer to [`ToEngine::FindObjects`], sent only to the UI that asked.
    ObjectsFound { query: String, objects: Vec<CatalogObject> },
}

/// UI → engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToEngine {
    ConnectIndi {
        host: String,
        port: u16,
    },
    DisconnectIndi,
    ConnectDevice {
        device: String,
    },
    DisconnectDevice {
        device: String,
    },
    SetText {
        device: String,
        name: String,
        values: Vec<(String, String)>,
    },
    SetNumber {
        device: String,
        name: String,
        values: Vec<(String, f64)>,
    },
    SetSwitch {
        device: String,
        name: String,
        values: Vec<(String, bool)>,
    },
    SetRoles(Roles),
    Capture(CaptureRequest),
    AbortCapture,
    SetImagingSettings(ImagingSettings),
    /// Read the masters folder again now (it is also re-read every minute).
    RescanMasters,
    StartSequence(Sequence),
    PauseSequence,
    ResumeSequence,
    AbortSequence,
    /// Save (or replace, by name) a sequence in the engine's library.
    SaveSequence(Sequence),
    DeleteSequence { name: String },
    /// Loop exposures on the guide camera.
    GuiderLoop,
    GuiderStop,
    /// Select the guide star nearest to this guide-frame position.
    GuiderSelectStar { x: f64, y: f64 },
    GuiderAutoSelect,
    GuiderCalibrate,
    /// Start guiding (selecting a star and calibrating first if needed).
    GuiderGuide,
    GuiderClearCalibration,
    SetGuiderSettings(GuiderSettings),
    /// Shift the lock position and wait for guiding to settle.
    GuiderDither,
    /// Slew to J2000 coordinates and track.
    MountGoto { ra_hours: f64, dec_deg: f64 },
    MountPark,
    MountUnpark,
    MountAbort,
    MountTracking { on: bool },
    /// Take a frame, plate solve it and sync the mount.
    SolveAndSync,
    /// Slew to J2000 coordinates, then solve/sync/re-slew until centred.
    CenterOn { ra_hours: f64, dec_deg: f64 },
    AbortSolve,
    SetSolverSettings(SolverSettings),
    /// Move the focuser by a number of steps (positive = outward).
    FocuserMove { steps: i64 },
    FocuserGoto { position: i64 },
    FocuserAbort,
    Autofocus,
    AbortAutofocus,
    SetFocusSettings(FocusSettings),
    SetOptics(Optics),
    /// Save the current rig (INDI server, roles, all settings) under a name.
    SaveProfile { name: String },
    LoadProfile { name: String },
    DeleteProfile { name: String },
    /// Changes to a shared document; `seq` counts up per UI session.
    EditDoc {
        doc: DocId,
        seq: u64,
        ops: Vec<doc::Op>,
    },
    /// Look up deep-sky objects by designation or common name.
    FindObjects { query: String },
}

macro_rules! json_codec {
    ($t:ty) => {
        impl $t {
            pub fn to_json(&self) -> String {
                serde_json::to_string(self).expect("protocol messages always serialize")
            }

            pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
                serde_json::from_str(s)
            }
        }
    };
}
json_codec!(ToUi);
json_codec!(ToEngine);

/// Engine state as mirrored by a UI.
#[derive(Debug, Clone)]
pub struct UiState {
    pub engine_version: Option<String>,
    pub indi_host: String,
    pub indi_port: u16,
    pub indi_status: IndiStatus,
    pub roles: Roles,
    pub cache: Cache,
    pub log: VecDeque<LogEntry>,
    /// Set when the engine speaks a different protocol version.
    pub version_mismatch: Option<u32>,
    pub capture: CaptureState,
    pub frames_captured: u32,
    pub last_frame: Option<FrameInfo>,
    pub imaging: ImagingSettings,
    pub masters: MastersStatus,
    pub sequence: Option<SequenceStatus>,
    pub library: Vec<Sequence>,
    pub guider: GuiderStatus,
    pub guider_settings: GuiderSettings,
    /// Recent guide steps, oldest first.
    pub guide_history: VecDeque<GuideStep>,
    pub mount: Option<MountInfo>,
    pub solve: SolveStatus,
    pub solver_settings: SolverSettings,
    pub autofocus: AutofocusStatus,
    pub focus_settings: FocusSettings,
    pub profiles: Vec<String>,
    pub active_profile: Option<String>,
    pub optics: Optics,
    /// This UI's session on the engine.
    pub session: Option<u64>,
    /// The engine's latest shared documents (UIs edit them via [`SharedDoc`]).
    pub docs: HashMap<DocId, serde_json::Value>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            engine_version: None,
            indi_host: "localhost".into(),
            indi_port: indi_link::DEFAULT_PORT,
            indi_status: IndiStatus::Disconnected { error: None },
            roles: Roles::default(),
            cache: Cache::default(),
            log: VecDeque::new(),
            version_mismatch: None,
            capture: CaptureState::Idle,
            frames_captured: 0,
            last_frame: None,
            imaging: ImagingSettings::default(),
            masters: MastersStatus::default(),
            sequence: None,
            library: Vec::new(),
            guider: GuiderStatus::default(),
            guider_settings: GuiderSettings::default(),
            guide_history: VecDeque::new(),
            mount: None,
            solve: SolveStatus::default(),
            solver_settings: SolverSettings::default(),
            autofocus: AutofocusStatus::default(),
            focus_settings: FocusSettings::default(),
            profiles: Vec::new(),
            active_profile: None,
            optics: Optics::default(),
            session: None,
            docs: HashMap::new(),
        }
    }
}

impl UiState {
    pub const MAX_LOG: usize = 5000;
    pub const MAX_GUIDE_HISTORY: usize = 1000;

    pub fn apply(&mut self, msg: ToUi) {
        match msg {
            ToUi::Hello {
                protocol,
                engine_version,
                session,
            } => {
                // The engine re-sends history after Hello, so start fresh.
                *self = UiState {
                    engine_version: Some(engine_version),
                    version_mismatch: (protocol != PROTOCOL_VERSION).then_some(protocol),
                    session: Some(session),
                    ..UiState::default()
                };
            }
            ToUi::IndiStatus { host, port, status } => {
                self.indi_host = host;
                self.indi_port = port;
                self.indi_status = status;
            }
            ToUi::Roles(r) => self.roles = r,
            ToUi::Devices(c) => self.cache = c,
            ToUi::PropertyDefined(p) | ToUi::PropertyUpdated(p) => {
                let device = self
                    .cache
                    .devices
                    .entry(p.device.clone())
                    .or_insert_with(|| Device {
                        name: p.device.clone(),
                        properties: Default::default(),
                    });
                device.properties.insert(p.name.clone(), p);
            }
            ToUi::PropertyDeleted { device, name } => match name {
                Some(n) => {
                    if let Some(d) = self.cache.devices.get_mut(&device) {
                        d.properties.shift_remove(&n);
                    }
                }
                None => {
                    self.cache.devices.shift_remove(&device);
                }
            },
            ToUi::Log(entry) => self.push_log(entry),
            ToUi::Capture { state, frames } => {
                self.capture = state;
                self.frames_captured = frames;
            }
            ToUi::Frame(info) => self.last_frame = Some(*info),
            ToUi::ImagingSettings(s) => self.imaging = s,
            ToUi::Masters(m) => self.masters = *m,
            ToUi::Sequence { status } => self.sequence = status.map(|s| *s),
            ToUi::SequenceLibrary { sequences } => self.library = sequences,
            ToUi::Guider { status } => self.guider = *status,
            ToUi::GuideStep(step) => {
                if self.guide_history.len() >= Self::MAX_GUIDE_HISTORY {
                    self.guide_history.pop_front();
                }
                self.guide_history.push_back(step);
            }
            ToUi::GuiderSettings(s) => self.guider_settings = s,
            ToUi::Mount { info } => self.mount = info.map(|i| *i),
            ToUi::Solve { status } => self.solve = status,
            ToUi::SolverSettings(s) => self.solver_settings = s,
            ToUi::Autofocus { status } => self.autofocus = status,
            ToUi::FocusSettings(s) => self.focus_settings = s,
            ToUi::Optics(o) => self.optics = o,
            ToUi::Profiles { names, active } => {
                self.profiles = names;
                self.active_profile = active;
            }
            ToUi::Doc { doc, value, .. } => {
                self.docs.insert(doc, value);
            }
            // A reply to one view's search, not shared state.
            ToUi::ObjectsFound { .. } => {}
        }
    }

    pub fn push_log(&mut self, entry: LogEntry) {
        if self.log.len() >= Self::MAX_LOG {
            self.log.pop_front();
        }
        self.log.push_back(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use guider::GuideDir;
    use indi_link::*;

    fn number_prop() -> Property {
        Property {
            device: "CCD".into(),
            name: "CCD_TEMPERATURE".into(),
            label: "Temperature".into(),
            group: "Main".into(),
            kind: PropertyKind::Number,
            state: PropertyState::Busy,
            perm: Permission::ReadWrite,
            rule: SwitchRule::OneOfMany,
            timeout: 60.0,
            timestamp: None,
            elements: vec![Element {
                name: "CCD_TEMPERATURE_VALUE".into(),
                label: "Temp".into(),
                value: ElementValue::Number(NumberValue {
                    value: -10.0,
                    min: -50.0,
                    max: 50.0,
                    step: 0.5,
                    format: "%5.2f".into(),
                }),
            }],
        }
    }

    #[test]
    fn addresses() {
        assert_eq!(parse_host_port("pi.local", 7624), ("pi.local".into(), 7624));
        assert_eq!(parse_host_port("pi.local:7625", 7624), ("pi.local".into(), 7625));
        assert_eq!(parse_host_port("[::1]:7000", 7624), ("::1".into(), 7000));
        assert_eq!(parse_host_port("::1", 7624), ("::1".into(), 7624));
        assert_eq!(engine_url("pi.local"), "ws://pi.local:7700/ws");
        assert_eq!(engine_url(" 10.0.0.5:9000 "), "ws://10.0.0.5:9000/ws");
        assert_eq!(engine_url("::1"), "ws://[::1]:7700/ws");
        assert_eq!(engine_url("wss://x.org/ws"), "wss://x.org/ws");
    }

    #[test]
    fn messages_round_trip() {
        let msgs = vec![
            ToUi::Hello {
                protocol: PROTOCOL_VERSION,
                engine_version: "0.1.0".into(),
                session: 3,
            },
            ToUi::Doc {
                doc: DocId::Capture,
                value: serde_json::to_value(CaptureForm::default()).unwrap(),
                ack: Some(Ack { session: 3, seq: 9 }),
            },
            ToUi::IndiStatus {
                host: "pi".into(),
                port: 7624,
                status: IndiStatus::Disconnected {
                    error: Some("refused".into()),
                },
            },
            ToUi::PropertyUpdated(number_prop()),
            ToUi::PropertyDeleted {
                device: "CCD".into(),
                name: None,
            },
        ];
        for m in msgs {
            assert_eq!(ToUi::from_json(&m.to_json()).unwrap(), m);
        }
        let cmd = ToEngine::SetSwitch {
            device: "CCD".into(),
            name: "CONNECTION".into(),
            values: vec![("CONNECT".into(), true)],
        };
        assert_eq!(ToEngine::from_json(&cmd.to_json()).unwrap(), cmd);
    }

    #[test]
    fn preview_frames() {
        use preview_frame::Kind;
        let msg = preview_frame::encode(Kind::Main, 42, &[0xFF, 0xD8, 1, 2]);
        assert_eq!(preview_frame::decode(&msg), Some((Kind::Main, 42, &[0xFF, 0xD8, 1, 2][..])));
        let msg = preview_frame::encode(Kind::Guide, 7, &[1]);
        assert_eq!(preview_frame::decode(&msg), Some((Kind::Guide, 7, &[1][..])));
        assert_eq!(preview_frame::decode(b"nope"), None);
        assert_eq!(preview_frame::decode(b"ICPV123"), None);
    }

    #[test]
    fn capture_messages_round_trip() {
        let cmd = ToEngine::Capture(CaptureRequest {
            exposure: 300.0,
            frame_type: FrameType::Dark,
            gain: Some(100.0),
            filter_slot: Some(3),
            object: "M 31".into(),
            ..Default::default()
        });
        assert_eq!(ToEngine::from_json(&cmd.to_json()).unwrap(), cmd);
        let st = ToUi::Capture {
            state: CaptureState::Exposing { total: 300.0, remaining: 12.5 },
            frames: 2,
        };
        assert_eq!(ToUi::from_json(&st.to_json()).unwrap(), st);
    }

    #[test]
    fn sequence_messages_round_trip() {
        let seq = Sequence {
            name: "LRGB".into(),
            steps: vec![SequenceStep::default(), sequence::presets::bias(10, Some(-10.0))],
            ..Default::default()
        };
        let status = SequenceStatus {
            sequence: seq.clone(),
            state: RunState::Running,
            current_step: Some(1),
            done: vec![3, 0],
            message: "x".into(),
        };
        for m in [
            ToUi::Sequence { status: None },
            ToUi::Sequence { status: Some(Box::new(status)) },
            ToUi::SequenceLibrary { sequences: vec![] },
            ToUi::SequenceLibrary { sequences: vec![seq.clone()] },
            ToUi::Guider { status: Box::default() },
            ToUi::GuideStep(GuideStep {
                frame: 3,
                time: 6.0,
                ra_px: 0.2,
                dec_px: -0.1,
                ra_pulse: Some((GuideDir::East, 120.0)),
                dec_pulse: None,
                snr: 40.0,
                star_lost: false,
            }),
            ToUi::GuiderSettings(GuiderSettings::default()),
            ToUi::Mount { info: None },
            ToUi::Solve { status: SolveStatus::default() },
            ToUi::ObjectsFound {
                query: "m110".into(),
                objects: vec![CatalogObject {
                    name: "M 110".into(),
                    aliases: vec!["NGC 205".into()],
                    common_names: vec![],
                    kind: "Galaxy".into(),
                    ra_hours: 0.673,
                    dec_deg: 41.685,
                    mag: Some(8.15),
                }],
            },
        ] {
            assert_eq!(ToUi::from_json(&m.to_json()).unwrap(), m);
        }
        for c in [
            ToEngine::StartSequence(seq.clone()),
            ToEngine::SaveSequence(seq),
            ToEngine::DeleteSequence { name: "x".into() },
            ToEngine::PauseSequence,
            ToEngine::AbortCapture,
            ToEngine::GuiderSelectStar { x: 1.5, y: 2.0 },
            ToEngine::GuiderGuide,
            ToEngine::GuiderDither,
            ToEngine::MountGoto { ra_hours: 5.5, dec_deg: -5.4 },
            ToEngine::MountTracking { on: false },
            ToEngine::CenterOn { ra_hours: 5.5, dec_deg: -5.4 },
            ToEngine::SetSolverSettings(SolverSettings::default()),
            ToEngine::FocuserMove { steps: -100 },
            ToEngine::SetFocusSettings(FocusSettings::default()),
            ToEngine::LoadProfile { name: "Esprit".into() },
            ToEngine::SetGuiderSettings(GuiderSettings::default()),
            ToEngine::FindObjects { query: "Orion".into() },
            ToEngine::RescanMasters,
            ToEngine::EditDoc {
                doc: DocId::Sequence,
                seq: 4,
                ops: vec![("/sequence/steps/0/count".into(), serde_json::json!(5))],
            },
        ] {
            assert_eq!(ToEngine::from_json(&c.to_json()).unwrap(), c);
        }
    }

    #[test]
    fn versioned_urls() {
        let v = PROTOCOL_VERSION;
        assert_eq!(versioned_url("ws://pi:7700/ws"), format!("ws://pi:7700/ws?protocol={v}"));
        assert_eq!(versioned_url("ws://pi/ws?x=1"), format!("ws://pi/ws?x=1&protocol={v}"));
        let once = versioned_url("ws://pi/ws");
        assert_eq!(versioned_url(&once), once);
    }

    #[test]
    fn blob_data_is_not_serialized() {
        let mut p = number_prop();
        p.kind = PropertyKind::Blob;
        p.elements[0].value = ElementValue::Blob(BlobValue {
            format: ".fits".into(),
            size: 4,
            data: std::sync::Arc::new(vec![1, 2, 3, 4]),
        });
        let back = ToUi::from_json(&ToUi::PropertyUpdated(p).to_json()).unwrap();
        let ToUi::PropertyUpdated(back) = back else {
            panic!()
        };
        let ElementValue::Blob(b) = &back.elements[0].value else {
            panic!()
        };
        assert_eq!((b.size, b.data.len()), (4, 0));
    }

    #[test]
    fn ui_state_mirrors_updates() {
        let mut s = UiState::default();
        s.push_log(LogEntry {
            time: "00:00:00".into(),
            device: None,
            text: "stale".into(),
            level: LogLevel::Info,
        });
        s.apply(ToUi::Hello {
            protocol: PROTOCOL_VERSION + 1,
            engine_version: "9".into(),
            session: 1,
        });
        assert!(s.log.is_empty());
        assert_eq!(s.version_mismatch, Some(PROTOCOL_VERSION + 1));

        s.apply(ToUi::PropertyDefined(number_prop()));
        let mut p = number_prop();
        p.state = PropertyState::Ok;
        s.apply(ToUi::PropertyUpdated(p));
        assert_eq!(
            s.cache.property("CCD", "CCD_TEMPERATURE").unwrap().state,
            PropertyState::Ok
        );
        s.apply(ToUi::PropertyDeleted {
            device: "CCD".into(),
            name: Some("CCD_TEMPERATURE".into()),
        });
        assert!(s.cache.property("CCD", "CCD_TEMPERATURE").is_none());
        assert!(s.cache.devices.contains_key("CCD"));
    }
}
