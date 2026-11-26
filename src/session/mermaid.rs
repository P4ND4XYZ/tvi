use std::{
    cell::Cell,
    io,
    ops::Range,
    time::{Duration, Instant},
};

use image::DynamicImage;
use serde_json::json;

use crate::{
    app::{App, Document},
    images::{self, Budget},
    logging::{self, Level},
    markdown::{MarkdownResources, RenderedMarkdown, mermaid::extract, tui_markdown::TuiMarkdown},
    mermaid::{CacheIssue, Conversion, Failure, Mermaid, Stage, TimeoutScope},
    terminal::ImageMode,
};

mod worker;
use worker::Request;
pub(super) use worker::{Event, Jobs};

pub(super) struct PreparedDisplay {
    pub document: RenderedMarkdown,
    pub images: Vec<Option<DynamicImage>>,
    placements: Vec<Placement>,
    batch: Option<Batch>,
    mermaid_failures: Cell<usize>,
}

impl PreparedDisplay {
    fn place_completed(
        &mut self,
        markdown: &str,
        resources: &mut MarkdownResources,
        added: Vec<(usize, DynamicImage)>,
    ) -> Vec<Range<usize>> {
        if added.is_empty() {
            return Vec::new();
        }
        let mut removed = Vec::new();
        // Code ranges are in the old display. Remove only ranges for newly
        // placed diagrams; already-rendered diagrams occupy one empty line.
        let mut ranges = self.document.expanded_diagrams.iter();
        let mut decoded = added.into_iter().peekable();
        for (ordinal, slot) in &mut self.placements {
            if slot.is_none() {
                let range = ranges.next();
                if decoded.peek().is_some_and(|(id, _)| id == ordinal)
                    && let Some((_, image)) = decoded.next()
                {
                    removed.extend(range.cloned());
                    *slot = Some(self.images.len());
                    self.images.push(Some(image));
                }
            }
        }
        resources.successful_diagrams = Some(successful(&self.placements));
        self.document = TuiMarkdown.render_with_diagrams(markdown, 0, resources, &self.placements);
        removed
    }
}

type Placement = (usize, Option<usize>);

struct Batch {
    started: Instant,
    name: String,
    cache_hits: usize,
    converted: usize,
    failures: usize,
}

impl Batch {
    fn log(&self) {
        logging::emit(
            Level::Debug,
            "mermaid.summary",
            json!({
                "document": self.name, "cache_hits": self.cache_hits, "converted": self.converted,
                "fallbacks": self.failures, "elapsed_ms": self.started.elapsed().as_millis(),
            }),
        );
    }

    fn record_diagram(
        &mut self,
        line: usize,
        conversion: Conversion<DynamicImage>,
        timeout_limit: Option<Duration>,
        pending: &mut Vec<usize>,
    ) -> Option<DynamicImage> {
        cache_event(conversion.cache.lookup_issue, "lookup", "regenerate");
        cache_event(conversion.cache.save_issue, "save", "display_without_cache");
        self.failures += usize::from(conversion.result.is_err());
        match conversion.result {
            Ok(image) => {
                self.cache_hits += usize::from(conversion.cache.hit);
                self.converted += usize::from(!conversion.cache.hit);
                return Some(image);
            }
            Err(Failure::Launch { kind, os_code }) => {
                let mut fields = json!({
                    "document": self.name, "reason": io_reason(kind), "fallback": "code",
                });
                if let Some(code) = os_code {
                    fields["os_code"] = code.into();
                }
                logging::emit(Level::Warn, "mermaid.launch_failed", fields);
                self.fail_remaining(pending);
            }
            Err(Failure::Timeout(scope)) => {
                let mut fields = json!({
                    "document": self.name,
                    "scope": match scope {
                        TimeoutScope::Diagram => "diagram",
                        TimeoutScope::Document => "document",
                    },
                    "limit_ms": timeout_limit.map(|limit| limit.as_millis()),
                });
                if scope == TimeoutScope::Document {
                    fields["remaining"] = (pending.len() + 1).into();
                    self.fail_remaining(pending);
                } else {
                    fields["line"] = line.into();
                }
                logging::emit(Level::Warn, "mermaid.timeout", fields);
            }
            Err(Failure::Unavailable) => self.fail_remaining(pending),
            Err(failure) => {
                conversion_failed(&self.name, line, failure, conversion.elapsed.as_millis());
            }
        }
        None
    }

    fn fail_remaining(&mut self, pending: &mut Vec<usize>) {
        self.failures += pending.len();
        pending.clear();
    }
}

