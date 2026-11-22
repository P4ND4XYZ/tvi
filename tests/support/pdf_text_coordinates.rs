//! Windows search and coordinate integration: real input, `WinRT` raster and Kitty clipping.

#[path = "kitty_clip.rs"]
mod kitty_clip;

use std::{cell::RefCell, error::Error, fs, io, ops::Range, rc::Rc, sync::mpsc};

use KeyCode::{Char, Enter, Esc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use image::{RgbaImage, imageops};
use pdf_extract::MediaBox;
use ratatui::{Terminal, backend::TestBackend};

use crate::{
    app::{Action, Direction, Document, Mode, ViewRows},
    logging,
    lua::{FileOpen, Host, Runtime},
    pdf::Failure,
    session::{BODY_VIEW, InputQueue, Session, poll_pdf},
    terminal::ImageMode,
    ui,
};

use super::{Intent, PlacedPage, Running, tests::wait};
use kitty_clip::KittyClip;

const TERM: &str = "ハイ";
const VIEW_ROWS: u16 = 10;
const CELL_W: u32 = 8;
const CELL_H: u32 = 16;
const KITTY: ImageMode = ImageMode::Kitty {
    cell_width: 8,
    cell_height: 16,
};

// Independently specified from the synthetic PDF, not production-extracted positions.
#[derive(Debug)]
struct Hit {
    page: u32,
    media: MediaBox,
    x: f64,
    y: f64,
    right: f64,
}

fn hit(page: u32, x: f64, y: f64, height: f64) -> Hit {
    Hit {
        page,
        media: MediaBox {
            llx: 0.,
            lly: 0.,
            urx: 240.,
            ury: height,
        },
        x,
        y,
        right: x + 72.,
    }
}

#[derive(Debug)]
struct Ink {
    x: Range<u32>,
    y: Range<u32>,
}

// Only the visible Type 3 strokes use these colors. Sample the decoded WinRT
// bitmap, not PDF objects, callback positions, or marks added by the probe.
fn ink_bounds(image: &RgbaImage) -> [Option<Ink>; 3] {
    let mut bounds = [None, None, None];
    for (x, y, pixel) in image.enumerate_pixels() {
        let channel = match pixel.0 {
            [210..=255, 0..=130, 0..=130, _] => 0,
            [0..=130, 210..=255, 0..=130, _] => 1,
            [0..=130, 0..=130, 210..=255, _] => 2,
            _ => continue,
        };
        let ink = bounds[channel].get_or_insert(Ink {
            x: x..x + 1,
            y: y..y + 1,
        });
        ink.x.start = ink.x.start.min(x);
        ink.x.end = ink.x.end.max(x + 1);
        ink.y.end = y + 1; // Pixels are in row-major order.
    }
    bounds
}

#[test]
fn japanese_same_line_hits_scroll_to_winrt_ink() -> Result<(), Box<dyn Error>> {
    let (mut view, dir) = View::open()?;
    let first = view.sync()?;
    let document = view.session.borrow().app.document_id();
    let [upper, lower, other] = [
        hit(1, 54., 838., 960.),
        hit(1, 112., 104., 960.),
        hit(2, 77., 288., 600.),
    ];
    assert_eq!(first.rows().len(), 64);

    // The first match is above the viewport top. Forward search starts at
    // that top, not at the first match in PDF content or visual order.
    view.key(&first, KeyCode::PageDown);
    assert_eq!(view.session.borrow().app.top_row(), 10);
    view.query(&first, '/', TERM)?;
    let first = view.expect_hit(&lower, 2, 2)?;
    view.key(&first, Char('N'));
    let first = view.expect_hit(&upper, 0, 1)?;
    view.key(&first, Char('n'));
    let first = view.expect_hit(&lower, 2, 2)?;
    let before = view.session.borrow().app.top_row();

    // A live refit keeps the reading position and match count. The same
    // position is checked against actual Kitty tile-row offsets at both widths.
    view.terminal.backend_mut().resize(56, VIEW_ROWS + 1);
    let wide = view.expect_hit(&lower, 2, 2)?;
    assert!(!Rc::ptr_eq(&first, &wide), "resize reused old placement");
    assert_eq!(wide.rows().len(), 112);
    assert_eq!((before, view.session.borrow().app.top_row()), (52, 91));

    view.key(&wide, Char('N'));
    let wide = view.expect_hit(&upper, 0, 1)?;
    view.key(&wide, Char('n'));
    let wide = view.expect_hit(&lower, 2, 2)?;
    view.key(&wide, Char('n'));
    let wide_second = view.expect_hit(&other, 1, 3)?;
    assert_eq!(wide_second.rows().len(), 70);

    view.key(&wide_second, Char('N'));
    poll_pdf(&view.session, &view.runtime); // submit cross-page render at 56 columns
    view.terminal.backend_mut().resize(32, VIEW_ROWS + 1);
    let first = view.expect_hit(&lower, 2, 2)?;
    assert_eq!(first.rows().len(), 64);
    view.key(&first, Char('n'));
    let second = view.expect_hit(&other, 1, 3)?;
    assert_eq!(second.rows().len(), 40);
    let second_top = view.session.borrow().app.top_row();

    let markdown = dir.path().join("after-pdf.md");
    fs::write(&markdown, "# After PDF")?;
    assert!(matches!(
        view.session.open_file(BODY_VIEW, &markdown)?,
        FileOpen::Opened
    ));
    view.press(&[], Char('o'), KeyModifiers::CONTROL);
    let second = view.expect_hit(&other, 1, 3)?;
    assert_eq!(view.session.borrow().app.document_id(), document);
    assert_eq!(view.session.borrow().app.top_row(), second_top);

    view.key(&second, Char('n'));
    let first = view.expect_hit(&upper, 0, 1)?; // wrap forward
    view.key(&first, Char('N'));
    let second = view.expect_hit(&other, 1, 3)?; // wrap backward
    view.query(&second, '?', "")?; // reuse query, change direction
    let first = view.expect_hit(&lower, 2, 2)?;
    view.key(&first, Char('n'));
    let first = view.expect_hit(&upper, 0, 1)?;
    view.key(&first, Char('N'));
    let first = view.expect_hit(&lower, 2, 2)?;
    view.press(first.as_ref(), Char('o'), KeyModifiers::CONTROL);
    assert_eq!(
        view.session.borrow().app.document_path(),
        std::path::Path::new("start.md")
    );
    Ok(())
}

#[test]
fn partial_literal_highlight_after_scroll_resize_and_noh() -> Result<(), Box<dyn Error>> {
    let (mut view, _dir) = View::open()?;
    let first = view.sync()?;
    let original = view.native_page()?;
    // Query matches the middle of イハイハ: the two black glyphs are not matches.
    view.query(&first, '/', TERM)?;
    let first = view.sync()?;
    assert_eq!(view.session.borrow().app.search_selection(), Some((1, 3)));
    view.key(&first, Char('j'));
    for width in [32, 56] {
        view.terminal.backend_mut().resize(width, VIEW_ROWS + 1);
        let layout = view.sync()?;
        let clip = viewport_pixels(&mut view, &layout)?;
        let top = view.session.borrow().app.top_row();
        assert_partial_mark(
            &clip,
            &original,
            layout.rows().len(),
            top,
            Some([255, 255, 0]),
        )?;
        assert!(
            Rc::ptr_eq(&layout, &view.sync()?),
            "plain redraw refitted the raster"
        );
    }

    let layout = view.sync()?;
    let top = view.session.borrow().app.top_row();
    view.command(&layout, "noh");
    let plain = view.sync()?;
    assert_eq!(view.session.borrow().app.search_selection(), Some((1, 3)));
    assert_eq!(view.session.borrow().app.top_row(), top);
    assert!(!view.session.borrow().app.search_highlights_enabled());
    let clip = viewport_pixels(&mut view, &plain)?;
    assert_partial_mark(&clip, &original, plain.rows().len(), top, None)?;

    view.key(&plain, Char('n'));
    let layout = view.sync()?;
    assert_eq!(view.session.borrow().app.search_selection(), Some((2, 3)));
    assert!(view.session.borrow().app.search_highlights_enabled());
    for _ in 0..12 {
        view.key(&layout, KeyCode::PageUp);
    }
    for _ in 0..top {
        view.key(&layout, Char('j'));
    }
    assert_eq!(view.session.borrow().app.top_row(), top);
    let clip = viewport_pixels(&mut view, &layout)?;
    assert_partial_mark(
        &clip,
        &original,
        layout.rows().len(),
        top,
        Some([0, 0, 255]),
    )?;

    view.command(&layout, "nohlsearch");
    let plain = view.sync()?;
    view.key(&plain, Char('N'));
    let layout = view.sync()?;
    assert_eq!(view.session.borrow().app.search_selection(), Some((1, 3)));
    let clip = viewport_pixels(&mut view, &layout)?;
    let top = view.session.borrow().app.top_row();
    assert_partial_mark(
        &clip,
        &original,
        layout.rows().len(),
        top,
        Some([255, 255, 0]),
    )?;
    assert_eq!(view.native_page()?, original, "the source raster changed");
    Ok(())
}

#[test]
fn clipped_labels_are_not_counted_or_painted() -> Result<(), Box<dyn Error>> {
    for (path_clip, mode, count) in [("W", 4, 4), ("W*", 5, 4), ("W", 6, 4), ("W*", 7, 2)] {
        check_clipped_labels(path_clip, mode, count)?;
    }
    Ok(())
}

fn check_clipped_labels(path_clip: &str, mode: u8, count: usize) -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("labels.pdf");
    fs::write(&path, crate::test_pdf::clipped_labels(path_clip, mode))?;
    let mut view = View::new()?;
    assert!(matches!(
        view.session.open_file(BODY_VIEW, &path)?,
        FileOpen::Pending
    ));
    let plain = view.sync()?;
    let base = viewport_pixels(&mut view, &plain)?;
    view.query(&plain, '/', "TAG")?;
    let layout = view.sync()?;
    assert_eq!(
        view.session.borrow().app.search_selection(),
        Some((1, count)),
        "{path_clip}, Tr {mode}"
    );
    let marked = viewport_pixels(&mut view, &layout)?;
    // The two clipped labels would have baselines at 116 and 62pt, on a page
    // fitted to 256x160 pixels. Verify WinRT rendered blank there and that
    // the fitted image stayed blank after the production highlight pass.
    let mut blank = vec![24..40, 82..97];
    if mode == 7 {
        blank.extend([44..59, 63..78]); // Clip-only glyphs never paint.
    }
    for band in blank {
        for y in band {
            for x in 24..70 {
                assert_eq!(base.get_pixel(x, y).0, [255; 4], "unclipped ink @ {x},{y}");
                assert_eq!(
                    marked.get_pixel(x, y),
                    base.get_pixel(x, y),
                    "blank tinted @ {x},{y}"
                );
            }
        }
    }
    assert!(
        base.enumerate_pixels().any(|(x, y, p)| {
            y < 18
                && p.0[0] < 40
                && p.0[1] > 100
                && marked.get_pixel(x, y).0 == blended(p.0, [255, 255, 0])
        }),
        "visible label outside the clip was not highlighted"
    );
    assert!(
        base.enumerate_pixels().any(|(x, y, p)| {
            (100..115).contains(&y)
                && p.0[0] < 40
                && p.0[1] > 100
                && marked.get_pixel(x, y).0 == blended(p.0, [0, 0, 255])
        }),
        "the label after Q did not regain highlighting"
    );
    Ok(())
}

