use std::{io, path::Path, sync::Arc};

use crate::images::Budget;

#[cfg(windows)]
mod windows;

pub(crate) fn is_pdf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Failure {
    pub stage: &'static str,
    pub reason: &'static str,
    pub code: Option<i32>,
    pub message: &'static str,
}

impl Failure {
    pub const fn new(stage: &'static str, reason: &'static str, message: &'static str) -> Self {
        Self {
            stage,
            reason,
            code: None,
            message,
        }
    }

    pub const fn platform() -> Self {
        Self::new(
            "pdf_support",
            "unsupported_platform",
            "PDF viewing is only supported on Windows",
        )
    }

    pub const fn terminal() -> Self {
        Self::new(
            "pdf_support",
            "kitty_required",
            "PDF viewing requires a Kitty graphics terminal",
        )
    }

    pub const fn worker() -> Self {
        Self::new(
            "pdf_worker",
            "worker_unavailable",
            "Could not start or complete the PDF request",
        )
    }

    #[cfg(windows)]
    fn io(stage: &'static str, error: &io::Error) -> Self {
        Self {
            stage,
            reason: crate::logging::io_reason(error),
            code: error.raw_os_error(),
            message: "Could not read the PDF document",
        }
    }
}

impl From<Failure> for io::Error {
    fn from(failure: Failure) -> Self {
        Self::other(failure.message)
    }
}

#[derive(Clone)]
pub(crate) struct Source {
    #[cfg(windows)]
    native: ::windows::Data::Pdf::PdfDocument,
    pub count: u32,
}

#[derive(Clone)]
pub(crate) struct Page {
    pub source: Source,
    pub index: u32,
    // Keep a bounded raster for the current page of each document in history, not an
    // entire PDF's worth of decoded pages or terminal placements.
    png: Arc<[u8]>,
    aspect_height: f64,
}

impl Page {
    pub fn open(path: &Path) -> Result<Self, Failure> {
        #[cfg(windows)]
        return windows::open(path);
        #[cfg(not(windows))]
        {
            let _ = path;
            Err(Failure::platform())
        }
    }

    pub fn render(source: Source, index: u32) -> Result<Self, Failure> {
        #[cfg(windows)]
        return windows::render(source, index);
        #[cfg(not(windows))]
        {
            let _ = (source, index);
            Err(Failure::platform())
        }
    }

    pub fn decode(&self) -> Result<image::DynamicImage, Failure> {
        Budget::default().png(&self.png).ok_or_else(|| {
            Failure::new(
                "pdf_png",
                "invalid_png",
                "Could not decode the PDF page image",
            )
        })
    }

    pub fn aspect_height(&self) -> f64 {
        self.aspect_height
    }
}
