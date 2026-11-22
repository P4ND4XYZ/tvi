use std::{fs, os::windows::ffi::OsStrExt as _, path::Path, sync::Arc};

use ::windows::{
    Data::Pdf::{PdfDocument, PdfPage, PdfPageRenderOptions, PdfPageRotation},
    Graphics::Imaging::BitmapEncoder,
    Storage::Streams::{DataReader, IRandomAccessStream, InMemoryRandomAccessStream},
    UI::Color,
    Win32::System::WinRT::{
        CreateRandomAccessStreamOnFile, RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
    },
    core::{Error, PCWSTR},
};

use super::{Failure, Page, Source, text};

const MAX_PNG_BYTES: u64 = 16 * 1024 * 1024;
const LOAD: &str = "Could not open the PDF document (invalid or password-protected PDF)";
const RENDER: &str = "Could not render the PDF page";

pub(super) struct Apartment;

impl Apartment {
    #[allow(
        unsafe_code,
        reason = "WinRT initialization for the dedicated native renderer thread"
    )]
    pub(super) fn initialize() -> Result<Self, Failure> {
        // SAFETY: Called on the current worker thread. Both successful initialization
        // statuses must be paired with RoUninitialize on this same thread.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
            .map_err(|error| native("pdf_worker", "initialize", RENDER, &error))?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    #[allow(
        unsafe_code,
        reason = "paired with the renderer thread's successful RoInitialize"
    )]
    fn drop(&mut self) {
        // SAFETY: The renderer owns this guard and is created and dropped on the same worker.
        unsafe { RoUninitialize() };
    }
}

fn native(
    stage: &'static str,
    reason: &'static str,
    message: &'static str,
    error: &Error,
) -> Failure {
    Failure {
        stage,
        reason,
        message,
        code: Some(error.code().0),
    }
}

pub(super) struct Document {
    native: PdfDocument,
    // Keep the input readable for lazy page loads; release it after the document.
    _stream: IRandomAccessStream,
    pub count: u32,
}

impl Document {
    pub(super) fn open(path: &Path) -> Result<(Self, text::Input), Failure> {
        let absolute =
            std::path::absolute(path).map_err(|error| Failure::io("pdf_load", &error))?;
        let metadata = fs::metadata(&absolute).map_err(|error| Failure::io("metadata", &error))?;
        if !metadata.is_file() {
            return Err(Failure::new(
                "metadata",
                "not_regular_file",
                "PDF path is not a regular file",
            ));
        }
        let load_error = |error| native("pdf_load", "invalid_or_password_protected", LOAD, &error);
        let stream = open_stream(&absolute)?;
        let native = PdfDocument::LoadFromStreamAsync(&stream)
            .and_then(|op| op.join())
            .map_err(load_error)?;
        if native.IsPasswordProtected().map_err(load_error)? {
            return Err(Failure::new(
                "pdf_load",
                "password_protected",
                "Password-protected PDFs are not supported",
            ));
        }
        let count = native.PageCount().map_err(load_error)?;
        Ok((
            Self {
                native,
                _stream: stream,
                count,
            },
            text::Input::new(absolute, &metadata),
        ))
    }

    pub(super) fn render(&self, source: Source, index: u32) -> Result<Page, Failure> {
        if index >= self.count {
            return Err(Failure::new("pdf_render", "page_out_of_range", RENDER));
        }
        let render_error = |error| native("pdf_render", "winrt", RENDER, &error);
        let png_error = |error| native("pdf_png", "stream", RENDER, &error);
        let page = self.native.GetPage(index).map_err(render_error)?;
        // Failure to identify search geometry must not affect ordinary rendering.
        let searchable_geometry = searchable_geometry(&page).unwrap_or(false);
        let result = render_png(&page, render_error, png_error);
        let close = page.Close().map_err(render_error);
        let (png, aspect_height) = result?;
        close?;
        Ok(Page {
            source,
            index,
            png,
            aspect_height,
            searchable_geometry,
        })
    }
}

fn searchable_geometry(page: &PdfPage) -> windows::core::Result<bool> {
    if page.Rotation()? != PdfPageRotation::Normal {
        return Ok(false);
    }
    let dimensions = page.Dimensions()?;
    Ok(dimensions.MediaBox()? == dimensions.CropBox()?)
}

#[allow(
    unsafe_code,
    reason = "Win32 to WinRT file-stream adapter for canonical and verbatim paths"
)]
fn open_stream(path: &Path) -> Result<IRandomAccessStream, Failure> {
    let wide: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: The absolute path was checked by fs::metadata. The UTF-16 buffer
    // is NUL-terminated and lives through this call. Mode 0 is FileAccessMode::Read.
    unsafe { CreateRandomAccessStreamOnFile(PCWSTR::from_raw(wide.as_ptr()), 0) }.map_err(|error| {
        native(
            "pdf_load",
            "file_stream",
            "Could not read the PDF document",
            &error,
        )
    })
}