fn viewport_pixels(view: &mut View, layout: &PlacedPage) -> Result<RgbaImage, Box<dyn Error>> {
    for _ in 0..8 {
        view.draw(layout)?;
        let buffer = view.terminal.backend().buffer();
        let state = view.session.borrow();
        let body = ui::body_area(buffer.area, state.app.mode());
        if !layout.pdf_transfer_pending(body, state.app.top_row()) {
            return view.kitty.clip(buffer, body, CELL_W, CELL_H);
        }
    }
    Err("visible PDF tiles did not finish transferring".into())
}

fn assert_partial_mark(
    clip: &RgbaImage,
    native: &RgbaImage,
    rows: usize,
    top: usize,
    tint: Option<[u16; 3]>,
) -> Result<(), Box<dyn Error>> {
    let start = u32::try_from(top)? * CELL_H;
    let base = imageops::resize(
        native,
        clip.width(),
        u32::try_from(rows)? * CELL_H,
        imageops::FilterType::Triangle,
    );
    let mut ink = [0; 3];
    for (x, y, original) in base.enumerate_pixels() {
        let [r, g, b, _] = original.0;
        let kind = if r > 210 && g < 110 && b < 110 {
            0 // Match ink is uniquely red in the fixture.
        } else if r == g && g == b && r < 250 && y < base.height() / 5 {
            if x * 240 < 54 * base.width() {
                1
            } else if x * 240 >= 126 * base.width() {
                2
            } else {
                continue;
            }
        } else {
            continue;
        };
        assert!(
            (start..start + clip.height()).contains(&y),
            "fixture ink type {kind} not in viewport @ {x},{y}, top={top}"
        );
        let visible = clip.get_pixel(x, y - start);
        let expected = if kind == 0 {
            tint.map_or(original.0, |tint| blended(original.0, tint))
        } else {
            original.0
        };
        assert_eq!(visible.0, expected, "ink type {kind} @ {x},{y}");

        ink[kind] += 1;
    }
    assert!(
        ink.into_iter().all(|pixels| pixels > 16),
        "ink sample missing: {ink:?}"
    );
    Ok(())
}

