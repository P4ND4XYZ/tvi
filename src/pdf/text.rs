//! Text extraction is independent of the `WinRT` renderer and returns only hit positions.

use std::sync::Arc;

use super::Failure;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hit {
    pub page: u32,
    pub from_top: f64,
    pub aspect_height: f64,
    pub bounds: Option<Bounds>,
}

/// Normalized left, top, right and bottom edges of complete matched glyphs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Bounds(pub [f64; 4]);

pub(crate) type ResultHits = Result<Arc<[Hit]>, Failure>;

#[cfg(windows)]
mod extract;
#[cfg(windows)]
mod worker;
#[cfg(windows)]
pub(super) use extract::Input;
#[cfg(windows)]
pub(super) use worker::spawn;
#[cfg(windows)]
pub(crate) use worker::worker_entry;
