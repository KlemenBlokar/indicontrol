//! Image handling for indicontrol: FITS I/O, statistics, previews, and the
//! astronomy needed for frame metadata. Platform-independent (builds for wasm).

pub mod astro;
pub mod fits;
pub mod image;
pub mod preview;
pub mod stats;

pub use fits::{Card, Fits, FitsError, Header, Value};
pub use image::{Cfa, Image};
pub use preview::{Preview, Stf, encode_jpeg, make_preview};
pub use stats::{Analysis, ChannelStats, HIST_BINS, analyze};