#[test]
fn pending_cancel_and_superseded_searches_preserve_the_view() -> Result<(), Box<dyn Error>> {
    let (mut view, dir) = View::open()?;
    let first = view.sync()?;
    view.key(&first, KeyCode::PageDown);
    let snapshot = |view: &View| {
        let state = view.session.borrow();
        let app = &state.app;
        (
            app.document_id(),
            app.pdf().map(|page| page.index),
            app.top_row(),
        )
    };
    let original = snapshot(&view);

    view.query(&first, '/', "")?;
    assert_eq!(
        view.session.borrow().app.notice(),
        Some("No previous search")
    );
    assert_eq!(snapshot(&view), original);

    view.key(&first, Char('/'));
    view.key(&first, Char('x'));
    view.key(&first, Esc);
    assert_eq!(view.session.borrow().app.mode(), Mode::Normal);
    assert!(view.session.borrow().app.search_query().is_empty());
    assert_eq!(snapshot(&view), original);

    view.query(&first, '/', TERM)?;
    poll_pdf(&view.session, &view.runtime);
    view.key(&first, Esc);
    let first = view.sync()?;
    assert_eq!(view.session.borrow().app.notice(), None);
    assert!(view.session.borrow().app.search_query().is_empty());
    assert_eq!(snapshot(&view), original);

    // Let the older query start but do not poll its completion before dispatching
    // the replacement. Only the replacement may publish.
    view.query(&first, '/', TERM)?;
    poll_pdf(&view.session, &view.runtime);
    view.query(&first, '/', "一致しない")?;
    let first = view.sync()?;
    assert_eq!(view.session.borrow().app.search_query(), "一致しない");
    assert_eq!(view.session.borrow().app.search_selection(), None);
    assert_eq!(view.session.borrow().app.notice(), Some("No matches"));
    assert_eq!(snapshot(&view), original);

    // Even a rapid move away and back to the same document ID must not revive
    // the previous in-flight request.
    view.query(&first, '/', TERM)?;
    poll_pdf(&view.session, &view.runtime);
    let markdown = dir.path().join("leave.md");
    fs::write(&markdown, "# Leave PDF")?;
    assert!(matches!(
        view.session.open_file(BODY_VIEW, &markdown)?,
        FileOpen::Opened
    ));
    view.press(&[], Char('o'), KeyModifiers::CONTROL);
    view.sync()?;
    assert_eq!(view.session.borrow().app.search_query(), "一致しない");
    assert_eq!(view.session.borrow().app.search_selection(), None);
    assert_eq!(snapshot(&view), original);
    Ok(())
}

