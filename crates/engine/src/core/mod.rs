//! The engine core: a single task that owns the INDI connection and all
//! engine state. UI sessions and helper tasks talk to it through [`CoreMsg`].

mod capture;
mod focus;
mod guider;
mod mount;
mod sequence;
mod solve;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use indi_link::{Cache, Client, ClientError, Event};
use protocol::{IndiStatus, LogEntry, LogLevel, PROTOCOL_VERSION, ToEngine, ToUi};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::frame::ProcessOutput;
use crate::state::PersistedState;

/// Log entries replayed to newly connected UIs.
const LOG_HISTORY: usize = 500;
const BROADCAST_CAPACITY: usize = 4096;

type ConnectResult = Result<(Client, mpsc::UnboundedReceiver<Event>), ClientError>;

pub(crate) enum CoreMsg {
    Ui(ToEngine),
    NewSession(oneshot::Sender<Session>),
    ConnectResult { attempt: u64, result: ConnectResult },
    Retry { attempt: u64 },
    FrameDone {
        job_id: u64,
        result: Box<Result<ProcessOutput, String>>,
    },
    CaptureTimeout { job_id: u64, frame: u32 },
    FilterTimeout { job_id: u64 },
    TemperatureTimeout { run_id: u64, step: usize },
    GuideFrame {
        seq: u64,
        result: Box<Result<guider::GuideFrameOut, String>>,
    },
    GuideNext { seq: u64 },
    GuideTimeout { seq: u64 },
    SequenceRecheck { run_id: u64 },
    FlipTimeout { run_id: u64 },
    SolveDone {
        id: u64,
        result: Box<Result<protocol::SolveResult, String>>,
    },
    SolveTimer {
        id: u64,
        epoch: u64,
        timer: solve::SolveTimer,
    },
    FocusTimeout { id: u64, epoch: u64 },
}

/// A message to UIs: JSON text or a binary image preview.
#[derive(Clone)]
pub(crate) enum Outgoing {
    Text(Arc<str>),
    Binary(Bytes),
}

impl Outgoing {
    fn text(msg: &ToUi) -> Self {
        Outgoing::Text(msg.to_json().into())
    }
}

/// What a new UI session needs: a state dump, then live updates.
pub(crate) struct Session {
    pub initial: Vec<Outgoing>,
    pub updates: broadcast::Receiver<Outgoing>,
}

pub(crate) struct Core {
    rx: mpsc::UnboundedReceiver<CoreMsg>,
    tx: mpsc::UnboundedSender<CoreMsg>,
    updates: broadcast::Sender<Outgoing>,
    state: PersistedState,
    state_file: Option<PathBuf>,
    client: Option<Client>,
    indi_events: Option<mpsc::UnboundedReceiver<Event>>,
    status: IndiStatus,
    /// Identifies the current connection attempt; stale results are dropped.
    attempt: u64,
    failures: u32,
    log: VecDeque<LogEntry>,
    job: Option<capture::Job>,
    next_job_id: u64,
    next_frame_id: u64,
    /// Latest frame info + preview, replayed to new UIs.
    last_frame: Option<(Outgoing, Outgoing)>,
    run: Option<sequence::Run>,
    next_run_id: u64,
    guider: guider::GuiderCore,
    solve_job: Option<solve::SolveJob>,
    next_solve_id: u64,
    solve_status: protocol::SolveStatus,
    focus_job: Option<focus::FocusJob>,
    next_focus_id: u64,
    focus_status: protocol::AutofocusStatus,
    last_focus_temperature: Option<f64>,
}

impl Core {
    pub fn new(state: PersistedState, state_file: Option<PathBuf>) -> (Self, mpsc::UnboundedSender<CoreMsg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (updates, _) = broadcast::channel(BROADCAST_CAPACITY);
        let core = Core {
            rx,
            tx: tx.clone(),
            updates,
            state,
            state_file,
            client: None,
            indi_events: None,
            status: IndiStatus::Disconnected { error: None },
            attempt: 0,
            failures: 0,
            log: VecDeque::new(),
            job: None,
            next_job_id: 0,
            next_frame_id: 0,
            last_frame: None,
            run: None,
            next_run_id: 0,
            guider: Default::default(),
            solve_job: None,
            next_solve_id: 0,
            solve_status: Default::default(),
            focus_job: None,
            next_focus_id: 0,
            focus_status: Default::default(),
            last_focus_temperature: None,
        };
        (core, tx)
    }

