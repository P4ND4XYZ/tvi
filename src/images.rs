use std::{
    fs::{self, File},
    io::{Cursor, Read},
    path::{Component, Path},
};

use image::{DynamicImage, ImageFormat, ImageReader, Limits};

use crate::markdown::ImageReference;

const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DIMENSION: u32 = 4096;
const MAX_PIXELS: u64 = 4 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;

pub fn load(document: &Path, references: &[ImageReference]) -> Vec<Option<DynamicImage>> {
    let parent = document.parent().unwrap_or_else(|| Path::new(""));
    let mut budget = MAX_DOCUMENT_BYTES;
    references
        .iter()
        .map(|reference| {
            let target = Path::new(&reference.target);
            if budget == 0
                || target.has_root()
                || matches!(target.components().next(), Some(Component::Prefix(_)))
            {
                return None;
            }
            let image = decode(&parent.join(target), budget)?;
            budget -= image.as_bytes().len() as u64;
            Some(image)
        })
        .collect()
}

fn decode(path: &Path, budget: u64) -> Option<DynamicImage> {
    // Check before opening so a FIFO or directory is not read as an image.
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    let file = File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    // Bound input even if the file grows after the metadata check.
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return None;
    }

    let format = image::guess_format(&bytes).ok()?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        return None;
    }
    let reader = || {
        let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(MAX_DIMENSION);
        limits.max_image_height = Some(MAX_DIMENSION);
        limits.max_alloc = Some(32 * 1024 * 1024);
        reader.limits(limits);
        reader
    };
    let (width, height) = reader().into_dimensions().ok()?;
    let pixels = u64::from(width) * u64::from(height);
    // PNG RGBA16 is the largest supported decoded pixel (eight bytes).
    if pixels == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || pixels > MAX_PIXELS
        || pixels * 8 > budget
    {
        return None;
    }
    reader().decode().ok()
}