#[test]
fn failed_or_cancelled_search_render_does_not_replace_the_page_or_selection()
-> Result<(), Box<dyn Error>> {
    let (mut view, dir) = View::open()?;
    let first = view.sync()?;
    // Start backwards above the first hit to wrap immediately to page two.
    view.query(&first, '?', TERM)?;
    let second = view.expect_hit(&hit(2, 77., 288., 600.), 1, 3)?;
    let before = {
        let state = view.session.borrow();
        (
            state.app.document_id(),
            state.app.pdf().map(|page| page.index),
            state.app.top_row(),
            state.app.search_selection(),
        )
    };

    view.key(&second, Char('n'));
    let failure = Failure::new("pdf_render", "fixture", "Could not render the PDF page");
    {
        let mut state = view.session.borrow_mut();
        let request = state.pdf.queued.take().ok_or("expected search render")?;
        assert!(matches!(request.intent, Intent::Search { .. }));
        let (send, result) = mpsc::channel();
        assert!(send.send(Err(failure)).is_ok());
        // Substitute only native completion, not input, extraction or navigation.
        state.pdf.running = Some(Running {
            intent: Some(request.intent),
            geometry: state.pdf_geometry.map_err(io::Error::from)?,
            marks: None,
            result,
        });
    }
    let logs = dir.path().join("logs");
    logging::capture_for_test(&logs, || assert!(poll_pdf(&view.session, &view.runtime)));
    assert_eq!(view.session.borrow().app.notice(), Some(failure.message));
    let log = fs::read_to_string(logs.join("tvi.log"))?;
    let record: serde_json::Value = serde_json::from_str(&log)?;
    assert_eq!(record["event"], "pdf.search_failed");
    assert_eq!(record["stage"], "pdf_render");
    assert_eq!(record["reason"], "fixture");
    assert!(record.get("document").is_none());
    assert!(!log.contains(TERM) && !log.contains("kana-probe.pdf"));

    let check = |view: &View| {
        let state = view.session.borrow();
        assert_eq!(
            (
                state.app.document_id(),
                state.app.pdf().map(|page| page.index),
                state.app.top_row(),
                state.app.search_selection(),
            ),
            before
        );
        assert!(
            state
                .pdf
                .placed(&state.app)
                .is_some_and(|placed| Rc::ptr_eq(&placed, &second))
        );
    };
    check(&view);

    view.key(&second, Char('n'));
    poll_pdf(&view.session, &view.runtime);
    view.key(&second, Esc);
    view.sync()?;
    assert_eq!(view.session.borrow().app.notice(), None);
    check(&view);

    // A rejected extraction request also leaves the committed match untouched.
    view.key(&second, Char('/'));
    {
        let mut state = view.session.borrow_mut();
        for _ in 0..=16 * 1024 {
            state.app.apply(Action::Insert('x'), second.as_ref(), "");
        }
    }
    view.key(&second, Enter);
    view.sync()?;
    assert_eq!(
        view.session.borrow().app.notice(),
        Some("PDF is too large to search")
    );
    assert_eq!(view.session.borrow().app.search_query(), TERM);
    check(&view);
    Ok(())
}

