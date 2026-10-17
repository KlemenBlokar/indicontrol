//! Messages exchanged between the indicontrol engine and its UIs.
//!
//! Transport is a WebSocket carrying one JSON message per text frame.
//! (Binary frames are reserved for image previews.) On connect, the engine
//! sends [`ToUi::Hello`] followed by a full state dump; afterwards it sends
//! incremental updates. UIs mirror state with [`UiState::apply`].

use std::collections::VecDeque;

use indi_link::{Cache, Device, Property};
use serde::{Deserialize, Serialize};

pub use indi_link;

/// Bump on any incompatible message change.
pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_ENGINE_PORT: u16 = 7700;
pub const WS_PATH: &str = "/ws";

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

/// Engine → UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToUi {
    Hello {
        protocol: u32,
        engine_version: String,
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
        }
    }
}

impl UiState {
    pub const MAX_LOG: usize = 5000;

    pub fn apply(&mut self, msg: ToUi) {
        match msg {
            ToUi::Hello {
                protocol,
                engine_version,
            } => {
                // The engine re-sends history after Hello, so start fresh.
                *self = UiState {
                    engine_version: Some(engine_version),
                    version_mismatch: (protocol != PROTOCOL_VERSION).then_some(protocol),
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