pub(super) fn prepare(
    app: &mut App,
    mode: ImageMode,
    renderer: Option<&Mermaid>,
    jobs: &mut Jobs,
) -> PreparedDisplay {
    let Some((path, markdown, resources)) = Document::markdown_display_input(app) else {
        unreachable!("only Markdown documents use Markdown preparation");
    };
    let mut budget = Budget::default();
    // Local images keep their original indices and have priority in the shared budget.
    let mut images = if mode == ImageMode::Text {
        Vec::new()
    } else {
        images::load(path, &resources.images, &mut budget)
    };
    let renderer = renderer.filter(|_| mode != ImageMode::Text);
    let previous = resources.successful_diagrams.take();
    let unshown = std::mem::take(&mut resources.unshown_diagrams);
    let pending = std::mem::take(&mut resources.pending_diagrams);
    let mut diagrams = if renderer.is_some() || previous.as_ref().is_some_and(|ids| !ids.is_empty())
    {
        extract(markdown)
    } else {
        Vec::new()
    };
    if let Some(previous) = &previous {
        diagrams.retain(|diag| {
            previous.binary_search(&diag.ordinal).is_ok()
                || unshown.binary_search(&diag.ordinal).is_ok()
                || pending.binary_search(&diag.ordinal).is_ok()
        });
    }
    let mut batch = Batch {
        started: Instant::now(),
        name: logging::basename(path),
        cache_hits: 0,
        converted: 0,
        failures: 0,
    };
    let cache = renderer
        .filter(|_| !diagrams.is_empty())
        .map(Mermaid::start_document);
    let available = cache
        .as_ref()
        .is_some_and(crate::mermaid::Document::available);
    if previous.is_none() && !diagrams.is_empty() {
        logging::emit(
            Level::Debug,
            "mermaid.detect",
            json!({"available": available, "action": if available { "convert" } else { "show_code" }}),
        );
    }
    let mut todo = Vec::new();
    let mut placements = Vec::new();
    let mut newly_restored = Vec::new();
    for diagram in diagrams {
        let ordinal = diagram.ordinal;
        let restoring = previous
            .as_ref()
            .is_some_and(|ids| ids.binary_search(&ordinal).is_ok());
        let cache_only = unshown.binary_search(&ordinal).is_ok();
        // Only previously shown diagrams use image coordinates in the old display.
        // For a buffered success, keep the code at first so its position can be remapped.
        let image = cache
            .as_ref()
            .filter(|_| previous.is_none() || restoring || cache_only)
            .and_then(|doc| cached_image(doc, &diagram.source, &budget, restoring || cache_only));
        if cache_only {
            if let Some(image) = image {
                budget.charge(&image);
                batch.cache_hits += 1;
                newly_restored.push((ordinal, image));
            }
            placements.push((ordinal, None));
            continue;
        }
        let slot = image.map(|image| {
            batch.cache_hits += 1;
            budget.charge(&image);
            let slot = images.len();
            images.push(Some(image));
            slot
        });
        if slot.is_none() && !restoring {
            if available {
                todo.push(diagram);
            } else {
                batch.failures += 1;
            }
        }
        placements.push((ordinal, slot));
    }

    let document = TuiMarkdown.render_with_diagrams(markdown, 0, resources, &placements);
    let placed = document
        .images
        .iter()
        .filter(|slot| slot.image >= resources.images.len())
        .count();
    let uncertain = placed + document.expanded_diagrams.len() != placements.len();
    let expansions = placements
        .iter()
        .filter(|(_, slot)| slot.is_none())
        .zip(&document.expanded_diagrams)
        .filter(|((ordinal, _), _)| {
            previous
                .as_ref()
                .is_some_and(|ids| ids.binary_search(ordinal).is_ok())
        })
        .map(|(_, range)| range.clone())
        .collect::<Vec<_>>();
    if uncertain {
        images.truncate(resources.images.len());
        batch.failures += todo.len() + batch.cache_hits;
        batch.cache_hits = 0;
        todo.clear();
        placements.clear();
        newly_restored.clear();
    }
    resources.successful_diagrams = Some(successful(&placements));
    resources.pending_diagrams = todo.iter().map(|diagram| diagram.ordinal).collect();
    let failures = if available { batch.failures } else { 0 };
    let batch = if let Some(renderer) = renderer.filter(|_| !todo.is_empty()) {
        jobs.queue(Request {
            renderer: renderer.clone(),
            diagrams: todo,
            budget,
        });
        Some(batch)
    } else {
        if previous.is_none() && (batch.cache_hits + batch.failures > 0) {
            batch.log();
        }
        None
    };
    Document::restore_display_positions(
        app,
        document.search_text(),
        &expansions,
        previous.is_some() && uncertain,
    );
    let mut display = PreparedDisplay {
        document,
        images,
        placements,
        batch,
        mermaid_failures: Cell::new(failures),
    };
    if !newly_restored.is_empty()
        && let Some((_, markdown, resources)) = Document::markdown_display_input(app)
    {
        let removed = display.place_completed(markdown, resources, newly_restored);
        Document::collapse_diagram_positions(app, display.document.search_text(), &removed);
    }
    display
}

