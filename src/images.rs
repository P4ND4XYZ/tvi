use std::{
    fs::{self, File},
    io::{Cursor, Read},
    path::{Component, Path},
};

use image::{DynamicImage, ImageFormat, ImageReader, Limits};

use crate::{
    logging::{self, Level},
    markdown::ImageReference,
};

const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DIMENSION: u32 = 4096;
const MAX_PIXELS: u64 = 4 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) struct Budget(u64);

impl Default for Budget {
    fn default() -> Self {
        Self(MAX_DOCUMENT_BYTES)
    }
}

impl Budget {
    pub(crate) fn png(&self, bytes: &[u8]) -> Option<DynamicImage> {
        self.cached_png(bytes).ok().flatten()
    }

    // A valid image that exceeds the remaining document budget is not corrupt.
    pub(crate) fn cached_png(&self, bytes: &[u8]) -> Result<Option<DynamicImage>, ()> {
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(());
        }
        match decode_bytes(bytes, ImageFormat::Png, self.0) {
            Ok(image) => Ok(Some(image)),
            Err(("limits", "document_budget")) => Ok(None),
            Err(_) => Err(()),
        }
    }

    pub(crate) fn charge(&mut self, image: &DynamicImage) {
        self.0 -= image.as_bytes().len() as u64;
    }
}

pub fn load(
    document: &Path,
    references: &[ImageReference],
    budget: &mut Budget,
) -> Vec<Option<DynamicImage>> {
    let parent = document.parent().unwrap_or_else(|| Path::new(""));
    references
        .iter()
        .map(|reference| {
            let target = Path::new(&reference.target);
            let result = if budget.0 == 0 {
                Err(("limits", "document_budget"))
            } else if target.has_root()
                || matches!(target.components().next(), Some(Component::Prefix(_)))
            {
                Err(("resolve", "non_relative_path"))
            } else {
                decode(&parent.join(target), budget.0)
            };
            match result {
                Ok(image) => {
                    budget.charge(&image);
                    Some(image)
                }
                Err((stage, reason)) => {
                    logging::emit(
                        Level::Warn,
                        "image.load_failed",
                        serde_json::json!({
                            "document":logging::basename(document), "image_index":reference.ordinal + 1,
                            "stage":stage, "reason":reason,
                        }),
                    );
                    None
                }
            }
        })
        .collect()
}

fn decode(path: &Path, budget: u64) -> Result<DynamicImage, (&'static str, &'static str)> {
    // Check before opening so a FIFO or directory is not read as an image.
    let metadata = fs::metadata(path).map_err(|error| ("metadata", logging::io_reason(&error)))?;
    if !metadata.is_file() {
        return Err(("metadata", "not_regular_file"));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(("limits", "file_size"));
    }
    let file = File::open(path).map_err(|error| ("open", logging::io_reason(&error)))?;
    if !file
        .metadata()
        .map_err(|error| ("metadata", logging::io_reason(&error)))?
        .is_file()
    {
        return Err(("metadata", "not_regular_file"));
    }
    // Bound input even if the file grows after the metadata check.
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ("read", logging::io_reason(&error)))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(("limits", "file_size"));
    }

    let format = image::guess_format(&bytes).map_err(|_| ("format", "unsupported_format"))?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        return Err(("format", "unsupported_format"));
    }
    decode_bytes(&bytes, format, budget)
}

fn decode_bytes(
    bytes: &[u8],
    format: ImageFormat,
    budget: u64,
) -> Result<DynamicImage, (&'static str, &'static str)> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(("limits", "file_size"));
    }
    let reader = || {
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(MAX_DIMENSION);
        limits.max_image_height = Some(MAX_DIMENSION);
        limits.max_alloc = Some(32 * 1024 * 1024);
        reader.limits(limits);
        reader
    };
    let (width, height) = reader()
        .into_dimensions()
        .map_err(|error| ("dimensions", decode_reason(&error)))?;
    let pixels = u64::from(width) * u64::from(height);
    // PNG RGBA16 is the largest supported decoded pixel (eight bytes).
    if pixels == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION || pixels > MAX_PIXELS {
        return Err(("limits", "image_size_or_budget"));
    }
    if pixels * 8 > budget {
        return Err(("limits", "document_budget"));
    }
    reader()
        .decode()
        .map_err(|error| ("decode", decode_reason(&error)))
}

fn decode_reason(error: &image::ImageError) -> &'static str {
    match error {
        image::ImageError::Limits(_) => "image_limit",
        _ => "invalid_image",
    }
}
