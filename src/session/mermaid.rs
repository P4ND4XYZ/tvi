use std::{cell::Cell, io, path::Path, time::Instant};

use image::DynamicImage;
use serde_json::json;

use crate::{
    app::App,
    images::{self, Budget},
    logging::{self, Level},
    markdown::{RenderedMarkdown, mermaid::extract, tui_markdown::TuiMarkdown},
    mermaid::{CacheIssue, Failure, Mermaid, Stage, TimeoutScope},
    terminal::ImageMode,
};

pub(super) struct PreparedDisplay {
    pub document: RenderedMarkdown,
    pub images: Vec<Option<DynamicImage>>,
    mermaid_failures: Cell<usize>,
}

type Placement = (usize, Option<usize>);

pub(super) fn prepare(
    app: &mut App,
    mode: ImageMode,
    renderer: Option<&Mermaid>,
) -> PreparedDisplay {
    let Some((path, markdown, resources)) = app.display_input() else {
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
    let restoring = resources.successful_diagrams.is_some();
    let (placements, mut failures) = match &resources.successful_diagrams {
        Some(successful) => (
            restore(markdown, successful, renderer, &mut budget, &mut images),
            0,
        ),
        None => renderer.map_or_else(
            || (Vec::new(), 0),
            |renderer| convert(path, markdown, renderer, &mut budget, &mut images),
        ),
    };
    let document = TuiMarkdown.render_with_diagrams(markdown, 0, resources, &placements);
    let mut successful = Vec::new();
    let placed = document
        .images
        .iter()
        .filter(|slot| slot.image >= resources.images.len())
        .count();
    for &(ordinal, image) in &placements {
        if image.is_some() {
            if placed == 0 {
                failures += 1;
            } else {
                successful.push(ordinal);
            }
        }
    }
    images.truncate(resources.images.len() + placed);
    resources.successful_diagrams = Some(successful);
    // If the adapter could not match its fences, it rendered all blocks as
    // code. Recompute matches regardless, using the old position as an estimate.
    let uncertain = restoring && placed + document.expanded_diagrams.len() != placements.len();
    app.restore_display_positions(
        document.search_text(),
        &document.expanded_diagrams,
        uncertain,
    );
    PreparedDisplay {
        document,
        images,
        mermaid_failures: Cell::new(failures),
    }
}

fn restore(
    markdown: &str,
    successful: &[usize],
    renderer: Option<&Mermaid>,
    budget: &mut Budget,
    images: &mut Vec<Option<DynamicImage>>,
) -> Vec<Placement> {
    if successful.is_empty() {
        return Vec::new();
    }
    let document = renderer.map(Mermaid::start_document);
    extract(markdown)
        .into_iter()
        .filter(|diagram| successful.binary_search(&diagram.ordinal).is_ok())
        .map(|diagram| {
            let image =
                document
                    .as_ref()
                    .and_then(|document| match document.cached_png(&diagram.source) {
                        Ok(Some(png)) => {
                            if let Ok(image) = budget.cached_png(&png) {
                                image
                            } else {
                                cache_event(Some(CacheIssue::InvalidEntry), "lookup", "show_code");
                                None
                            }
                        }
                        Ok(None) => None,
                        Err(issue) => {
                            cache_event(Some(issue), "lookup", "show_code");
                            None
                        }
                    });
            let slot = image.map(|image| {
                budget.charge(&image);
                let slot = images.len();
                images.push(Some(image));
                slot
            });
            (diagram.ordinal, slot)
        })
        .collect()
}

pub(super) fn show_notice(app: &mut App, display: &PreparedDisplay) {
    let count = display.mermaid_failures.replace(0);
    if count != 0 {
        app.set_notice(format!("Mermaidの図 {count} 件をコードで表示します"));
    }
}

fn convert(
    path: &Path,
    markdown: &str,
    renderer: &Mermaid,
    budget: &mut Budget,
    images: &mut Vec<Option<DynamicImage>>,
) -> (Vec<Placement>, usize) {
    let diagrams = extract(markdown);
    if diagrams.is_empty() {
        return (Vec::new(), 0);
    }
    let started = Instant::now();
    let name = logging::basename(path);
    let mut document = renderer.start_document();
    let available = document.available();
    logging::emit(
        Level::Debug,
        "mermaid.detect",
        json!({
            "available": available, "action": if available { "convert" } else { "show_code" },
        }),
    );
    let mut placements = Vec::new();
    let mut cache_hits = 0;
    let mut converted = 0;
    if available {
        for (index, diagram) in diagrams.iter().enumerate() {
            let result = document.convert(&diagram.source, |bytes| budget.png(bytes));
            cache_event(result.cache.lookup_issue, "lookup", "regenerate");
            cache_event(result.cache.save_issue, "save", "display_without_cache");
            match result.result {
                Ok(image) => {
                    cache_hits += usize::from(result.cache.hit);
                    converted += usize::from(!result.cache.hit);
                    budget.charge(&image);
                    placements.push((diagram.ordinal, Some(images.len())));
                    images.push(Some(image));
                }
                Err(Failure::Launch { kind, os_code }) => {
                    let mut fields = json!({
                        "document": name, "reason": io_reason(kind), "fallback": "code",
                    });
                    if let Some(code) = os_code {
                        fields["os_code"] = code.into();
                    }
                    logging::emit(Level::Warn, "mermaid.launch_failed", fields);
                    break;
                }
                Err(Failure::Timeout(scope)) => {
                    let mut fields = json!({
                        "document": name,
                        "scope": match scope { TimeoutScope::Diagram => "diagram", TimeoutScope::Document => "document" },
                        "limit_ms": document.timeout_limit(scope).as_millis(),
                    });
                    if scope == TimeoutScope::Document {
                        fields["remaining"] = (diagrams.len() - index).into();
                    } else {
                        fields["line"] = diagram.line.into();
                    }
                    logging::emit(Level::Warn, "mermaid.timeout", fields);
                    if scope == TimeoutScope::Document {
                        break;
                    }
                }
                Err(failure) => {
                    conversion_failed(&name, diagram.line, failure, result.elapsed.as_millis());
                }
            }
        }
    }
    let fallbacks = diagrams.len() - placements.len();
    logging::emit(
        Level::Debug,
        "mermaid.summary",
        json!({
            "document": name, "cache_hits": cache_hits, "converted": converted,
            "fallbacks": fallbacks, "elapsed_ms": started.elapsed().as_millis(),
        }),
    );
    (placements, if available { fallbacks } else { 0 })
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
        app::{Action, Direction, ViewAnchor, ViewRows},
        lua::Host,
        session::{BODY_VIEW, Session, read_document},
        ui::layout::Layout,
    };
    use std::{cell::RefCell, env, fs, process::Command, thread, time::Duration};

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
        let initial = logging::capture_for_test(&logs, || session.borrow_mut().display());
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
        let linked = session.borrow_mut().display();
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
        let prepared = session.borrow_mut().display();
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
            Session::with_images(
                read_document(&file).unwrap(),
                ImageMode::Kitty {
                    cell_width: 10,
                    cell_height: 20,
                },
                Some(renderer),
            )
            .display()
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