fn cached_image(
    document: &crate::mermaid::Document<'_>,
    source: &str,
    budget: &Budget,
    restoring: bool,
) -> Option<DynamicImage> {
    let image = match document.cached_png(source) {
        Ok(Some(bytes)) => budget
            .cached_png(&bytes)
            .map_err(|()| CacheIssue::InvalidEntry),
        Ok(None) => return None,
        Err(issue) => Err(issue),
    };
    match image {
        Ok(image) => image,
        Err(issue) => {
            if restoring {
                // New conversions report cache failures from the worker instead.
                cache_event(Some(issue), "lookup", "show_code");
            }
            None
        }
    }
}

fn successful(placements: &[Placement]) -> Vec<usize> {
    placements
        .iter()
        .filter_map(|&(ordinal, slot)| slot.map(|_| ordinal))
        .collect()
}

pub(super) fn archive_completed(resources: &mut MarkdownResources, completed: Vec<Event>) {
    for event in completed {
        if let Event::Diagram {
            ordinal,
            conversion,
            ..
        } = event
            && let Ok(pending) = resources.pending_diagrams.binary_search(&ordinal)
        {
            resources.pending_diagrams.remove(pending);
            if conversion.result.is_ok() {
                // Reuse the cache on return, but never regenerate a completed diagram.
                resources.unshown_diagrams.push(ordinal);
            } else if Event::ends_batch(&conversion.result) {
                resources.pending_diagrams.clear();
            }
        }
    }
}

pub(super) fn publish(app: &mut App, display: &mut PreparedDisplay, events: Vec<Event>) {
    let Some(batch) = &mut display.batch else {
        return;
    };
    let Some((_, markdown, resources)) = Document::markdown_display_input(app) else {
        return;
    };
    let mut added = Vec::new();
    let mut finished = false;
    for event in events {
        match event {
            Event::Diagram {
                ordinal,
                line,
                conversion,
                timeout_limit,
            } => {
                let Ok(pending) = resources.pending_diagrams.binary_search(&ordinal) else {
                    continue;
                };
                resources.pending_diagrams.remove(pending);
                if let Some(image) = batch.record_diagram(
                    line,
                    conversion,
                    timeout_limit,
                    &mut resources.pending_diagrams,
                ) {
                    added.push((ordinal, image));
                }
            }
            Event::Stopped(error) => {
                if !resources.pending_diagrams.is_empty() {
                    logging::emit(
                        Level::Warn,
                        "mermaid.worker_failed",
                        json!({
                            "document": batch.name,
                            "reason": error.map_or("stopped", io_reason),
                            "fallback": "code",
                        }),
                    );
                    batch.fail_remaining(&mut resources.pending_diagrams);
                }
                finished = true;
            }
        }
    }
    if finished {
        batch.log();
        display
            .mermaid_failures
            .set(display.mermaid_failures.get() + batch.failures);
        display.batch = None;
    }
    let removed = display.place_completed(markdown, resources, added);
    if !removed.is_empty() {
        Document::collapse_diagram_positions(app, display.document.search_text(), &removed);
    }
}

pub(super) fn show_notice(app: &mut App, display: &PreparedDisplay) {
    let count = display.mermaid_failures.replace(0);
    if count != 0 {
        app.set_notice(format!("Mermaidの図 {count} 件をコードで表示します"));
    }
}

fn io_reason(kind: io::ErrorKind) -> &'static str {
    logging::io_reason(&io::Error::from(kind))
}

fn conversion_failed(document: &str, line: usize, failure: Failure, elapsed_ms: u128) {
    let stage = match failure.stage() {
        Stage::Prepare => "prepare",
        Stage::Execute => "convert",
        Stage::Output => "validate_png",
        _ => return,
    };
    let reason = match failure {
        Failure::Prepare(kind) | Failure::Wait(kind) | Failure::ReadOutput(kind) => io_reason(kind),
        Failure::Exit { code: Some(_) } => "nonzero_exit",
        Failure::Exit { code: None } => "terminated",
        Failure::MissingOutput => "missing_output",
        Failure::InvalidPng => "invalid_png",
        _ => return,
    };
    let mut fields = json!({
        "document": document, "line": line, "stage": stage, "reason": reason,
        "elapsed_ms": elapsed_ms, "fallback": "code",
    });
    if let Failure::Exit { code: Some(code) } = failure {
        fields["exit_code"] = code.into();
    }
    logging::emit(Level::Warn, "mermaid.convert_failed", fields);
}

