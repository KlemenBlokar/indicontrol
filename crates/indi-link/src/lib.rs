//! INDI protocol (Instrument Neutral Distributed Interface) support.
//!
//! The property model, parser and formatting are platform-independent (they
//! also build for wasm). The async TCP [`Client`] needs the `client` feature.

pub mod cache;
#[cfg(feature = "client")]
pub mod client;
pub mod format;
pub mod parser;
pub mod types;

pub use cache::{Cache, Device};
#[cfg(feature = "client")]
pub use client::{BlobMode, Client, ClientError, Event};
/// Default `indiserver` TCP port.
pub const DEFAULT_PORT: u16 = 7624;

pub use format::{format_number, parse_number};
pub use types::*;

pub use indexmap::IndexMap;