fn render_png(
    page: &PdfPage,
    render_error: impl Fn(Error) -> Failure,
    png_error: impl Fn(Error) -> Failure,
) -> Result<(Arc<[u8]>, f64), Failure> {
    let size = page.Size().map_err(&render_error)?;
    let (width, height) = (f64::from(size.Width), f64::from(size.Height));
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(Failure::new("pdf_render", "invalid_page_size", RENDER));
    }

    // Limit the pixel count before invoking the encoder; keep room for PNG overhead
    // under the encoded byte limit and the shared image loader's own limits.
    let scale = (2048.0 / width)
        .min(4096.0 / height)
        .min((3_900_000.0 / (width * height)).sqrt());
    let pixels = |size: f64| {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "both output dimensions are finite and bounded above"
        )]
        let dimension = (size * scale).floor().max(1.0) as u32;
        dimension
    };

    let options = PdfPageRenderOptions::new().map_err(&render_error)?;
    options
        .SetBackgroundColor(Color {
            A: 255,
            R: 255,
            G: 255,
            B: 255,
        })
        .map_err(&render_error)?;
    options
        .SetIsIgnoringHighContrast(true)
        .map_err(&render_error)?;
    options
        .SetDestinationWidth(pixels(width))
        .map_err(&render_error)?;
    options
        .SetDestinationHeight(pixels(height))
        .map_err(&render_error)?;
    options
        .SetBitmapEncoderId(BitmapEncoder::PngEncoderId().map_err(&render_error)?)
        .map_err(&render_error)?;
    let stream = InMemoryRandomAccessStream::new().map_err(&png_error)?;
    page.RenderWithOptionsToStreamAsync(&stream, &options)
        .and_then(|op| op.join())
        .map_err(&render_error)?;

    let length = stream.Size().map_err(&png_error)?;
    if length == 0 || length > MAX_PNG_BYTES {
        return Err(Failure::new("pdf_png", "encoded_size", RENDER));
    }
    let length =
        u32::try_from(length).map_err(|_| Failure::new("pdf_png", "encoded_size", RENDER))?;
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0).map_err(&png_error)?)
        .map_err(&png_error)?;
    if reader
        .LoadAsync(length)
        .and_then(|op| op.join())
        .map_err(&png_error)?
        != length
    {
        return Err(Failure::new("pdf_png", "incomplete_stream", RENDER));
    }
    let mut png = vec![0; length as usize];
    reader.ReadBytes(&mut png).map_err(&png_error)?;
    Ok((png.into(), height / width))
}

#[cfg(test)]
mod tests {
    use image::GenericImageView as _;

    use super::*;
    use crate::pdf::{Renderer, Worker};
    use std::{io, time::Duration};

    fn native_error(failure: Failure) -> io::Error {
        io::Error::other(format!("{failure:?}"))
    }

    fn request(
        worker: &mut Worker,
        job: impl FnOnce(&mut Renderer) -> Result<Page, Failure> + Send + 'static,
    ) -> Result<Page, Box<dyn std::error::Error>> {
        Ok(worker
            .submit(job)
            .map_err(native_error)?
            .recv_timeout(Duration::from_secs(15))?
            .map_err(native_error)?)
    }

    #[test]
    fn rotated_pdf_page_uses_its_display_aspect_ratio() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("rotation.pdf");
        fs::write(&path, crate::test_pdf::rotated_second_page())?;
        let mut worker = Worker::default();
        let first = request(&mut worker, move |renderer| renderer.open(&path))?;
        let rotated = request(&mut worker, move |renderer| {
            renderer.render(first.source, 1)
        })?;
        let raster = rotated.decode().map_err(native_error)?;
        let (w, h) = raster.dimensions();
        assert!(w.abs_diff(h * 2) <= 2, "rotated raster is {w} x {h}");
        assert!((rotated.aspect_height() - 0.5).abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn windows_pdf_renders_only_the_requested_page_to_bounded_png()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("資料.PDF");
        fs::write(&path, crate::test_pdf::two_pages())?;
        let mut worker = Worker::default();
        let first = request(&mut worker, move |renderer| renderer.open(&path))?;
        assert_eq!(first.source.count, 2);
        let source = first.source.clone();
        let second = request(&mut worker, move |renderer| renderer.render(source, 1))?;
        for (page, index, height_ratio, bottom) in [
            (first, 0, 4, [255, 0, 0, 255]),
            (second, 1, 2, [0, 0, 255, 255]),
        ] {
            assert_eq!(page.index, index);
            assert!(page.png.len() as u64 <= MAX_PNG_BYTES);
            assert!(page.png.starts_with(b"\x89PNG\r\n\x1a\n"));
            let raster = page.decode().map_err(native_error)?;
            let (w, h) = raster.dimensions();
            assert!(h.abs_diff(w * height_ratio) <= 4, "{w} x {h}");
            assert_eq!(raster.get_pixel(w / 2, h / 4).0, [255, 255, 255, 255]);
            assert_eq!(raster.get_pixel(w / 2, 3 * h / 4).0, bottom);
        }
        Ok(())
    }
}