fn cache_event(issue: Option<CacheIssue>, operation: &'static str, action: &'static str) {
    let reason = match issue {
        None => return,
        Some(CacheIssue::InvalidEntry) => "invalid_entry",
        Some(CacheIssue::Read(kind) | CacheIssue::Write(kind)) => io_reason(kind),
        Some(CacheIssue::Busy) => "busy",
        Some(CacheIssue::Capacity) => "capacity",
    };
    logging::emit(
        Level::Warn,
        "mermaid.cache_failed",
        json!({
            "operation": operation, "reason": reason, "action": action,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{Action, Direction, Motion, ViewAnchor, ViewRows},
        lua::Host,
        session::{BODY_VIEW, Session, read_document},
        ui::layout::Layout,
    };
    use std::{
        cell::RefCell, env, fs, path::Path, process::Command, rc::Rc, thread, time::Duration,
    };

    fn tick(session: &mut Session) {
        let events = session.mermaid_jobs.poll();
        session.publish_mermaid(events);
    }

    fn until(mut ready: impl FnMut() -> bool) {
        let started = Instant::now();
        while !ready() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "Mermaid test condition timed out"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn settle(session: &mut Session) -> Rc<PreparedDisplay> {
        until(|| {
            tick(session);
            session.display().batch.is_none()
        });
        session.display()
    }

    fn install_fake(bin: &Path) -> std::path::PathBuf {
        let target = bin.join(if cfg!(windows) { "mmdc.exe" } else { "mmdc" });
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_mmdc.rs");
        let mut child = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .args(["--edition=2024", "--crate-name=fake_mmdc"])
            .arg(source)
            .arg("-o")
            .arg(&target)
            .current_dir(bin)
            .spawn()
            .unwrap();
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "fake renderer compilation failed: {status}"
                );
                return target;
            }
            if started.elapsed() > Duration::from_secs(30) {
                drop(child.kill());
                drop(child.wait());
                panic!("fake renderer compilation timed out");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn mixed_document_reuses_images_and_search_on_resize_and_history_return() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        install_fake(dir);
        let renderer = Mermaid::isolated(
            env::join_paths([dir]).unwrap(),
            dir.into(),
            dir.join("cache"),
            4096,
        );
        let file = dir.join("source.md");
        let markdown = "````text\n```mermaid\nexample\n```\n````\n\n> ```mermaid\n> quoted\n> ```\n\n- ```mermaid\n  listed\n  ```\n\n```mermaid\nsecret_success\n```\n\n![blue](blue.png)\n\n[next](next.md)\n\n```mermaid\nexit";
        fs::write(&file, markdown).unwrap();
        fs::write(dir.join("next.md"), "~~~mermaid\nlinked_diagram\n~~~").unwrap();
        image::RgbImage::from_pixel(2, 2, image::Rgb([0, 0, 255]))
            .save(dir.join("blue.png"))
            .unwrap();
        let logs = dir.join("logs");
        let mode = ImageMode::Kitty {
            cell_width: 10,
            cell_height: 20,
        };
        let session = RefCell::new(Session::with_images(
            read_document(&file).unwrap(),
            mode,
            Some(renderer),
        ));
        let initial = logging::capture_for_test(&logs, || {
            let mut session = session.borrow_mut();
            let first = session.display();
            assert_eq!(first.document.images.len(), 1);
            assert!(first.document.search_text().contains("secret_success"));
            drop(first);
            settle(&mut session)
        });
        let text = initial.document.search_text();
        assert_eq!(session.borrow().app.links().len(), 1);
        assert_eq!(initial.document.images.len(), 2);
        let colors: Vec<_> = initial
            .document
            .images
            .iter()
            .map(|slot| {
                initial.images[slot.image]
                    .as_ref()
                    .unwrap()
                    .to_rgb8()
                    .get_pixel(0, 0)
                    .0
            })
            .collect();
        assert_eq!(colors, [[255, 0, 0], [0, 0, 255]]);
        assert!(initial.document.images[0].line < initial.document.images[1].line);
        for visible in ["example", "quoted", "listed", "exit"] {
            assert!(text.contains(visible));
        }
        assert!(!text.contains("secret_success"));
        assert_eq!(fs::read(&file).unwrap(), markdown.as_bytes());

        let layout = Layout::with_images(&initial.document, 30, &initial.images, mode);
        let rows: Vec<_> = initial
            .document
            .images
            .iter()
            .map(|placement| {
                layout.row_at(ViewAnchor {
                    byte: initial.document.line_ranges()[placement.line].start,
                    image_row: 0,
                })
            })
            .collect();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 40)).unwrap();
        terminal
            .draw(|frame| layout.draw_images(frame, frame.area(), 0))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for row in rows {
            assert!(buffer[(0, row as u16)].symbol().contains('\u{10eeee}'));
        }
        let collapsed = Layout::with_images(&initial.document, 0, &initial.images, mode);
        assert_eq!(collapsed.len(), 0);

        let text_display =
            Session::with_images(read_document(&file).unwrap(), ImageMode::Text, None).display();
        assert!(
            text_display
                .document
                .search_text()
                .contains("secret_success")
        );
        assert!(text_display.images.is_empty());

        let mut current = session.borrow_mut();
        show_notice(&mut current.app, &initial);
        assert_eq!(
            current.app.notice(),
            Some("Mermaidの図 1 件をコードで表示します")
        );
        current.app.reflow(&layout, 1);
        current
            .app
            .apply(Action::StartSearch(Direction::Forward), &layout, text);
        for ch in "exit".chars() {
            current.app.apply(Action::Insert(ch), &layout, text);
        }
        current.app.apply(Action::Submit, &layout, text);
        assert_eq!(current.app.search_selection(), Some((1, 1)));
        let id = current.app.links()[0].id;
        let selection = current.app.highlights().1.cloned().unwrap();
        let top = current.app.top_row();
        let released = std::rc::Rc::downgrade(&initial);
        assert_eq!(initial.mermaid_failures.get(), 0);
        drop(current);
        drop(layout);
        drop(initial);
        let path = session.resolve_link(BODY_VIEW, id).unwrap();
        session.open_file(BODY_VIEW, &path).unwrap();
        assert!(
            released.upgrade().is_none(),
            "history must not retain decoded pixels"
        );
        image::RgbImage::from_pixel(2, 2, image::Rgb([0, 255, 0]))
            .save(dir.join("blue.png"))
            .unwrap();
        let linked = {
            let mut session = session.borrow_mut();
            drop(session.display());
            settle(&mut session)
        };
        assert_eq!(linked.document.images.len(), 1);
        assert!(!linked.document.search_text().contains("linked_diagram"));
        let linked_layout = Layout::with_images(&linked.document, 30, &linked.images, mode);
        let mut current = session.borrow_mut();
        current.app.apply(
            Action::JumpBack,
            &linked_layout,
            linked.document.search_text(),
        );
        let restored = current.display();
        assert_eq!(restored.document.images.len(), 2);
        assert_eq!(
            restored.images[0]
                .as_ref()
                .unwrap()
                .to_rgb8()
                .get_pixel(0, 0)
                .0,
            [0, 255, 0]
        );
        assert_eq!(
            restored.images[1]
                .as_ref()
                .unwrap()
                .to_rgb8()
                .get_pixel(0, 0)
                .0,
            [255, 0, 0]
        );
        assert_eq!(fs::read(&file).unwrap(), markdown.as_bytes());
        let restored_layout = Layout::with_images(&restored.document, 30, &restored.images, mode);
        current.app.reflow(&restored_layout, 1);
        assert_eq!(current.app.top_row(), top);
        assert_eq!(current.app.highlights().1, Some(&selection));
        let narrow = Layout::with_images(&restored.document, 8, &restored.images, mode);
        current.app.reflow(&narrow, 1);
        assert_eq!(current.app.search_selection(), Some((1, 1)));
        assert!(std::rc::Rc::ptr_eq(&restored, &current.display()));

        let logs = fs::read_to_string(logs.join("tvi.log")).unwrap();
        let failure = logs
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|record| record["event"] == "mermaid.convert_failed")
            .unwrap();
        assert_eq!(failure["document"], "source.md");
        assert_eq!(failure["line"], 23);
        assert_eq!(failure["exit_code"], 17);
        assert_eq!(failure["fallback"], "code");
        assert!(!logs.contains("private renderer data") && !logs.contains("secret_success"));
        assert!(!logs.contains(&*dir.to_string_lossy().replace('\\', "\\\\")));
    }

    #[test]
    fn missing_and_corrupt_history_cache_restore_code_and_refresh_search() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let executable = install_fake(dir);
        let cache = dir.join("cache");
        let renderer = Mermaid::isolated(
            env::join_paths([dir]).unwrap(),
            dir.into(),
            cache.clone(),
            4096,
        );
        let file = dir.join("source.md");
        fs::write(&file, "```mermaid\nfind missing\n```\n\n```mermaid\nfind corrupt\n```\n\n[next](next.md)\n\nfind after").unwrap();
        fs::write(dir.join("next.md"), "next").unwrap();
        let mode = ImageMode::Kitty {
            cell_width: 10,
            cell_height: 20,
        };
        let session = RefCell::new(Session::with_images(
            read_document(&file).unwrap(),
            mode,
            Some(renderer),
        ));
        let mut current = session.borrow_mut();
        drop(current.display());
        let prepared = settle(&mut current);
        drop(current);
        let layout = Layout::with_images(&prepared.document, 40, &prepared.images, mode);
        let mut current = session.borrow_mut();
        let text = prepared.document.search_text();
        current.app.reflow(&layout, 1);
        current
            .app
            .apply(Action::StartSearch(Direction::Forward), &layout, text);
        for ch in "find".chars() {
            current.app.apply(Action::Insert(ch), &layout, text);
        }
        current.app.apply(Action::Submit, &layout, text);
        assert_eq!(current.app.search_selection(), Some((1, 1)));
        current.app.apply(Action::StartCommand, &layout, text);
        for ch in "noh".chars() {
            current.app.apply(Action::Insert(ch), &layout, text);
        }
        current.app.apply(Action::Submit, &layout, text);
        assert!(current.app.highlights().0.is_empty());
        let id = current.app.links()[0].id;
        drop(current);
        let path = session.resolve_link(BODY_VIEW, id).unwrap();
        session.open_file(BODY_VIEW, &path).unwrap();
        let mut entries = fs::read_dir(&cache)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "png"));
        let missing = entries.next().unwrap();
        let corrupt = entries.next().unwrap();
        assert!(entries.next().is_none());
        fs::remove_file(&missing).unwrap();
        fs::write(&corrupt, b"\x89PNG\r\n\x1a\n").unwrap();
        // Still detectable; any attempted launch would emit a launch warning.
        fs::write(executable, "not an executable").unwrap();
        let logs = dir.join("logs");
        let mut current = session.borrow_mut();
        current.app.apply(Action::JumpBack, &layout, text);
        let restored = logging::capture_for_test(&logs, || current.display());
        assert!(restored.document.images.is_empty());
        let text = restored.document.search_text();
        assert!(text.contains("find missing") && text.contains("find corrupt"));
        assert_eq!(current.app.search_query(), "find");
        assert_eq!(current.app.search_selection(), Some((3, 3)));
        assert!(current.app.highlights().0.is_empty());
        let restored_layout = Layout::with_images(&restored.document, 40, &restored.images, mode);
        current.app.reflow(&restored_layout, 1);
        assert_eq!(
            restored_layout
                .anchor_at(current.app.top_row())
                .unwrap()
                .byte,
            text.find("find after").unwrap()
        );
        assert!(!missing.exists());
        assert_eq!(fs::read(corrupt).unwrap(), b"\x89PNG\r\n\x1a\n");
        let log = fs::read_to_string(logs.join("tvi.log")).unwrap();
        assert_eq!(log.lines().count(), 1);
        assert!(log.contains("mermaid.cache_failed") && !log.contains("mermaid.launch_failed"));
    }

    #[test]
    fn background_conversion_preserves_navigation_and_search_when_a_diagram_appears() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        install_fake(dir);
        let file = dir.join("source.md");
        fs::write(&file, "first\n\n```mermaid\ngate\n```\n\nlast needle").unwrap();
        let mode = ImageMode::Kitty {
            cell_width: 10,
            cell_height: 20,
        };
        let mut session = Session::with_images(
            read_document(&file).unwrap(),
            mode,
            Some(test_renderer(dir)),
        );

        let initial = session.display();
        assert!(initial.document.images.is_empty());
        drop(initial);
        until(|| {
            tick(&mut session);
            dir.join("gate-started").exists()
        });
        let initial = session.display();
        let text = initial.document.search_text();
        assert!(text.contains("gate"));
        let layout = Layout::with_images(&initial.document, 40, &initial.images, mode);
        session.app.reflow(&layout, 1);
        session.apply(Action::Move(Motion::LineDown), &layout, text);
        assert!(session.app.top_row() > 0);
        session.apply(Action::StartSearch(Direction::Forward), &layout, text);
        for ch in "needle".chars() {
            session.apply(Action::Insert(ch), &layout, text);
        }
        session.apply(Action::Submit, &layout, text);
        session.app.reflow(&layout, 1);
        assert_eq!(session.app.search_selection(), Some((1, 1)));
        assert_eq!(
            layout.anchor_at(session.app.top_row()).unwrap().byte,
            text.find("last needle").unwrap()
        );
        drop(layout);
        drop(initial);

        fs::write(dir.join("gate-release"), b"done").unwrap();
        let rendered = settle(&mut session);
        let text = rendered.document.search_text();
        assert!(!text.contains("gate"));
        assert_eq!(rendered.document.images.len(), 1);
        let layout = Layout::with_images(&rendered.document, 40, &rendered.images, mode);
        session.app.reflow(&layout, 1);
        assert_eq!(
            layout.anchor_at(session.app.top_row()).unwrap().byte,
            text.find("last needle").unwrap()
        );
        assert_eq!(session.app.search_query(), "needle");
        assert_eq!(session.app.search_selection(), Some((1, 1)));
        assert_eq!(
            session.app.highlights().1.unwrap().start,
            text.find("needle").unwrap()
        );
    }

    #[test]
    fn history_restarts_only_interrupted_conversion_and_stops_the_old_renderer() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        install_fake(dir);
        let file = dir.join("source.md");
        let other = dir.join("other.md");
        fs::write(
            &file,
            "```mermaid\nred\n```\n\n```mermaid\nexit\n```\n\n```mermaid\ngate\n```",
        )
        .unwrap();
        fs::write(&other, "other document").unwrap();
        let mode = ImageMode::Kitty {
            cell_width: 10,
            cell_height: 20,
        };
        let session = RefCell::new(Session::with_images(
            read_document(&file).unwrap(),
            mode,
            Some(test_renderer(dir)),
        ));
        let logs = dir.join("logs");
        logging::capture_for_test(&logs, || {
            let mut current = session.borrow_mut();
            drop(current.display());
            until(|| {
                tick(&mut current);
                dir.join("gate-started").exists()
                    && current.display().document.images.len() == 1
                    && current.app.markdown_resources().pending_diagrams == [2]
            });
            drop(current);
            session.open_file(BODY_VIEW, &other).unwrap();

            let mut current = session.borrow_mut();
            let lock = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(dir.join("gate.lock"))
                .unwrap();
            until(|| lock.try_lock().is_ok());
            drop(lock);
            tick(&mut current);
            let next = current.display();
            assert_eq!(next.document.search_text(), "other document");
            assert!(next.document.images.is_empty());
            let layout = Layout::with_images(&next.document, 40, &next.images, mode);
            current.apply(Action::JumpBack, &layout, next.document.search_text());
            drop(layout);
            drop(next);

            let restored = current.display();
            assert_eq!(restored.document.images.len(), 1);
            assert!(restored.document.search_text().contains("gate"));
            drop(restored);
            until(|| {
                tick(&mut current);
                fs::read(dir.join("gate-started")).is_ok_and(|starts| starts == b"..")
            });
            fs::write(dir.join("gate-release"), b"done").unwrap();
            let rendered = settle(&mut current);
            assert_eq!(rendered.document.images.len(), 2);
            assert!(!rendered.document.search_text().contains("gate"));
            assert!(rendered.document.search_text().contains("exit"));
        });
        let log = fs::read_to_string(logs.join("tvi.log")).unwrap();
        assert_eq!(log.lines().count(), 1);
        assert!(log.contains("mermaid.convert_failed"));
    }

    #[test]
    fn history_does_not_retry_buffered_success_or_failure() {
        for keep_cache in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path();
            install_fake(dir);
            let file = dir.join("source.md");
            let other = dir.join("other.md");
            fs::write(
                &file,
                "```mermaid\ngate\n```\n\n```mermaid\nexit\n```\n\nlast needle",
            )
            .unwrap();
            fs::write(&other, "other document").unwrap();
            let session = history_test_session(&file, test_renderer(dir));

            {
                let mut state = session.borrow_mut();
                drop(state.display());
                tick(&mut state);
                until(|| dir.join("gate-started").is_file());
                let display = state.display();
                let layout =
                    Layout::with_images(&display.document, 40, &display.images, state.image_mode);
                let text = display.document.search_text();
                state.app.reflow(&layout, 1);
                state.apply(Action::StartSearch(Direction::Forward), &layout, text);
                for ch in "needle".chars() {
                    state.apply(Action::Insert(ch), &layout, text);
                }
                state.apply(Action::Submit, &layout, text);
                state.app.reflow(&layout, 1);
                fs::write(dir.join("gate-release"), b"done").unwrap();
                // The worker has queued both results, but there is no second UI poll.
                until(|| state.mermaid_jobs.finished_with_receiver());
                assert!(display.document.images.is_empty());
            }
            session.open_file(BODY_VIEW, &other).unwrap();
            assert!(session.borrow_mut().display().document.images.is_empty());
            if !keep_cache {
                fs::remove_dir_all(dir.join("cache")).unwrap();
            }
            let mut state = session.borrow_mut();
            state.apply(Action::JumpBack, &[], "");
            let restored = state.display();
            assert!(state.app.markdown_resources().pending_diagrams.is_empty());
            assert!(restored.batch.is_none());
            assert_eq!(restored.document.images.len(), usize::from(keep_cache));
            let text = restored.document.search_text();
            assert_eq!(text.contains("gate"), !keep_cache);
            assert!(text.contains("exit"));
            let layout =
                Layout::with_images(&restored.document, 40, &restored.images, state.image_mode);
            state.app.reflow(&layout, 1);
            assert_eq!(
                layout.anchor_at(state.app.top_row()).unwrap().byte,
                text.find("last needle").unwrap()
            );
            assert_eq!(
                state.app.highlights().1.unwrap().start,
                text.find("needle").unwrap()
            );
        }
    }

    #[test]
    fn history_does_not_restart_the_tail_after_a_buffered_document_timeout() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        install_fake(dir);
        let file = dir.join("source.md");
        let other = dir.join("other.md");
        fs::write(
            &file,
            "```mermaid\ngate\n```\n\n```mermaid\nrecord_launch\n```",
        )
        .unwrap();
        fs::write(&other, "other document").unwrap();
        let renderer = test_renderer(dir).with_limits(crate::mermaid::Limits {
            per_diagram: Duration::from_secs(4),
            per_document: Duration::from_millis(900),
        });
        let session = history_test_session(&file, renderer);
        {
            let mut state = session.borrow_mut();
            drop(state.display());
            tick(&mut state);
            until(|| state.mermaid_jobs.finished_with_receiver());
            assert!(dir.join("gate-started").is_file());
        }
        session.open_file(BODY_VIEW, &other).unwrap();
        let mut state = session.borrow_mut();
        state.apply(Action::JumpBack, &[], "");
        let restored = state.display();
        assert!(restored.document.images.is_empty());
        assert!(restored.document.search_text().contains("record_launch"));
        assert!(state.app.markdown_resources().pending_diagrams.is_empty());
        assert!(restored.batch.is_none());
        assert!(!dir.join("renderer-launched").exists());
    }

    fn history_test_session(path: &Path, renderer: Mermaid) -> RefCell<Session> {
        RefCell::new(Session::with_images(
            read_document(path).unwrap(),
            ImageMode::Kitty {
                cell_width: 10,
                cell_height: 20,
            },
            Some(renderer),
        ))
    }

    fn test_renderer(dir: &Path) -> Mermaid {
        Mermaid::isolated(
            env::join_paths([dir]).unwrap(),
            dir.into(),
            dir.join("cache"),
            4096,
        )
        .with_limits(crate::mermaid::Limits {
            per_diagram: Duration::from_secs(8),
            per_document: Duration::from_secs(20),
        })
    }

    #[test]
    fn launch_failure_records_os_code_once_for_the_document() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let executable = dir.join(if cfg!(windows) { "mmdc.exe" } else { "mmdc" });
        fs::write(&executable, "not an executable").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let file = dir.join("source.md");
        fs::write(&file, "```mermaid\nfirst\n```\n\n```mermaid\nsecond\n```").unwrap();
        let logs = dir.join("logs");
        let renderer = Mermaid::isolated(
            env::join_paths([dir]).unwrap(),
            dir.into(),
            dir.join("cache"),
            4096,
        );
        let prepared = logging::capture_for_test(&logs, || {
            let mut session = Session::with_images(
                read_document(&file).unwrap(),
                ImageMode::Kitty {
                    cell_width: 10,
                    cell_height: 20,
                },
                Some(renderer),
            );
            drop(session.display());
            settle(&mut session)
        });
        assert_eq!(prepared.mermaid_failures.get(), 2);
        assert!(prepared.document.images.is_empty());
        let log = fs::read_to_string(logs.join("tvi.log")).unwrap();
        let records: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["event"], "mermaid.launch_failed");
        assert_eq!(records[0]["document"], "source.md");
        assert_eq!(records[0]["fallback"], "code");
        assert!(records[0]["os_code"].as_i64().is_some());
    }

    #[test]
    fn text_mode_skips_available_images_and_renderer() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        install_fake(dir);
        let renderer = Mermaid::isolated(
            env::join_paths([dir]).unwrap(),
            dir.into(),
            dir.join("cache"),
            4096,
        );
        image::RgbImage::new(1, 1)
            .save(dir.join("image.png"))
            .unwrap();
        let file = dir.join("source.md");
        fs::write(
            &file,
            "![diagram](image.png)\n\n```mermaid\nrecord_launch\n```",
        )
        .unwrap();
        let prepared = Session::with_images(
            read_document(&file).unwrap(),
            ImageMode::Text,
            Some(renderer),
        )
        .display();
        assert!(prepared.images.is_empty());
        let text = prepared.document.search_text();
        assert!(text.contains("diagram") && text.contains("record_launch"));
        assert!(
            prepared
                .document
                .text()
                .lines
                .iter()
                .any(|line| line.to_string() == "[img] diagram")
        );
        assert!(!dir.join("renderer-launched").exists());
    }

    #[test]
    fn text_mode_and_missing_renderer_keep_the_code_without_notice() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let file = dir.join("source.md");
        fs::write(&file, "```mermaid\ntext_only\n```").unwrap();
        let missing = || {
            Mermaid::isolated(
                env::join_paths([dir]).unwrap(),
                dir.into(),
                dir.join("cache"),
                4096,
            )
        };
        for mode in [
            ImageMode::Text,
            ImageMode::Kitty {
                cell_width: 10,
                cell_height: 20,
            },
        ] {
            let mut session =
                Session::with_images(read_document(&file).unwrap(), mode, Some(missing()));
            let prepared = session.display();
            assert!(prepared.document.search_text().contains("text_only"));
            assert!(prepared.document.images.is_empty());
            show_notice(&mut session.app, &prepared);
            assert_eq!(session.app.notice(), None);
        }
        assert!(!dir.join("cache").exists());
    }
}