    pub async fn run(mut self) {
        if self.state.indi.autoconnect {
            self.connect();
        }
        loop {
            tokio::select! {
                msg = self.rx.recv() => match msg {
                    Some(msg) => self.handle(msg),
                    None => break,
                },
                ev = next_event(&mut self.indi_events) => self.handle_indi(ev),
            }
        }
    }

    fn handle(&mut self, msg: CoreMsg) {
        match msg {
            CoreMsg::Ui(cmd) => self.handle_ui(cmd),
            CoreMsg::NewSession(reply) => {
                let _ = reply.send(self.session());
            }
            CoreMsg::ConnectResult { attempt, result } => {
                if attempt != self.attempt {
                    return; // superseded; dropping the client closes it
                }
                match result {
                    Ok((client, events)) => {
                        self.failures = 0;
                        self.client = Some(client);
                        self.indi_events = Some(events);
                        self.set_status(IndiStatus::Connected);
                        let text = format!(
                            "Connected to INDI server {}:{}",
                            self.state.indi.host, self.state.indi.port
                        );
                        self.log(None, text, LogLevel::Info);
                    }
                    Err(e) => {
                        self.set_status(IndiStatus::Disconnected {
                            error: Some(e.to_string()),
                        });
                        self.log(None, e.to_string(), LogLevel::Error);
                        self.schedule_retry();
                    }
                }
            }
            CoreMsg::FrameDone { job_id, result } => self.frame_done(job_id, *result),
            CoreMsg::CaptureTimeout { job_id, frame } => self.capture_timeout(job_id, frame),
            CoreMsg::FilterTimeout { job_id } => self.filter_timeout(job_id),
            CoreMsg::TemperatureTimeout { run_id, step } => self.temperature_timeout(run_id, step),
            CoreMsg::GuideFrame { seq, result } => self.guider_frame(seq, *result),
            CoreMsg::GuideNext { seq } => self.guider_next(seq),
            CoreMsg::GuideTimeout { seq } => self.guider_timeout(seq),
            CoreMsg::SequenceRecheck { run_id } => self.sequence_recheck(run_id),
            CoreMsg::FlipTimeout { run_id } => self.flip_timeout(run_id),
            CoreMsg::SolveDone { id, result } => self.solve_done(id, *result),
            CoreMsg::SolveTimer { id, epoch, timer } => self.solve_on_timer(id, epoch, timer),
            CoreMsg::FocusTimeout { id, epoch } => self.focus_timeout(id, epoch),
            CoreMsg::Retry { attempt } => {
                if attempt == self.attempt && self.state.indi.autoconnect && self.client.is_none() {
                    self.connect();
                }
            }
        }
    }

