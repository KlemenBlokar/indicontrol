//! The engine core: a single task that owns the INDI connection and all
//! engine state. UI sessions and helper tasks talk to it through [`CoreMsg`].

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use indi_link::{Cache, Client, ClientError, Event};
use protocol::{IndiStatus, LogEntry, LogLevel, PROTOCOL_VERSION, ToEngine, ToUi};
use tokio::sync::{broadcast, mpsc, oneshot};

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
}

/// What a new UI session needs: a state dump, then live updates.
pub(crate) struct Session {
    pub initial: Vec<String>,
    pub updates: broadcast::Receiver<Arc<str>>,
}

pub(crate) struct Core {
    rx: mpsc::UnboundedReceiver<CoreMsg>,
    tx: mpsc::UnboundedSender<CoreMsg>,
    updates: broadcast::Sender<Arc<str>>,
    state: PersistedState,
    state_file: Option<PathBuf>,
    client: Option<Client>,
    indi_events: Option<mpsc::UnboundedReceiver<Event>>,
    status: IndiStatus,
    /// Identifies the current connection attempt; stale results are dropped.
    attempt: u64,
    failures: u32,
    log: VecDeque<LogEntry>,
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
            }
            Event::Updated { device, name } => {
                if let Some(p) = self.property(&device, &name) {
                    self.send(ToUi::PropertyUpdated(p));
                }
            }
            Event::Deleted { device, name } => self.send(ToUi::PropertyDeleted { device, name }),
            Event::Message { device, text, .. } => self.log(device, text, LogLevel::Info),
            Event::Blob {
                device,
                element,
                format,
                data,
                ..
            } => self.log(
                Some(device),
                format!("Received {element} ({format}, {} bytes)", data.len()),
                LogLevel::Info,
            ),
            Event::Disconnected(reason) => {
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
        let mut initial = vec![
            ToUi::Hello {
                protocol: PROTOCOL_VERSION,
                engine_version: env!("CARGO_PKG_VERSION").into(),
            },
            self.status_msg(),
            ToUi::Roles(self.state.roles.clone()),
            ToUi::Devices(devices),
        ];
        initial.extend(self.log.iter().cloned().map(ToUi::Log));
        Session {
            initial: initial.iter().map(ToUi::to_json).collect(),
            updates,
        }
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
        // No receivers just means no UI is connected.
        let _ = self.updates.send(msg.to_json().into());
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
