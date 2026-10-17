//! INDI property model (protocol version 1.7).

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PropertyKind {
    Text,
    Number,
    Switch,
    Light,
    Blob,
}

impl PropertyKind {
    /// Tag suffix used in `def*Vector`, `set*Vector`, `new*Vector`.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Number => "Number",
            Self::Switch => "Switch",
            Self::Light => "Light",
            Self::Blob => "BLOB",
        }
    }

    pub(crate) fn from_tag(s: &str) -> Option<Self> {
        Some(match s {
            "Text" => Self::Text,
            "Number" => Self::Number,
            "Switch" => Self::Switch,
            "Light" => Self::Light,
            "BLOB" => Self::Blob,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PropertyState {
    #[default]
    Idle,
    Ok,
    Busy,
    Alert,
}

impl PropertyState {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "Idle" => Self::Idle,
            "Ok" => Self::Ok,
            "Busy" => Self::Busy,
            "Alert" => Self::Alert,
            _ => return None,
        })
    }
}

impl fmt::Display for PropertyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Idle => "Idle",
            Self::Ok => "Ok",
            Self::Busy => "Busy",
            Self::Alert => "Alert",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Permission {
    ReadOnly,
    WriteOnly,
    #[default]
    ReadWrite,
}

impl Permission {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "ro" => Self::ReadOnly,
            "wo" => Self::WriteOnly,
            "rw" => Self::ReadWrite,
            _ => return None,
        })
    }

    pub fn writable(self) -> bool {
        self != Self::ReadOnly
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SwitchRule {
    #[default]
    OneOfMany,
    AtMostOne,
    AnyOfMany,
}

impl SwitchRule {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "OneOfMany" => Self::OneOfMany,
            "AtMostOne" => Self::AtMostOne,
            "AnyOfMany" => Self::AnyOfMany,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NumberValue {
    pub value: f64,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    /// printf-style format, possibly the INDI `%m` sexagesimal extension.
    pub format: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlobValue {
    /// e.g. ".fits", ".fits.z" (zlib-compressed), ".stream".
    pub format: String,
    /// Decoded (and decompressed, if `.z`) payload. Empty until a BLOB arrives.
    pub data: std::sync::Arc<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElementValue {
    Text(String),
    Number(NumberValue),
    Switch(bool),
    Light(PropertyState),
    Blob(BlobValue),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub name: String,
    pub label: String,
    pub value: ElementValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub device: String,
    pub name: String,
    pub label: String,
    pub group: String,
    pub kind: PropertyKind,
    pub state: PropertyState,
    pub perm: Permission,
    pub rule: SwitchRule,
    pub timeout: f64,
    pub timestamp: Option<String>,
    pub elements: Vec<Element>,
}

impl Property {
    pub fn element(&self, name: &str) -> Option<&Element> {
        self.elements.iter().find(|e| e.name == name)
    }

    pub fn number(&self, name: &str) -> Option<f64> {
        match &self.element(name)?.value {
            ElementValue::Number(n) => Some(n.value),
            _ => None,
        }
    }

    pub fn text(&self, name: &str) -> Option<&str> {
        match &self.element(name)?.value {
            ElementValue::Text(t) => Some(t),
            _ => None,
        }
    }

    pub fn switch(&self, name: &str) -> Option<bool> {
        match &self.element(name)?.value {
            ElementValue::Switch(s) => Some(*s),
            _ => None,
        }
    }

    /// Name of the first switch that is on (useful for OneOfMany properties).
    pub fn active_switch(&self) -> Option<&str> {
        self.elements.iter().find_map(|e| match e.value {
            ElementValue::Switch(true) => Some(e.name.as_str()),
            _ => None,
        })
    }
}

/// `DRIVER_INFO.DRIVER_INTERFACE` bit flags.
pub mod interface {
    pub const GENERAL: u32 = 0;
    pub const TELESCOPE: u32 = 1 << 0;
    pub const CCD: u32 = 1 << 1;
    pub const GUIDER: u32 = 1 << 2;
    pub const FOCUSER: u32 = 1 << 3;
    pub const FILTER: u32 = 1 << 4;
    pub const DOME: u32 = 1 << 5;
    pub const GPS: u32 = 1 << 6;
    pub const WEATHER: u32 = 1 << 7;
    pub const AO: u32 = 1 << 8;
    pub const DUSTCAP: u32 = 1 << 9;
    pub const LIGHTBOX: u32 = 1 << 10;
    pub const DETECTOR: u32 = 1 << 11;
    pub const ROTATOR: u32 = 1 << 12;
}