struct View {
    session: Rc<RefCell<Session>>,
    runtime: Runtime,
    terminal: Terminal<TestBackend>,
    active: Option<PlacedPage>,
    kitty: KittyClip,
    input: InputQueue,
}

impl View {
    fn open() -> Result<(Self, tempfile::TempDir), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("kana-probe.pdf");
        fs::write(&path, crate::test_pdf::japanese_text_pages())?;
        let view = Self::new()?;
        assert!(matches!(
            view.session.open_file(BODY_VIEW, &path)?,
            FileOpen::Pending
        ));
        Ok((view, dir))
    }

    fn new() -> Result<Self, Box<dyn Error>> {
        let session = Rc::new(RefCell::new(Session::with_images(
            Document::new("start.md".into(), "Start".into(), "start.md".into(), vec![]),
            KITTY,
            None,
        )));
        let (runtime, error) = Runtime::load_from(session.clone(), None);
        assert!(error.is_none());
        let mut view = Self {
            session,
            runtime,
            terminal: Terminal::new(TestBackend::new(32, VIEW_ROWS + 1))?,
            active: None,
            kitty: KittyClip::default(),
            input: InputQueue::default(),
        };
        view.sync_width()?;
        Ok(view)
    }

    fn sync_width(&mut self) -> Result<(), Box<dyn Error>> {
        let mut retired = Vec::new();
        self.session.borrow_mut().sync_pdf_layout(
            self.terminal.size()?.width,
            KITTY,
            &mut self.active,
            &mut retired,
        );
        self.kitty.retire(retired);
        Ok(())
    }

    fn sync(&mut self) -> Result<PlacedPage, Box<dyn Error>> {
        self.sync_width()?;
        wait(&self.session, &self.runtime)?;
        self.sync_width()?;
        let layout = self.active.clone().ok_or("missing live PDF placement")?;
        self.draw(&layout)?;
        Ok(layout)
    }

    fn draw(&mut self, layout: &PlacedPage) -> Result<(), Box<dyn Error>> {
        self.terminal.draw(|frame| {
            self.session.borrow_mut().draw(frame, layout, &self.runtime);
        })?;
        self.kitty.receive(self.terminal.backend().buffer())?;
        Ok(())
    }

    fn press(&mut self, rows: &impl ViewRows, code: KeyCode, modifiers: KeyModifiers) {
        assert!(
            self.input.dispatch(
                Some(KeyEvent::new(code, modifiers)),
                &self.session,
                &self.runtime,
                rows,
                "",
            ),
            "key was not dispatched: {code:?}"
        );
    }

    fn key(&mut self, layout: &PlacedPage, code: KeyCode) {
        self.press(layout.as_ref(), code, KeyModifiers::NONE);
    }

    fn command(&mut self, layout: &PlacedPage, command: &str) {
        self.key(layout, Char(':'));
        for ch in command.chars() {
            self.key(layout, Char(ch));
        }
        self.key(layout, Enter);
    }

    fn native_page(&self) -> Result<RgbaImage, Box<dyn Error>> {
        let state = self.session.borrow();
        Ok(state
            .app
            .pdf()
            .ok_or("no PDF page")?
            .decode()
            .map_err(io::Error::from)?
            .into_rgba8())
    }

    fn query(&mut self, layout: &PlacedPage, key: char, query: &str) -> Result<(), Box<dyn Error>> {
        self.key(layout, Char(key));
        let direction = if key == '/' {
            Direction::Forward
        } else {
            Direction::Backward
        };
        assert_eq!(self.session.borrow().app.mode(), Mode::Search(direction));
        for ch in query.chars() {
            self.key(layout, Char(ch));
        }
        self.draw(layout)?;
        assert_eq!(
            self.terminal.backend().buffer()[(0, VIEW_ROWS)].symbol(),
            key.to_string()
        );
        self.key(layout, Enter);
        assert_eq!(self.session.borrow().app.mode(), Mode::Normal);
        Ok(())
    }

    fn expect_hit(
        &mut self,
        hit: &Hit,
        color: usize,
        index: usize,
    ) -> Result<PlacedPage, Box<dyn Error>> {
        let layout = self.sync()?;
        let state = self.session.borrow();
        assert_eq!(
            state.app.search_selection(),
            Some((index, 3)),
            "{:?}",
            state.app.notice()
        );
        assert!(state.app.search_highlights_enabled());
        drop(state);
        let status: String = (0..self.terminal.size()?.width)
            .map(|x| self.terminal.backend().buffer()[(x, VIEW_ROWS)].symbol())
            .collect();
        assert!(status.contains(&format!("[{index}/3]")), "{status:?}");
        self.check_clip(&layout, hit, color)?;
        Ok(layout)
    }

    fn check_clip(
        &mut self,
        layout: &PlacedPage,
        hit: &Hit,
        color: usize,
    ) -> Result<usize, Box<dyn Error>> {
        for _ in 0..8 {
            self.draw(layout)?;
            let buffer = self.terminal.backend().buffer();
            let session = self.session.borrow();
            let top = session.app.top_row();
            let body = ui::body_area(buffer.area, session.app.mode());
            if !layout.pdf_transfer_pending(body, top) {
                let clip = self.kitty.clip(buffer, body, CELL_W, CELL_H)?;
                let page = session.app.pdf().ok_or("missing rendered PDF")?;
                assert_eq!(page.index + 1, hit.page);
                let native = page.decode().map_err(io::Error::from)?.into_rgba8();
                verify_ink(&clip, &native, layout.rows().len(), top, hit, color)?;
                return Ok(top);
            }
        }
        Err("visible PDF tiles did not finish transferring".into())
    }
}