    fn handle_ui(&mut self, cmd: ToEngine) {
        let result = match cmd {
            ToEngine::ConnectIndi { host, port } => {
                self.state.indi.host = host;
                self.state.indi.port = port;
                self.state.indi.autoconnect = true;
                self.save();
                self.failures = 0;
                self.connect();
                Ok(())
            }
            ToEngine::DisconnectIndi => {
                self.fail_capture("disconnected from INDI".into());
                self.sequence_failed("disconnected from INDI".into());
                self.guider_halt("disconnected from INDI".into());
                self.state.indi.autoconnect = false;
                self.save();
                self.attempt += 1;
                if self.client.take().is_some() {
                    self.log(None, "Disconnected from INDI server", LogLevel::Info);
                }
                self.indi_events = None;
                self.set_status(IndiStatus::Disconnected { error: None });
                self.send(ToUi::Devices(Cache::default()));
                Ok(())
            }
            ToEngine::SetRoles(roles) => {
                self.state.roles = roles.clone();
                self.save();
                self.send(ToUi::Roles(roles));
                self.send(self.mount_msg());
                self.push_optics();
                self.send_guider();
                Ok(())
            }
            ToEngine::SetImagingSettings(settings) => {
                if settings.output_dir.trim().is_empty() {
                    self.log(None, "Output directory must not be empty", LogLevel::Error);
                } else {
                    self.state.imaging = settings.clone();
                    self.save();
                    self.send(ToUi::ImagingSettings(settings));
                }
                Ok(())
            }
            ToEngine::Capture(req) => {
                if let Err(e) = self.start_capture(req, capture::Purpose::User) {
                    self.log(None, format!("Cannot capture: {e}"), LogLevel::Error);
                }
                Ok(())
            }
            ToEngine::AbortCapture => {
                if self.run.is_some() {
                    self.abort_sequence();
                } else {
                    self.abort_capture();
                }
                Ok(())
            }
            ToEngine::StartSequence(seq) => {
                if let Err(e) = self.start_sequence(seq) {
                    self.log(None, format!("Cannot start sequence: {e}"), LogLevel::Error);
                }
                Ok(())
            }
            ToEngine::PauseSequence => {
                self.pause_sequence();
                Ok(())
            }
            ToEngine::ResumeSequence => {
                self.resume_sequence();
                Ok(())
            }
            ToEngine::AbortSequence => {
                self.abort_sequence();
                Ok(())
            }
            ToEngine::SaveSequence(seq) => {
                if seq.name.trim().is_empty() {
                    self.log(None, "Give the sequence a name before saving", LogLevel::Error);
                } else {
                    let lib = &mut self.state.sequences;
                    match lib.iter_mut().find(|s| s.name == seq.name) {
                        Some(existing) => *existing = seq,
                        None => lib.push(seq),
                    }
                    lib.sort_by_key(|a| a.name.to_lowercase());
                    self.save();
                    self.send(ToUi::SequenceLibrary {
                        sequences: self.state.sequences.clone(),
                    });
                }
                Ok(())
            }
            ToEngine::GuiderLoop => self.guider_result(|c| c.guider_loop()),
            ToEngine::GuiderStop => {
                self.guider_stop();
                Ok(())
            }
            ToEngine::GuiderSelectStar { x, y } => self.guider_result(|c| c.guider_select(Some((x, y)))),
            ToEngine::GuiderAutoSelect => self.guider_result(|c| c.guider_select(None)),
            ToEngine::GuiderCalibrate => self.guider_result(|c| c.guider_calibrate(false)),
            ToEngine::GuiderGuide => self.guider_result(|c| c.guider_guide()),
            ToEngine::GuiderDither => self.guider_result(|c| c.guider_dither()),
            ToEngine::MountGoto { ra_hours, dec_deg } => self.mount_result(|c| c.mount_goto(ra_hours, dec_deg)),
            ToEngine::MountPark => self.mount_result(|c| c.mount_park(true)),
            ToEngine::MountUnpark => self.mount_result(|c| c.mount_park(false)),
            ToEngine::MountAbort => self.mount_result(|c| c.mount_abort()),
            ToEngine::MountTracking { on } => self.mount_result(|c| c.mount_tracking(on)),
            ToEngine::SolveAndSync => self.solve_result(|c| c.start_solve(None, false)),
            ToEngine::CenterOn { ra_hours, dec_deg } => {
                self.solve_result(|c| c.start_solve(Some((ra_hours, dec_deg)), false))
            }
            ToEngine::AbortSolve => {
                self.abort_solve();
                Ok(())
            }
            ToEngine::SetSolverSettings(settings) => {
                self.state.solver = settings.clone();
                self.save();
                self.send(ToUi::SolverSettings(settings));
                Ok(())
            }
            ToEngine::FocuserMove { steps } => self.focus_result(|c| c.focuser_move(steps)),
            ToEngine::FocuserGoto { position } => self.focus_result(|c| {
                if c.focusing() {
                    return Err("autofocus is running".into());
                }
                c.focuser_goto(position)
            }),
            ToEngine::FocuserAbort => self.focus_result(|c| c.focuser_abort()),
            ToEngine::Autofocus => self.focus_result(|c| c.start_autofocus(None, false)),
            ToEngine::AbortAutofocus => {
                self.abort_autofocus();
                Ok(())
            }
            ToEngine::SetFocusSettings(settings) => {
                self.state.focus = settings.clone();
                self.save();
                self.send(ToUi::FocusSettings(settings));
                Ok(())
            }
            ToEngine::SetOptics(optics) => {
                self.state.optics = optics.clone();
                self.save();
                self.send(ToUi::Optics(optics));
                self.push_optics();
                self.send_guider();
                Ok(())
            }
            ToEngine::SaveProfile { name } => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    self.log(None, "Give the profile a name", LogLevel::Error);
                } else {
                    let p = self.state.snapshot(&name);
                    let list = &mut self.state.profiles;
                    match list.iter_mut().find(|x| x.name == name) {
                        Some(existing) => *existing = p,
                        None => list.push(p),
                    }
                    list.sort_by_key(|p| p.name.to_lowercase());
                    self.state.active_profile = Some(name.clone());
                    self.save();
                    self.log(None, format!("Profile '{name}' saved"), LogLevel::Info);
                    self.send(self.profiles_msg());
                }
                Ok(())
            }
            ToEngine::LoadProfile { name } => {
                if let Err(e) = self.load_profile(&name) {
                    self.log(None, format!("Cannot load profile: {e}"), LogLevel::Error);
                }
                Ok(())
            }
            ToEngine::DeleteProfile { name } => {
                self.state.profiles.retain(|p| p.name != name);
                if self.state.active_profile.as_deref() == Some(name.as_str()) {
                    self.state.active_profile = None;
                }
                self.save();
                self.send(self.profiles_msg());
                Ok(())
            }
            ToEngine::GuiderClearCalibration => {
                self.state.guider_calibration = None;
                self.save();
                self.send_guider();
                Ok(())
            }
            ToEngine::SetGuiderSettings(settings) => {
                self.state.guider = settings.clone();
                self.save();
                self.send(ToUi::GuiderSettings(settings));
                self.send_guider();
                Ok(())
            }
            ToEngine::DeleteSequence { name } => {
                self.state.sequences.retain(|s| s.name != name);
                self.save();
                self.send(ToUi::SequenceLibrary {
                        sequences: self.state.sequences.clone(),
                    });
                Ok(())
            }
            ToEngine::ConnectDevice { device } => self.with_client(|c| c.connect_device(&device)),
            ToEngine::DisconnectDevice { device } => {
                self.with_client(|c| c.disconnect_device(&device))
            }
            ToEngine::SetText {
                device,
                name,
                values,
            } => self.with_client(|c| {
                let v: Vec<_> = values.iter().map(|(e, v)| (e.as_str(), v.as_str())).collect();
                c.send_text(&device, &name, &v)
            }),
            ToEngine::SetNumber {
                device,
                name,
                values,
            } => self.with_client(|c| {
                let v: Vec<_> = values.iter().map(|(e, v)| (e.as_str(), *v)).collect();
                c.send_number(&device, &name, &v)
            }),
            ToEngine::SetSwitch {
                device,
                name,
                values,
            } => self.with_client(|c| {
                let v: Vec<_> = values.iter().map(|(e, v)| (e.as_str(), *v)).collect();
                c.send_switch(&device, &name, &v)
            }),
        };
        if let Err(e) = result {
            self.log(None, e.to_string(), LogLevel::Error);
        }
    }

    fn handle_indi(&mut self, ev: Option<Event>) {
        let Some(ev) = ev else {
            self.indi_events = None;
            return;
        };
        match ev {
            Event::Defined { device, name } => {
                if let Some(p) = self.property(&device, &name) {
                    self.send(ToUi::PropertyDefined(p));
                }
                // Cameras define SCOPE_INFO when they connect.
                if name == "SCOPE_INFO" {
                    self.push_optics();
                }
            }
            Event::Updated { device, name } => {
                if let Some(p) = self.property(&device, &name) {
                    self.send(ToUi::PropertyUpdated(p));
                }
                self.capture_on_update(&device, &name);
                self.mount_on_update(&device, &name);
                self.solve_on_update(&device, &name);
                self.focus_on_update(&device, &name);
                self.sequence_on_update(&device, &name);
            }
            Event::Deleted { device, name } => self.send(ToUi::PropertyDeleted { device, name }),
            Event::Message { device, text, .. } => self.log(device, text, LogLevel::Info),
            Event::Blob {
                device,
                format,
                data,
                ..
            } => {
                if !self.guider_on_blob(&device, format.clone(), data.clone()) {
                    self.capture_on_blob(&device, format, data);
                }
            }
            Event::Disconnected(reason) => {
                self.fail_capture("INDI connection lost".into());
                self.sequence_failed("INDI connection lost".into());
                self.guider_halt("INDI connection lost".into());
                self.client = None;
                self.indi_events = None;
                let error = reason.unwrap_or_else(|| "server closed the connection".into());
                self.log(None, format!("INDI connection lost: {error}"), LogLevel::Warning);
                self.set_status(IndiStatus::Disconnected { error: Some(error) });
                self.send(ToUi::Devices(Cache::default()));
                self.schedule_retry();
            }
        }
    }

    fn connect(&mut self) {
        self.attempt += 1;
        self.client = None;
        self.indi_events = None;
        self.send(ToUi::Devices(Cache::default()));
        self.set_status(IndiStatus::Connecting);
        let attempt = self.attempt;
        let addr = (self.state.indi.host.clone(), self.state.indi.port);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = Client::connect(addr).await;
            let _ = tx.send(CoreMsg::ConnectResult { attempt, result });
        });
    }

    fn schedule_retry(&mut self) {
        if !self.state.indi.autoconnect {
            return;
        }
        let delay = match self.failures {
            0 => 2,
            1 => 5,
            2 => 10,
            _ => 30,
        };
        self.failures += 1;
        self.log(None, format!("Reconnecting in {delay} s…"), LogLevel::Info);
        let attempt = self.attempt;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(delay)).await;
            let _ = tx.send(CoreMsg::Retry { attempt });
        });
    }

    /// Write configured focal lengths/apertures to the cameras' SCOPE_INFO.
    fn push_optics(&self) {
        let Some(client) = &self.client else {
            return;
        };
        let o = &self.state.optics;
        for (camera, focal, aperture) in [
            (&self.state.roles.main_camera, o.focal_length, o.aperture),
            (&self.state.roles.guide_camera, o.guide_focal_length, o.guide_aperture),
        ] {
            let (Some(camera), Some(focal)) = (camera, focal) else {
                continue;
            };
            let current = {
                let cache = client.cache();
                let Some(p) = cache.property(camera, "SCOPE_INFO") else {
                    continue;
                };
                (p.number("FOCAL_LENGTH"), p.number("APERTURE"))
            };
            let aperture = aperture.or(current.1).unwrap_or(0.0);
            if current != (Some(focal), Some(aperture)) {
                let _ = client.send_number(
                    camera,
                    "SCOPE_INFO",
                    &[("FOCAL_LENGTH", focal), ("APERTURE", aperture)],
                );
            }
        }
    }

    fn profiles_msg(&self) -> ToUi {
        ToUi::Profiles {
            names: self.state.profiles.iter().map(|p| p.name.clone()).collect(),
            active: self.state.active_profile.clone(),
        }
    }

    fn load_profile(&mut self, name: &str) -> Result<(), String> {
        if self.run.is_some() || self.job.is_some() || self.solving() || self.focusing() {
            return Err("stop the running sequence, capture, solve or autofocus first".into());
        }
        let profile = self
            .state
            .profiles
            .iter()
            .find(|p| p.name == name)
            .cloned()
            .ok_or_else(|| format!("no profile named '{name}'"))?;
        self.guider_stop();
        let old_server = (self.state.indi.host.clone(), self.state.indi.port);
        self.state.apply(&profile);
        self.save();
        self.push_optics();
        self.log(None, format!("Profile '{name}' loaded"), LogLevel::Info);
        for msg in [
            ToUi::Roles(self.state.roles.clone()),
            ToUi::Optics(self.state.optics.clone()),
            ToUi::ImagingSettings(self.state.imaging.clone()),
            ToUi::GuiderSettings(self.state.guider.clone()),
            ToUi::SolverSettings(self.state.solver.clone()),
            ToUi::FocusSettings(self.state.focus.clone()),
            self.profiles_msg(),
            self.mount_msg(),
            self.status_msg(),
        ] {
            self.send(msg);
        }
        self.send_guider();
        // A different INDI server: reconnect (or disconnect) accordingly.
        if (self.state.indi.host.clone(), self.state.indi.port) != old_server || self.client.is_none() {
            if self.state.indi.autoconnect {
                self.failures = 0;
                self.connect();
            } else if self.client.take().is_some() {
                self.attempt += 1;
                self.indi_events = None;
                self.set_status(IndiStatus::Disconnected { error: None });
                self.send(ToUi::Devices(Cache::default()));
            }
        }
        Ok(())
    }

    fn focus_result(&mut self, f: impl FnOnce(&mut Self) -> Result<(), String>) -> Result<(), ClientError> {
        if let Err(e) = f(self) {
            self.log(None, format!("Focuser: {e}"), LogLevel::Error);
        }
        Ok(())
    }

    fn solve_result(&mut self, f: impl FnOnce(&mut Self) -> Result<(), String>) -> Result<(), ClientError> {
        if let Err(e) = f(self) {
            self.log(None, format!("Plate solving: {e}"), LogLevel::Error);
        }
        Ok(())
    }

    fn mount_result(&mut self, f: impl FnOnce(&mut Self) -> Result<(), String>) -> Result<(), ClientError> {
        if let Err(e) = f(self) {
            self.log(None, format!("Mount: {e}"), LogLevel::Error);
        }
        Ok(())
    }

    fn guider_result(&mut self, f: impl FnOnce(&mut Self) -> Result<(), String>) -> Result<(), ClientError> {
        if let Err(e) = f(self) {
            self.log(None, format!("Guider: {e}"), LogLevel::Error);
            self.send_guider();
        }
        Ok(())
    }

    fn with_client(&self, f: impl FnOnce(&Client) -> Result<(), ClientError>) -> Result<(), ClientError> {
        match &self.client {
            Some(c) => f(c),
            None => Err(ClientError::Disconnected),
        }
    }

    fn property(&self, device: &str, name: &str) -> Option<indi_link::Property> {
        self.client.as_ref()?.cache().property(device, name).cloned()
    }

    fn session(&self) -> Session {
        // Subscribe first: anything broadcast after this point reaches the
        // session, and all messages are idempotent, so overlap is harmless.
        let updates = self.updates.subscribe();
        let devices = self
            .client
            .as_ref()
            .map(|c| c.cache().clone())
            .unwrap_or_default();
        let mut initial: Vec<Outgoing> = [
            ToUi::Hello {
                protocol: PROTOCOL_VERSION,
                engine_version: env!("CARGO_PKG_VERSION").into(),
            },
            self.status_msg(),
            ToUi::Roles(self.state.roles.clone()),
            ToUi::Optics(self.state.optics.clone()),
            ToUi::ImagingSettings(self.state.imaging.clone()),
            ToUi::Devices(devices),
            self.capture_msg(),
            ToUi::SequenceLibrary {
                sequences: self.state.sequences.clone(),
            },
            self.sequence_msg(),
            ToUi::GuiderSettings(self.state.guider.clone()),
            self.guider_msg(),
            self.mount_msg(),
            ToUi::SolverSettings(self.state.solver.clone()),
            self.solve_msg(),
            ToUi::FocusSettings(self.state.focus.clone()),
            self.focus_msg(),
            self.profiles_msg(),
        ]
        .iter()
        .map(Outgoing::text)
        .collect();
        if let Some((info, preview)) = &self.last_frame {
            initial.push(info.clone());
            initial.push(preview.clone());
        }
        initial.extend(self.guider_last_preview());
        initial.extend(self.log.iter().cloned().map(|l| Outgoing::text(&ToUi::Log(l))));
        Session { initial, updates }
    }

    fn status_msg(&self) -> ToUi {
        ToUi::IndiStatus {
            host: self.state.indi.host.clone(),
            port: self.state.indi.port,
            status: self.status.clone(),
        }
    }

    fn set_status(&mut self, status: IndiStatus) {
        self.status = status;
        self.send(self.status_msg());
    }

    fn send(&self, msg: ToUi) {
        self.broadcast(Outgoing::text(&msg));
    }

    fn broadcast(&self, msg: Outgoing) {
        // No receivers just means no UI is connected.
        let _ = self.updates.send(msg);
    }

    fn log(&mut self, device: Option<String>, text: impl Into<String>, level: LogLevel) {
        let entry = LogEntry {
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
            device,
            text: text.into(),
            level,
        };
        match level {
            LogLevel::Error => tracing::error!("{}", entry.text),
            LogLevel::Warning => tracing::warn!("{}", entry.text),
            LogLevel::Info => tracing::info!("{}", entry.text),
        }
        if self.log.len() >= LOG_HISTORY {
            self.log.pop_front();
        }
        self.log.push_back(entry.clone());
        self.send(ToUi::Log(entry));
    }

    fn save(&self) {
        if let Some(path) = &self.state_file {
            self.state.save(path);
        }
    }
}

async fn next_event(rx: &mut Option<mpsc::UnboundedReceiver<Event>>) -> Option<Event> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}