fn verify_ink(
    clip: &RgbaImage,
    native: &RgbaImage,
    rows: usize,
    top: usize,
    hit: &Hit,
    color: usize,
) -> Result<(), Box<dyn Error>> {
    // Reference: fit the whole WinRT raster as one image (these geometries
    // have no cell padding). Crop it without using any Kitty placement data.
    let height = u32::try_from(rows)? * CELL_H;
    let width = clip.width();
    let start = u32::try_from(top)? * CELL_H;
    let end = start + clip.height();
    let fitted = imageops::resize(native, width, height, imageops::FilterType::Triangle);
    let bounds = ink_bounds(&fitted);
    let full_ink = bounds[color]
        .as_ref()
        .ok_or("WinRT raster lacks the expected glyphs")?;
    assert!(
        full_ink.y.start >= start && full_ink.y.end <= end,
        "page {}: full ink {full_ink:?} outside viewport pixels {start}..{end}",
        hit.page
    );
    if hit.page == 1 {
        let [Some(upper), None, Some(lower)] = &bounds else {
            return Err("wrong colors in first-page raster".into());
        };
        assert!(lower.y.start >= upper.y.end + clip.height());
        let other = if color == 0 { lower } else { upper };
        assert!(other.y.end <= start || other.y.start >= end);
    } else {
        assert!(bounds[0].is_none() && bounds[2].is_none());
    }
    let to_pdf_x =
        |px| hit.media.llx + f64::from(px) / f64::from(width) * (hit.media.urx - hit.media.llx);
    assert!(
        (to_pdf_x(full_ink.x.start) - hit.x).abs() < 5.,
        "{hit:?} vs {full_ink:?}"
    );
    assert!(
        (to_pdf_x(full_ink.x.end) - hit.right).abs() < 8.,
        "{hit:?} vs {full_ink:?}"
    );

    // Independently decode the actual displayed pixels, including Kitty's
    // tile-local row offsets. Comparing the crop catches wrong tile slices
    // even when every terminal cell still contains a placeholder.
    let x0 = ((hit.x / hit.media.urx) * f64::from(width)).floor();
    let x1 = ((hit.right / hit.media.urx) * f64::from(width)).ceil();
    let y0 = ((hit.media.ury - hit.y - 36.) / hit.media.ury * f64::from(height)).floor();
    let y1 = ((hit.media.ury - hit.y + 36. * 0.22) / hit.media.ury * f64::from(height)).ceil();
    assert!(
        clip.enumerate_pixels().all(|(x, y, actual)| {
            let y = y + start;
            let original = fitted.get_pixel(x, y).0;
            let expected = if (x0..x1).contains(&f64::from(x)) && (y0..y1).contains(&f64::from(y)) {
                blended(original, [255, 255, 0])
            } else {
                original
            };
            actual.0 == expected
        }),
        "page {} at {} cols: Kitty viewport cropped the wrong pixels; got {:?}",
        hit.page,
        width / CELL_W,
        ink_bounds(clip)
    );
    Ok(())
}

fn blended(mut source: [u8; 4], tint: [u16; 3]) -> [u8; 4] {
    for (value, color) in source.iter_mut().zip(tint) {
        *value = u8::try_from((u16::from(*value) * 7 + color * 3) / 10).unwrap_or_default();
    }
    source
}
