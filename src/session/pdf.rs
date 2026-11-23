use std::{
    cell::RefCell,
    io,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc::{Receiver, TryRecvError},
};

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Rect, Size};

use crate::{
    app::{App, Direction, Document, Mode, Motion, PdfDocument, PdfSearch, ScrollEdge, ViewRows},
    logging::{self, Level},
    pdf::{Failure, Page, Renderer, Source, Worker},
    terminal::{self, CellSizeStatus, ImageMode, Screen},
    ui::{
        body_area,
        float::Floats,
        layout::{Layout, PdfGeometry as Geometry},
    },
};

use super::log_document_failure;

mod marks;
mod search;
mod zoom;
use marks::Marks;
use search::{Origin, SEARCHING, SearchJobs};
use zoom::{ZOOM_NOTICE, ZoomRequest, reveal_hit};

pub(super) type PlacedPage = Rc<Layout<'static>>;

pub(super) fn geometry(columns: u16, mode: ImageMode) -> Result<Geometry, Failure> {
    if !cfg!(windows) {
        return Err(Failure::platform());
    }
    match mode {
        ImageMode::Kitty {
            cell_width,
            cell_height,
        } if cell_width > 0 && cell_height > 0 => Ok(Geometry {
            columns,
            cell_width,
            cell_height,
        }),
        _ => Err(Failure::terminal()),
    }
}

pub(super) fn normal_body_height(size: Size) -> u16 {
    body_area(Rect::new(0, 0, size.width, size.height), Mode::Normal).height
}

#[derive(Clone, Copy)]
enum Scale {
    FitPage { body_rows: u16 },
    Fixed(f64),
}

#[derive(Clone, Copy)]
struct Target {
    geometry: Geometry,
    scale: Scale,
}

impl Target {
    fn needs_refit(self, geometry: Geometry, body_height: u16) -> bool {
        self.geometry != geometry
            || matches!(self.scale, Scale::FitPage { body_rows } if body_rows != body_height)
    }
}

struct Rendered {
    layout: Layout<'static>,
    scale: f64,
}

fn fit(page: &Page, target: Target, marks: Option<&Marks>) -> Result<Rendered, Failure> {
    const DISPLAY_SIZE: Failure = Failure::new(
        "pdf_layout",
        "display_size",
        "PDF page exceeds the display limits",
    );
    let image = page.decode()?;
    let aspect = page.aspect_height();
    let geometry = target.geometry;
    let scale = match target.scale {
        Scale::FitPage { body_rows } => geometry
            .initial_scale(aspect, body_rows)
            .ok_or(DISPLAY_SIZE)?,
        Scale::Fixed(scale) => scale,
    };
    let layout = Layout::pdf_page(&image, aspect, geometry, scale, |pixels| {
        if let Some(marks) = marks {
            marks.paint(page, pixels);
        }
    })
    .ok_or(DISPLAY_SIZE)?;
    Ok(Rendered { layout, scale })
}

type ResultPage = Result<(Page, Result<Rendered, Failure>), Failure>;

fn try_result(receiver: &Receiver<ResultPage>) -> Option<ResultPage> {
    match receiver.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Disconnected) => Some(Err(Failure::worker())),
        Err(TryRecvError::Empty) => None,
    }
}

enum Work {
    Open(PathBuf),
    Turn(Source, u32),
    Fit(Page),
}

impl Work {
    fn marks(&self, search: Option<&PdfSearch>) -> Option<Marks> {
        let index = match self {
            Self::Open(_) => return None,
            Self::Turn(_, index) => *index,
            Self::Fit(page) => page.index,
        };
        Marks::for_page(search, index)
    }

    fn place(self, renderer: &mut Renderer, target: Target, marks: Option<&Marks>) -> ResultPage {
        let page = match self {
            Self::Open(path) => renderer.open(&path)?,
            Self::Turn(source, index) => renderer.render(source, index)?,
            Self::Fit(page) => page,
        };
        let rendered = fit(&page, target, marks);
        Ok((page, rendered))
    }
}

#[derive(PartialEq)]
enum Intent {
    Open {
        from: u64,
        mode: Mode,
        path: PathBuf,
        float: Option<(u64, u64)>,
    },
    Restore {
        from: u64,
        epoch: u64,
        scale: f64,
    },
    Turn {
        from: (u64, u32),
        target: u32,
        previous: bool,
        motion: Motion,
    },
    Fit {
        from: (u64, u32),
    },
    Zoom {
        from: (u64, u32),
        epoch: u64,
        scale: f64,
    },
    Search {
        from: (u64, u32),
        result: PdfSearch,
    },
}

impl Intent {
    fn document_id(&self) -> u64 {
        match self {
            Self::Open { from, .. } | Self::Restore { from, .. } => *from,
            Self::Turn { from, .. }
            | Self::Fit { from }
            | Self::Zoom { from, .. }
            | Self::Search { from, .. } => from.0,
        }
    }

    fn still_current(&self, app: &App, floats: &Floats) -> bool {
        match self {
            Self::Open {
                from, mode, float, ..
            } => {
                *from == app.document_id()
                    && *float == floats.active()
                    && app.mode() == *mode
                    && !app.help_open()
            }
            Self::Restore { from, epoch, .. } => {
                *from == app.document_id()
                    && *epoch == app.display_epoch()
                    && app.history_pdf().is_some()
                    && floats.active().is_none()
                    && app.mode() == Mode::Normal
                    && !app.help_open()
            }
            Self::Turn { from, .. } | Self::Search { from, .. } => can_navigate(*from, app, floats),
            Self::Fit { from } => Some(*from) == page_key(app),
            Self::Zoom { from, epoch, .. } => {
                Some(*from) == page_key(app) && *epoch == app.display_epoch()
            }
        }
    }

    fn scale(&self, app: &App, body_height: u16) -> Scale {
        match self {
            Self::Open { .. } => Scale::FitPage {
                body_rows: body_height,
            },
            Self::Restore { scale, .. } | Self::Zoom { scale, .. } => Scale::Fixed(*scale),
            _ => Scale::Fixed(app.document().as_pdf().map_or(1.0, PdfDocument::scale)),
        }
    }
}

struct Request {
    intent: Intent,
    work: Work,
}

struct Running {
    // Dropping the intent cancels publication, but the native call may still be completing.
    intent: Option<Intent>,
    target: Target,
    marks: Option<Marks>,
    result: Receiver<ResultPage>,
}

#[derive(Default)]
pub(super) struct Jobs {
    queued: Option<Request>,
    running: Option<Running>,
    placed: Option<Placement>,
    failed_fit: Option<((u64, u32), Geometry, Option<Marks>)>,
    deferred_motion: Option<((u64, u32), Motion)>,
    requested_zoom: Option<ZoomRequest>,
    search: SearchJobs,
    worker: Rc<RefCell<Worker>>,
}

struct Placement {
    key: (u64, u32),
    geometry: Geometry,
    marks: Option<Marks>,
    layout: PlacedPage,
}

fn active_page(app: &App) -> Option<&Page> {
    app.document().as_pdf().map(PdfDocument::page)
}

fn pdf_search(app: &App) -> Option<&PdfSearch> {
    app.document().as_pdf().and_then(PdfDocument::search)
}

fn publish_search(app: &mut App, result: PdfSearch) {
    let no_matches = result.current().is_none();
    if Document::publish_pdf_search(app, result) {
        if no_matches {
            app.set_notice("No matches".into());
        } else {
            app.clear_notice();
        }
    }
}

fn turn_target(app: &App, motion: Motion, rows: &impl ViewRows) -> Option<(u32, bool)> {
    let edge = app.scroll_edge(motion, rows)?;
    let target = app.document().as_pdf()?.turn_target(edge)?;
    Some((target, matches!(edge, ScrollEdge::Start)))
}

fn page_key(app: &App) -> Option<(u64, u32)> {
    active_page(app).map(|page| (app.document_id(), page.index))
}

fn can_navigate(from: (u64, u32), app: &App, floats: &Floats) -> bool {
    Some(from) == page_key(app)
        && floats.active().is_none()
        && app.mode() == Mode::Normal
        && !app.help_open()
}

impl Jobs {
    pub(super) fn with_screen(
        run: impl FnOnce(&mut Screen, &mut terminal::Input, Self) -> io::Result<()>,
    ) -> io::Result<()> {
        let jobs = Self::default();
        // Restore the terminal before waiting for any in-flight native operation at exit.
        let _worker = Rc::clone(&jobs.worker);
        terminal::with_screen(|screen, input| run(screen, input, jobs))
    }

    fn submit(
        &self,
        work: Work,
        target: Target,
        marks: Option<Marks>,
    ) -> Result<Receiver<ResultPage>, Failure> {
        self.worker
            .borrow_mut()
            .submit(move |renderer| work.place(renderer, target, marks.as_ref()))
    }

    pub(super) fn reset(&mut self) {
        self.search.cancel();
        self.cancel_if(|_| true);
        self.requested_zoom = None;
        self.placed = None;
        self.failed_fit = None;
        self.deferred_motion = None;
    }

    pub(super) fn placed(&self, app: &App) -> Option<PlacedPage> {
        self.placed
            .as_ref()
            .filter(|placed| Some(placed.key) == page_key(app))
            .map(|placed| Rc::clone(&placed.layout))
    }

    pub(super) fn depart(&mut self, document: u64) {
        if self.search.from().is_some_and(|(id, _)| id == document) {
            self.search.cancel();
        }
        self.cancel_if(|intent| intent.document_id() == document);
        self.requested_zoom = self.requested_zoom.filter(|zoom| zoom.document != document);
        if self
            .placed
            .as_ref()
            .is_some_and(|placed| placed.key.0 == document)
        {
            self.placed = None;
        }
        self.failed_fit = None;
        self.deferred_motion = None;
    }

    pub(super) fn open(&mut self, app: &App, path: &Path, float: Option<(u64, u64)>) {
        let intent = Intent::Open {
            from: app.document_id(),
            mode: app.mode(),
            path: path.to_owned(),
            float,
        };
        self.deferred_motion = None;
        if self.pending() != Some(&intent) {
            self.replace(Request {
                intent,
                work: Work::Open(path.to_owned()),
            });
        }
    }

    /// Keep the history entry and current display intact until the saved PDF is ready.
    pub(super) fn restore(&mut self, app: &App) -> bool {
        let Some((_, pdf)) = app.history_pdf() else {
            return false;
        };
        let intent = Intent::Restore {
            from: app.document_id(),
            epoch: app.display_epoch(),
            scale: pdf.scale(),
        };
        self.deferred_motion = None;
        if self.pending() != Some(&intent) {
            self.replace(Request {
                intent,
                work: Work::Fit(pdf.page().clone()),
            });
        }
        true
    }

    pub(super) fn needs_cell_size_check(&self) -> bool {
        matches!(
            self.pending(),
            Some(Intent::Open { .. } | Intent::Restore { .. })
        )
    }

    pub(super) fn search_pending(&self) -> bool {
        self.search_from().is_some()
    }

    fn search_from(&self) -> Option<(u64, u32)> {
        self.search.from().or_else(|| match self.pending() {
            Some(Intent::Search { from, .. }) => Some(*from),
            _ => None,
        })
    }

    pub(super) fn cancel_search(&mut self, app: &mut App) {
        if self.search_pending() {
            self.search.cancel();
            self.cancel_if(|intent| matches!(intent, Intent::Search { .. }));
            app.clear_notice_if(SEARCHING);
        }
    }

    pub(super) fn start_search(&mut self, app: &mut App, input: String, direction: Direction) {
        if let Some(from) = page_key(app) {
            self.prioritize_search();
            let origin = self
                .placed(app)
                .and_then(|layout| Origin::new(from.1, app.top_row(), &layout));
            self.search.start(app, from, input, direction, origin);
        }
    }

    pub(super) fn repeat_search(
        &mut self,
        app: &mut App,
        geometry: Result<Geometry, Failure>,
        reverse: bool,
    ) {
        if self.search.from().is_some() {
            return; // Cannot step until the new query has a result.
        }
        let result = match self.pending() {
            Some(Intent::Search { result, .. }) => Some(result),
            _ => pdf_search(app),
        };
        let Some(mut result) = result.cloned() else {
            app.set_notice("No previous search".into());
            return;
        };
        self.prioritize_search();
        result.step(reverse);
        self.reveal_search(app, geometry, result);
    }

    fn prioritize_search(&mut self) {
        self.deferred_motion = None;
        self.cancel_if(|intent| !matches!(intent, Intent::Fit { .. }));
    }

    fn reveal_search(
        &mut self,
        app: &mut App,
        geometry: Result<Geometry, Failure>,
        result: PdfSearch,
    ) {
        let Some((from, page)) = page_key(app).zip(active_page(app)) else {
            return;
        };
        let Some(hit) = result.current() else {
            publish_search(app, result);
            return;
        };
        let work = if hit.page == page.index {
            if !page.supports_hit(hit) {
                search::fail(app, search::GEOMETRY);
                return;
            }
            if let Some(layout) = self.matching_layout(app, geometry)
                && let Ok(geometry) = geometry
            {
                reveal_hit(app, layout.as_ref(), geometry.columns, hit);
                publish_search(app, result);
                return;
            }
            Work::Fit(page.clone())
        } else {
            Work::Turn(page.source.clone(), hit.page)
        };
        if let Err(failure) = geometry {
            search::fail(app, failure);
            return;
        }
        self.replace(Request {
            intent: Intent::Search { from, result },
            work,
        });
        app.set_notice(SEARCHING.into());
    }

    fn revalidate_search(
        &mut self,
        app: &mut App,
        floats: &Floats,
        geometry: Result<Geometry, Failure>,
    ) -> bool {
        let Some(from) = self.search_from() else {
            return false;
        };
        if !can_navigate(from, app, floats) {
            self.cancel_search(app);
        } else if let Err(failure) = geometry {
            self.cancel_search(app);
            search::fail(app, failure);
        } else {
            return false;
        }
        true
    }

    fn advance_search(&mut self, app: &mut App, geometry: Result<Geometry, Failure>) -> bool {
        if let Some(from) = self.search.needs_origin()
            && self.placed(app).is_none()
            && geometry.is_ok_and(|geometry| self.fit_failed_at(from, geometry))
        {
            self.cancel_search(app);
            search::fail(app, search::GEOMETRY);
            return true;
        }
        let origin = self.search.needs_origin().and_then(|from| {
            let layout = self.placed(app)?;
            // A restored page may have just received its first layout since coming
            // back from history. Compute the origin from its restored anchor.
            app.reflow_current_height(layout.as_ref());
            Origin::new(from.1, app.top_row(), &layout)
        });
        match self
            .search
            .poll(active_page(app).map(|page| &page.source), origin)
        {
            Ok(Some(result)) => self.reveal_search(app, geometry, result),
            Err(failure) => search::fail(app, failure),
            Ok(None) => return false,
        }
        true
    }

    fn matching_layout(
        &self,
        app: &App,
        geometry: Result<Geometry, Failure>,
    ) -> Option<PlacedPage> {
        let geometry = geometry.ok()?;
        let placed = self.placed.as_ref()?;
        (Some(placed.key) == page_key(app) && placed.geometry == geometry)
            .then(|| Rc::clone(&placed.layout))
    }

    pub(super) fn move_pdf(
        &mut self,
        app: &mut App,
        motion: Motion,
        geometry: Result<Geometry, Failure>,
    ) -> bool {
        let Some(from) = page_key(app) else {
            return false;
        };
        if app.mode() != Mode::Normal || app.help_open() {
            return false;
        }
        if let Some(layout) = self.matching_layout(app, geometry) {
            self.deferred_motion = None;
            self.apply_motion(app, motion, layout.as_ref());
        } else {
            self.cancel_if(|intent| matches!(intent, Intent::Turn { .. }));
            if geometry.is_ok_and(|geometry| !self.fit_failed_at(from, geometry)) {
                // Keep the latest motion across a refit, not an edge decision from the old view.
                self.deferred_motion = Some((from, motion));
            }
        }
        true
    }

    fn apply_motion(&mut self, app: &mut App, motion: Motion, rows: &impl ViewRows) {
        let Some(page) = active_page(app) else { return };
        let Some((target, previous)) = turn_target(app, motion, rows) else {
            self.cancel_if(|intent| matches!(intent, Intent::Turn { .. }));
            app.apply(crate::app::Action::Move(motion), rows, "");
            return;
        };
        let intent = Intent::Turn {
            from: (app.document_id(), page.index),
            target,
            previous,
            motion,
        };
        if self.pending() != Some(&intent) {
            self.replace(Request {
                intent,
                work: Work::Turn(page.source.clone(), target),
            });
        }
    }

    fn revalidate_turn(&mut self, app: &App, floats: &Floats, geometry: Result<Geometry, Failure>) {
        let can_move = |key| can_navigate(key, app, floats);
        self.deferred_motion = self.deferred_motion.filter(|&(key, _)| can_move(key));
        let layout = self.matching_layout(app, geometry);
        if let Some(&Intent::Turn {
            from,
            motion,
            target,
            previous,
        }) = self.pending()
            && layout.as_ref().is_none_or(|layout| {
                turn_target(app, motion, layout.as_ref()) != Some((target, previous))
            })
        {
            self.cancel_if(|intent| matches!(intent, Intent::Turn { .. }));
            self.deferred_motion = Some((from, motion));
        }
    }

    fn replay_resized_motion(
        &mut self,
        app: &mut App,
        geometry: Result<Geometry, Failure>,
    ) -> bool {
        if let Some(layout) = self.matching_layout(app, geometry)
            && let Some((_, motion)) = self.deferred_motion.take()
        {
            app.reflow_current_height(layout.as_ref());
            self.apply_motion(app, motion, layout.as_ref());
            return true;
        }
        false
    }

    fn pending(&self) -> Option<&Intent> {
        self.queued
            .as_ref()
            .map(|r| &r.intent)
            .or_else(|| self.running.as_ref()?.intent.as_ref())
    }

    fn cancel_if(&mut self, should_cancel: impl Fn(&Intent) -> bool) {
        if self
            .queued
            .as_ref()
            .is_some_and(|r| should_cancel(&r.intent))
        {
            self.queued = None;
        }
        if let Some(running) = &mut self.running
            && running.intent.as_ref().is_some_and(should_cancel)
        {
            running.intent = None;
        }
    }

    fn replace(&mut self, request: Request) {
        self.cancel_if(|_| true);
        self.queued = Some(request);
    }

    /// Poll and publish on the UI thread. At most one native/encoding job runs at a time;
    /// waiting UI requests coalesce to the latest intent.
    pub(super) fn poll(
        &mut self,
        app: &mut App,
        floats: &Floats,
        geometry: Result<Geometry, Failure>,
        body_height: u16,
        cell_size: CellSizeStatus,
    ) -> Update {
        let key = page_key(app);
        if self
            .placed
            .as_ref()
            .is_some_and(|placed| Some(placed.key) != key)
        {
            self.placed = None;
        }
        if self
            .failed_fit
            .as_ref()
            .is_some_and(|(failed, previous, _)| {
                Some(*failed) != key || geometry.is_ok_and(|geometry| geometry != *previous)
            })
        {
            self.failed_fit = None;
        }
        let search_cancelled = self.revalidate_search(app, floats, geometry);
        self.cancel_if(|intent| !intent.still_current(app, floats));
        // Wait for a pending resize probe before publishing the first layout.
        // Open can still use cached cells on timeout, as before; restore cannot.
        if cell_size == CellSizeStatus::Pending && self.needs_cell_size_check() {
            return Update {
                redraw: search_cancelled,
                close_float: None,
            };
        }
        if cell_size == CellSizeStatus::Unconfirmed
            && matches!(self.pending(), Some(Intent::Restore { .. }))
            && let Some(intent) = self.take_restore()
        {
            return self.fail(app, intent, CELL_SIZE_UNCONFIRMED, None, None);
        }
        self.revalidate_turn(app, floats, geometry);
        let mut update = self.poll_rendered(app, geometry, body_height);
        update.redraw |= search_cancelled;
        update.redraw |= self.advance_search(app, geometry);
        update.redraw |= self.replay_resized_motion(app, geometry);
        self.queue_zoom(app);
        self.queue_fit(app, geometry);
        if let Some(failed) = self.submit_queued(app, geometry, body_height) {
            update.redraw |= failed.redraw;
        }
        update
    }

    fn take_restore(&mut self) -> Option<Intent> {
        if self
            .queued
            .as_ref()
            .is_some_and(|request| matches!(request.intent, Intent::Restore { .. }))
        {
            self.queued.take().map(|request| request.intent)
        } else {
            self.running.as_mut()?.intent.take()
        }
    }

    fn poll_rendered(
        &mut self,
        app: &mut App,
        geometry: Result<Geometry, Failure>,
        body_height: u16,
    ) -> Update {
        if let Some(running) = &mut self.running
            && let Some(result) = try_result(&running.result)
        {
            let intent = running.intent.take();
            let target = running.target;
            let marks = running.marks.take();
            self.running = None;
            if let Some(intent) = intent {
                // A same-page search or :noh can change the desired tiles while
                // a refit is in progress. Do not publish the old marks.
                if let Intent::Fit { from } | Intent::Zoom { from, .. } = &intent
                    && marks != Marks::for_page(pdf_search(app), from.1)
                {
                    return Update::default();
                }
                return match (geometry, result) {
                    (Ok(latest), Ok((page, _))) if target.needs_refit(latest, body_height) => {
                        self.queued = Some(Request {
                            intent,
                            work: Work::Fit(page),
                        });
                        Update::default()
                    }
                    (Ok(_), Ok((page, Ok(rendered)))) => {
                        self.commit(app, intent, page, target.geometry, marks, rendered)
                    }
                    (_, Err(error) | Ok((_, Err(error)))) => {
                        self.fail(app, intent, error, Some(target.geometry), marks)
                    }
                    (Err(error), Ok(_)) => self.fail(app, intent, error, None, marks),
                };
            }
        }
        Update::default()
    }

    fn fit_failed_at(&self, key: (u64, u32), geometry: Geometry) -> bool {
        self.failed_fit
            .as_ref()
            .is_some_and(|failed| (failed.0, failed.1) == (key, geometry))
    }

    fn queue_fit(&mut self, app: &App, geometry: Result<Geometry, Failure>) {
        if self.pending().is_none()
            && let (Some(key), Some(page), Ok(geometry)) =
                (page_key(app), active_page(app), geometry)
            && let marks = Marks::for_page(pdf_search(app), key.1)
            && !self.placed.as_ref().is_some_and(|placed| {
                placed.key == key && placed.geometry == geometry && placed.marks == marks
            })
            && !self
                .failed_fit
                .as_ref()
                .is_some_and(|failed| (failed.0, failed.1) == (key, geometry) && failed.2 == marks)
        {
            self.queued = Some(Request {
                intent: Intent::Fit { from: key },
                work: Work::Fit(page.clone()),
            });
        }
    }

    fn submit_queued(
        &mut self,
        app: &mut App,
        geometry: Result<Geometry, Failure>,
        body_height: u16,
    ) -> Option<Update> {
        if self.running.is_some() {
            return None;
        }
        let Request { intent, work } = self.queued.take()?;
        let search = match &intent {
            Intent::Open { .. } => None,
            Intent::Search { result, .. } => Some(result),
            Intent::Restore { .. } => app.history_pdf().and_then(|(_, pdf)| pdf.search()),
            _ => pdf_search(app),
        };
        let marks = work.marks(search);
        let scale = intent.scale(app, body_height);
        match geometry.and_then(|geometry| {
            let target = Target { geometry, scale };
            self.submit(work, target, marks.clone())
                .map(|result| (target, result))
        }) {
            Ok((target, result)) => {
                self.running = Some(Running {
                    intent: Some(intent),
                    target,
                    marks,
                    result,
                });
                None
            }
            Err(error) => Some(self.fail(app, intent, error, geometry.ok(), marks)),
        }
    }

    fn commit(
        &mut self,
        app: &mut App,
        intent: Intent,
        page: Page,
        geometry: Geometry,
        marks: Option<Marks>,
        Rendered { layout, scale }: Rendered,
    ) -> Update {
        let close_float = match intent {
            Intent::Open { path, float, .. } => {
                self.requested_zoom = None;
                app.open_document(Document::pdf(path, page, scale));
                float
            }
            Intent::Restore { .. } => {
                self.requested_zoom = None;
                app.jump_back();
                None
            }
            Intent::Turn { previous, .. } => {
                Document::replace_pdf_page(app, page);
                app.reset_viewport(PdfDocument::arrival_anchor(previous));
                app.clear_notice_if(TURN_NOTICE);
                None
            }
            Intent::Fit { .. } => {
                if app
                    .document()
                    .as_pdf()
                    .is_some_and(|pdf| pdf.scale().to_bits() != 1.0_f64.to_bits())
                    && self
                        .placed
                        .as_ref()
                        .is_some_and(|old| old.geometry != geometry)
                {
                    self.preserve_zoom_y(app, &layout);
                }
                app.clear_notice_if(FIT_NOTICE);
                None
            }
            Intent::Zoom { .. } => {
                self.commit_zoom(app, geometry.columns, scale, &layout);
                app.clear_notice_if(FIT_NOTICE);
                None
            }
            Intent::Search { result, .. } => {
                let Some(hit) = result.current().filter(|hit| page.supports_hit(*hit)) else {
                    search::fail(app, search::GEOMETRY);
                    return Update {
                        redraw: true,
                        close_float: None,
                    };
                };
                Document::replace_pdf_page(app, page);
                reveal_hit(app, &layout, geometry.columns, hit);
                publish_search(app, result);
                None
            }
        };
        if let Some(key) = page_key(app) {
            self.placed = Some(Placement {
                key,
                geometry,
                marks,
                layout: Rc::new(layout),
            });
        }
        self.failed_fit = None;
        Update {
            redraw: true,
            close_float,
        }
    }

    fn fail(
        &mut self,
        app: &mut App,
        intent: Intent,
        failure: Failure,
        geometry: Option<Geometry>,
        marks: Option<Marks>,
    ) -> Update {
        match intent {
            Intent::Open { path, .. } => {
                log_open_failure(&path, "open_file", failure);
                app.set_notice(failure.message.into());
            }
            Intent::Restore { .. } => {
                if let Some((path, pdf)) = app.history_pdf() {
                    log_page_failure(path, pdf.page().index, "history", failure);
                }
                app.set_notice(format!("Could not return to PDF: {}", failure.message));
            }
            Intent::Turn { target, .. } => {
                log_page_failure(app.document_path(), target, "turn", failure);
                app.set_notice(TURN_NOTICE.into());
            }
            Intent::Fit { from } => {
                self.failed_fit = geometry.map(|geometry| (from, geometry, marks));
                self.deferred_motion = None;
                log_page_failure(app.document_path(), from.1, "resize", failure);
                app.set_notice(FIT_NOTICE.into());
            }
            Intent::Zoom { from, .. } => {
                self.requested_zoom = None;
                log_page_failure(app.document_path(), from.1, "zoom", failure);
                app.set_notice(ZOOM_NOTICE.into());
            }
            Intent::Search { .. } => search::fail(app, failure),
        }
        Update {
            redraw: true,
            close_float: None,
        }
    }
}

const CELL_SIZE_UNCONFIRMED: Failure = Failure::new(
    "pdf_layout",
    "unconfirmed_cell_size",
    "Could not confirm the terminal cell size after resizing",
);

const TURN_NOTICE: &str = "Could not display the requested PDF page";
const FIT_NOTICE: &str = "Could not fit the PDF page to this window";

#[derive(Default)]
pub(super) struct Update {
    pub redraw: bool,
    pub close_float: Option<(u64, u64)>,
}

pub(super) fn log_open_failure(path: &Path, trigger: &'static str, failure: Failure) {
    log_document_failure(path, trigger, failure.stage, failure.reason, failure.code);
}

fn log_page_failure(path: &Path, index: u32, trigger: &'static str, failure: Failure) {
    let mut fields = serde_json::json!({
        "document":logging::basename(path), "page":index + 1, "trigger":trigger,
        "stage":failure.stage, "reason":failure.reason,
    });
    if let Some(code) = failure.code {
        fields["os_code"] = code.into();
    }
    logging::emit(Level::Warn, "pdf.page_failed", fields);
}

// At startup there is no previous document or Lua view to publish into yet.
// Use the same open-and-place worker while allowing the user to cancel loading.
pub(super) fn startup(
    screen: &mut Screen,
    input: &mut terminal::Input,
    path: PathBuf,
    mut jobs: Jobs,
) -> io::Result<Option<(App, Jobs)>> {
    let mode = input.image_mode();
    let mut size = terminal::check(screen.size(), "size", Some(mode), "exit")?;
    let report = |failure: &Failure| log_open_failure(&path, "startup", *failure);
    let first_page_target = |geometry, body_rows| Target {
        geometry,
        scale: Scale::FitPage { body_rows },
    };
    let mut target = first_page_target(
        geometry(size.width, mode).inspect_err(report)?,
        normal_body_height(size),
    );
    let mut receiver = jobs
        .submit(Work::Open(path.clone()), target, None)
        .inspect_err(report)?;

    loop {
        let mode = input.image_mode();
        let latest_size = terminal::check(screen.size(), "size", Some(mode), "exit")?;
        if latest_size != size {
            size = latest_size;
            input.refresh_cell_size();
        }
        if input.pdf_restore_cell_size() != CellSizeStatus::Pending
            && let Some(result) = try_result(&receiver)
        {
            let (page, rendered) = result.inspect_err(report)?;
            let latest = geometry(size.width, mode).inspect_err(report)?;
            let body_rows = normal_body_height(size);
            if !target.needs_refit(latest, body_rows) {
                let Rendered { layout, scale } = rendered.inspect_err(report)?;
                let app = App::from_document(Document::pdf(path, page, scale));
                jobs.placed = page_key(&app).map(|key| Placement {
                    key,
                    geometry: target.geometry,
                    marks: None,
                    layout: Rc::new(layout),
                });
                return Ok(Some((app, jobs)));
            }
            // Keep the native raster; only the placement needs to match the new body size.
            target = first_page_target(latest, body_rows);
            receiver = jobs
                .submit(Work::Fit(page), target, None)
                .inspect_err(report)?;
        }
        terminal::check(
            screen.draw(|frame| {
                frame.render_widget(
                    ratatui::widgets::Paragraph::new("Opening PDF… (Esc to cancel)"),
                    frame.area(),
                );
            }),
            "draw",
            Some(mode),
            "exit",
        )?;
        if input.poll_key()?.is_some_and(|key| {
            key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        }) {
            return Ok(None);
        }
    }
}

#[cfg(all(test, windows))]
#[path = "../../tests/support/pdf_text_coordinates.rs"]
mod pdf_text_coordinates;

#[cfg(all(test, windows))]
mod tests {
    use std::{
        fs,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use crate::{
        app::Action,
        lua::{DocumentKind, FileOpen, Host, Runtime},
        session::{BODY_VIEW, InputQueue, Session, poll_pdf},
        ui::float::FloatOptions,
    };
    use crossterm::event::KeyEvent;

    use super::*;

    const KITTY: ImageMode = ImageMode::Kitty {
        cell_width: 2,
        cell_height: 4,
    };
    const FIRST: Geometry = Geometry {
        columns: 80,
        cell_width: 2,
        cell_height: 4,
    };

    fn test_session() -> Rc<RefCell<Session>> {
        Rc::new(RefCell::new(Session::with_images(
            Document::new(
                "original.md".into(),
                "Original".into(),
                "original.md".into(),
                vec![],
            ),
            KITTY,
            None,
        )))
    }

    pub(super) fn wait(session: &RefCell<Session>, runtime: &Runtime) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            poll_pdf(session, runtime);
            let jobs = &session.borrow().pdf;
            if jobs.running.is_none() && jobs.queued.is_none() && !jobs.search.busy() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "PDF worker did not finish",
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn place(session: &mut Session, height: usize) -> io::Result<PlacedPage> {
        let placed = session
            .pdf
            .placed(&session.app)
            .ok_or_else(|| io::Error::other("PDF was not placed"))?;
        session.app.reflow(placed.as_ref(), height);
        Ok(placed)
    }

    struct Demo {
        session: Rc<RefCell<Session>>,
        runtime: Runtime,
        original: u64,
        directory: tempfile::TempDir,
    }

    impl Demo {
        fn open() -> Result<Self, Box<dyn std::error::Error>> {
            Self::open_at(FIRST)
        }

        fn open_at(geometry: Geometry) -> Result<Self, Box<dyn std::error::Error>> {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("manual.pdf");
            fs::write(&path, crate::test_pdf::two_pages())?;
            let session = test_session();
            session.borrow_mut().pdf_geometry = Ok(geometry);
            let original = session.borrow().app.document_id();
            drop(session.borrow_mut().display()); // same Markdown cache as in a live session
            let (runtime, error) = Runtime::load_from(session.clone(), None);
            assert!(error.is_none());
            assert!(matches!(
                session.open_file(BODY_VIEW, &path)?,
                FileOpen::Pending
            ));
            assert_eq!(session.borrow().app.document_id(), original);
            wait(&session, &runtime)?;
            assert!(
                session.borrow().prepared.is_none(),
                "history must not reuse the old Markdown display"
            );
            // These navigation tests exercise pages that overflow the viewport.
            session.set_pdf_zoom(BODY_VIEW, 1.0)?;
            wait(&session, &runtime)?;
            Ok(Self {
                session,
                runtime,
                original,
                directory,
            })
        }

        fn page(&self) -> io::Result<PlacedPage> {
            wait(&self.session, &self.runtime)?;
            place(&mut self.session.borrow_mut(), 24)
        }

        fn move_by(&self, motion: Motion, page: &Layout<'static>) {
            self.session
                .borrow_mut()
                .apply(Action::Move(motion), page, "");
        }
    }

    #[test]
    fn opening_pdf_refits_to_latest_body_height_before_publishing()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("manual.pdf");
        fs::write(&path, crate::test_pdf::two_pages())?;
        let session = test_session();
        let original = session.borrow().app.document_id();
        let (runtime, error) = Runtime::load_from(session.clone(), None);
        assert!(error.is_none());
        let sync_size = |state: &mut Session, width, height| {
            state.sync_pdf_layout(Size::new(width, height), KITTY, &mut None, &mut Vec::new());
        };
        sync_size(&mut session.borrow_mut(), 80, 49);
        assert!(matches!(
            session.open_file(BODY_VIEW, &path)?,
            FileOpen::Pending
        ));
        poll_pdf(&session, &runtime); // submit using the old height
        {
            let mut state = session.borrow_mut();
            let running = state.pdf.running.as_mut().ok_or("expected open render")?;
            let completed = running.result.recv_timeout(Duration::from_secs(15))?;
            assert!(matches!(&completed, Ok((_, Ok(rendered))) if rendered.layout.len() == 48));
            let (send, result) = mpsc::channel();
            assert!(send.send(completed).is_ok());
            running.result = result;
            sync_size(&mut state, 80, 25);
        }
        poll_pdf(&session, &runtime);
        assert_eq!(session.borrow().app.document_id(), original);
        wait(&session, &runtime)?;
        let first = place(&mut session.borrow_mut(), 24)?;
        assert_eq!(first.len(), 24);
        assert_eq!(page_key(&session.borrow().app).map(|key| key.1), Some(0));
        let init = directory.path().join("zoom.lua");
        fs::write(
            &init,
            "assert(tvi.ui.current_view():document():zoom() == 0.15)",
        )?;
        let (_, error) = Runtime::load_from(session.clone(), Some(init.clone()));
        assert!(error.is_none(), "{error:?}");

        // The fitted page is already at its end. One down scroll turns it.
        let pdf_id = session.borrow().app.document_id();
        let move_down = Action::Move(Motion::LineDown);
        session.borrow_mut().apply(move_down, first.as_ref(), "");
        wait(&session, &runtime)?;
        let second = place(&mut session.borrow_mut(), 24)?;
        assert_eq!(page_key(&session.borrow().app), Some((pdf_id, 1)));
        assert_eq!(second.len(), 12);
        assert_eq!(session.pdf_zoom(BODY_VIEW)?, 0.15);

        sync_size(&mut session.borrow_mut(), 80, 41);
        wait(&session, &runtime)?;
        assert!(Rc::ptr_eq(&second, &place(&mut session.borrow_mut(), 40)?));
        sync_size(&mut session.borrow_mut(), 40, 41);
        wait(&session, &runtime)?;
        assert_eq!(place(&mut session.borrow_mut(), 40)?.len(), 6);
        assert_eq!(session.pdf_zoom(BODY_VIEW)?, 0.15);

        let markdown = directory.path().join("other.md");
        fs::write(&markdown, "Other Markdown")?;
        assert!(matches!(
            session.open_file(BODY_VIEW, &markdown)?,
            FileOpen::Opened
        ));
        session.borrow_mut().apply(Action::JumpBack, &[], "");
        wait(&session, &runtime)?;
        assert_eq!(page_key(&session.borrow().app), Some((pdf_id, 1)));
        assert_eq!(place(&mut session.borrow_mut(), 40)?.len(), 6);
        assert_eq!(session.pdf_zoom(BODY_VIEW)?, 0.15);

        fs::write(
            &init,
            "assert(tvi.ui.current_view():document():set_zoom(1.0))",
        )?;
        let (runtime, error) = Runtime::load_from(session.clone(), Some(init));
        assert!(error.is_none(), "{error:?}");
        wait(&session, &runtime)?;
        assert_eq!(place(&mut session.borrow_mut(), 40)?.len(), 40);
        assert_eq!(session.pdf_zoom(BODY_VIEW)?, 1.0);
        Ok(())
    }

    #[test]
    fn pdf_zoom_from_lua_keeps_the_view_center_and_survives_resize_and_turn()
    -> Result<(), Box<dyn std::error::Error>> {
        let demo = Demo::open()?;
        let first = demo.page()?;
        let handle = demo
            .session
            .document(BODY_VIEW)?
            .ok_or("missing PDF handle")?;
        demo.move_by(Motion::PageDown, &first);
        let init = demo.directory.path().join("init.lua");
        fs::write(
            &init,
            r"
            local doc = tvi.ui.current_view():document()
            assert(doc:type() == 'pdf' and doc:zoom() == 1.0)
            for _, scale in ipairs({1e-30, 1e30, 100}) do
                local accepted, err = doc:set_zoom(scale)
                assert(accepted == false and type(err) == 'string')
            end
            local accepted, err = doc:set_zoom(1.5)
            assert(accepted == true and err == nil and doc:zoom() == 1.0)
        ",
        )?;
        let (_runtime, error) = Runtime::load_from(demo.session.clone(), Some(init));
        assert!(error.is_none(), "{error:?}");
        let enlarged = demo.page()?;
        let position = || {
            (
                demo.session.pdf_zoom(BODY_VIEW),
                demo.session.borrow().app.top_row(),
            )
        };
        assert_eq!(position(), (Ok(1.5), 42));
        assert_eq!(enlarged.len(), 240);

        demo.session.borrow_mut().pdf_geometry = Ok(Geometry {
            columns: 64,
            ..FIRST
        });
        let resized = demo.page()?;
        assert_eq!(position(), (Ok(1.5), 31));
        assert_eq!(resized.len(), 192);
        demo.move_by(Motion::End, &resized);
        demo.move_by(Motion::LineDown, &resized);
        let next = demo.page()?;
        assert_eq!(position(), (Ok(1.5), 0));
        assert_eq!(next.len(), 96);
        assert_eq!(demo.session.document(BODY_VIEW)?, Some(handle));

        demo.session.set_pdf_zoom(BODY_VIEW, 0.5)?;
        assert_eq!(demo.page()?.len(), 32);
        assert_eq!(demo.session.pdf_zoom(BODY_VIEW)?, 0.5);
        demo.session.set_pdf_zoom(BODY_VIEW, 1.0)?;
        assert_eq!(demo.page()?.len(), 64);
        assert_eq!(demo.session.pdf_zoom(BODY_VIEW)?, 1.0);
        Ok(())
    }

    /// Deliver a completed zoom only when the UI next polls, even if it is superseded first.
    fn complete_zoom(
        demo: &Demo,
        failure: Option<Failure>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Session {
            app,
            pdf,
            pdf_geometry,
            pdf_normal_height,
            ..
        } = &mut *demo.session.borrow_mut();
        pdf.queue_zoom(app);
        let Request { intent, work } = pdf.queued.take().ok_or("expected zoom render")?;
        assert!(matches!(intent, Intent::Zoom { .. }));
        let target = Target {
            geometry: pdf_geometry.map_err(io::Error::from)?,
            scale: intent.scale(app, *pdf_normal_height),
        };
        let marks = work.marks(pdf_search(app));
        let completed = if let Some(failure) = failure {
            Err(failure)
        } else {
            pdf.submit(work, target, marks.clone())
                .map_err(io::Error::from)?
                .recv_timeout(Duration::from_secs(15))?
        };
        let (send, result) = mpsc::channel();
        assert!(send.send(completed).is_ok());
        pdf.running = Some(Running {
            intent: Some(intent),
            target,
            marks,
            result,
        });
        Ok(())
    }

    #[test]
    fn only_latest_pdf_zoom_commits_and_failed_zoom_keeps_the_display()
    -> Result<(), Box<dyn std::error::Error>> {
        let demo = Demo::open()?;
        let first = demo.page()?;
        demo.session.set_pdf_zoom(BODY_VIEW, 2.0)?;
        complete_zoom(&demo, None)?;
        demo.session.set_pdf_zoom(BODY_VIEW, 1.5)?;
        assert!(
            !poll_pdf(&demo.session, &demo.runtime),
            "superseded result redrew"
        );
        assert_eq!(demo.session.pdf_zoom(BODY_VIEW)?, 1.0);
        assert!(
            demo.session
                .borrow()
                .pdf
                .placed(&demo.session.borrow().app)
                .is_some_and(|placed| Rc::ptr_eq(&placed, &first))
        );
        let zoomed = demo.page()?;
        assert_eq!(demo.session.pdf_zoom(BODY_VIEW)?, 1.5);
        assert_eq!(zoomed.len(), 240);
        let top = demo.session.borrow().app.top_row();

        demo.session.set_pdf_zoom(BODY_VIEW, 1.25)?;
        complete_zoom(&demo, Some(Failure::worker()))?;
        let logs = demo.directory.path().join("logs");
        logging::capture_for_test(&logs, || assert!(poll_pdf(&demo.session, &demo.runtime)));
        assert!(Rc::ptr_eq(&demo.page()?, &zoomed));
        assert_eq!(demo.session.pdf_zoom(BODY_VIEW)?, 1.5);
        assert_eq!(demo.session.borrow().app.top_row(), top);
        assert_eq!(demo.session.borrow().app.notice(), Some(ZOOM_NOTICE));
        Ok(())
    }

    #[test]
    fn resize_can_reject_an_accepted_zoom_without_changing_the_committed_scale()
    -> Result<(), Box<dyn std::error::Error>> {
        let demo = Demo::open_at(Geometry {
            columns: 32,
            ..FIRST
        })?;
        let original = demo.page()?;
        demo.session.set_pdf_zoom(BODY_VIEW, 12.0)?;
        complete_zoom(&demo, None)?;
        demo.session.borrow_mut().pdf_geometry = Ok(Geometry {
            columns: 128,
            ..FIRST
        });
        let logs = demo.directory.path().join("logs");
        let fitted = logging::capture_for_test(&logs, || demo.page())?;
        assert_eq!(demo.session.pdf_zoom(BODY_VIEW)?, 1.0);
        assert_eq!(fitted.len(), 256);
        assert_eq!(demo.session.borrow().app.notice(), Some(ZOOM_NOTICE));
        assert_eq!(original.len(), 64);
        Ok(())
    }

    #[test]
    fn opening_another_pdf_cannot_publish_an_old_zoom_and_history_restores_the_confirmed_zoom()
    -> Result<(), Box<dyn std::error::Error>> {
        let demo = Demo::open()?;
        demo.page()?;
        demo.session.set_pdf_zoom(BODY_VIEW, 1.5)?;
        let previous = demo.page()?;
        let id = demo.session.borrow().app.document_id();
        demo.session.set_pdf_zoom(BODY_VIEW, 2.0)?;
        complete_zoom(&demo, None)?;

        let other = demo.directory.path().join("other.pdf");
        fs::write(&other, crate::test_pdf::two_pages())?;
        assert!(matches!(
            demo.session.open_file(BODY_VIEW, &other)?,
            FileOpen::Pending
        ));
        let current = demo.page()?;
        assert_ne!(demo.session.borrow().app.document_id(), id);
        assert_eq!(demo.session.pdf_zoom(BODY_VIEW)?, 0.15);
        assert_eq!(current.len(), 24);
        demo.session
            .borrow_mut()
            .apply(Action::JumpBack, current.as_ref(), "");
        assert_eq!(demo.page()?.len(), previous.len());
        assert_eq!(demo.session.borrow().app.document_id(), id);
        assert_eq!(demo.session.pdf_zoom(BODY_VIEW)?, 1.5);
        Ok(())
    }

    #[test]
    fn async_pdf_turns_render_pixels_after_open_and_history_restore()
    -> Result<(), Box<dyn std::error::Error>> {
        use image::GenericImageView as _;

        let demo = Demo::open()?;
        let check = |index, color| -> Result<PlacedPage, Box<dyn std::error::Error>> {
            let layout = demo.page()?;
            let session = demo.session.borrow();
            let page = active_page(&session.app).ok_or("expected PDF")?;
            assert_eq!(page.index, index);
            let raster = page.decode().map_err(io::Error::from)?;
            let (w, h) = raster.dimensions();
            assert_eq!(
                raster.get_pixel(w / 2, 3 * h / 4).0,
                color,
                "page {} content",
                index + 1
            );
            Ok(layout)
        };
        let red = [255, 0, 0, 255];
        let blue = [0, 0, 255, 255];

        let first = check(0, red)?;
        let pdf_id = demo.session.borrow().app.document_id();
        demo.move_by(Motion::End, &first);
        demo.move_by(Motion::LineDown, &first);
        let second = check(1, blue)?;
        demo.move_by(Motion::LineUp, &second);
        check(0, red)?;

        let other = demo.directory.path().join("other.md");
        fs::write(&other, "Other Markdown")?;
        assert!(matches!(
            demo.session.open_file(BODY_VIEW, &other)?,
            FileOpen::Opened
        ));
        {
            let mut state = demo.session.borrow_mut();
            assert!(active_page(&state.app).is_none());
            state.pdf.depart(pdf_id);
        }
        wait(&demo.session, &demo.runtime)?;
        demo.session.borrow_mut().apply(Action::JumpBack, &[], "");
        let restored = check(0, red)?;
        demo.move_by(Motion::End, &restored);
        demo.move_by(Motion::LineDown, &restored);
        check(1, blue)?;
        Ok(())
    }

    #[test]
    fn scroll_during_width_refit_cannot_turn_from_the_old_layout()
    -> Result<(), Box<dyn std::error::Error>> {
        let narrow = Geometry {
            columns: 24,
            ..FIRST
        };
        let demo = Demo::open_at(narrow)?;
        let old = place(&mut demo.session.borrow_mut(), 79)?;
        assert_eq!(old.len(), 48);
        demo.session.borrow_mut().pdf_geometry = Ok(FIRST);
        demo.move_by(Motion::LineDown, &old);
        wait(&demo.session, &demo.runtime)?;
        let current = place(&mut demo.session.borrow_mut(), 79)?;
        assert_eq!(current.len(), 160);
        assert_eq!(
            page_key(&demo.session.borrow().app).map(|key| key.1),
            Some(0)
        );
        assert_eq!(demo.session.borrow().app.top_row(), 1);
        Ok(())
    }

    #[test]
    fn resize_rechecks_even_an_already_completed_turn_before_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        for geometry_changes in [false, true] {
            let narrow = Geometry {
                columns: 24,
                ..FIRST
            };
            let demo = Demo::open_at(narrow)?;
            let old = place(&mut demo.session.borrow_mut(), 79)?;
            demo.move_by(Motion::LineDown, &old);
            {
                let mut state = demo.session.borrow_mut();
                let request = state.pdf.queued.take().ok_or("expected turn")?;
                assert!(matches!(request.intent, Intent::Turn { .. }));
                // Wait without polling, then deliver the completed result to make this race deterministic.
                let target = Target {
                    geometry: narrow,
                    scale: Scale::Fixed(1.0),
                };
                let completed = state
                    .pdf
                    .submit(request.work, target, None)
                    .map_err(io::Error::from)?
                    .recv_timeout(Duration::from_secs(15))?;
                let (send, result) = mpsc::channel();
                assert!(send.send(completed).is_ok());
                state.pdf.running = Some(Running {
                    intent: Some(request.intent),
                    target,
                    marks: None,
                    result,
                });
                if geometry_changes {
                    state.pdf_geometry = Ok(FIRST);
                } else {
                    state.app.reflow(old.as_ref(), 24);
                }
            }
            wait(&demo.session, &demo.runtime)?;
            assert_eq!(
                page_key(&demo.session.borrow().app).map(|key| key.1),
                Some(0)
            );
            assert_eq!(demo.session.borrow().app.top_row(), 1);
        }
        Ok(())
    }

    #[test]
    fn tall_page_turns_only_after_reaching_the_edge_and_does_not_add_history()
    -> Result<(), Box<dyn std::error::Error>> {
        let demo = Demo::open()?;
        let first = demo.page()?;
        let id = demo.session.borrow().app.document_id();
        assert_eq!(page_key(&demo.session.borrow().app), Some((id, 0)));
        assert_eq!(first.len(), 160);
        assert!(demo.session.borrow().app.links().is_empty());
        demo.move_by(Motion::LineUp, &first);
        assert!(demo.session.borrow().pdf.pending().is_none());
        for top in [24, 48, 72, 96, 120, 136] {
            demo.move_by(Motion::PageDown, &first);
            let state = demo.session.borrow();
            assert_eq!(state.app.top_row(), top);
            assert!(
                state.pdf.pending().is_none(),
                "reaching the bottom must not turn the page"
            );
            assert!(state.app.notice().is_none());
        }
        demo.move_by(Motion::LineDown, &first);
        demo.move_by(Motion::LineDown, &first);
        let second = demo.page()?;
        assert_eq!(page_key(&demo.session.borrow().app), Some((id, 1)));
        assert_eq!(demo.session.borrow().app.top_row(), 0);

        demo.move_by(Motion::End, &second);
        demo.move_by(Motion::PageDown, &second);
        assert!(demo.session.borrow().pdf.pending().is_none());
        assert!(demo.session.borrow().app.notice().is_none());

        demo.move_by(Motion::Start, &second);
        demo.move_by(Motion::LineUp, &second);
        let first = demo.page()?;
        assert_eq!(page_key(&demo.session.borrow().app), Some((id, 0)));
        assert_eq!(demo.session.borrow().app.top_row(), 136);
        demo.session
            .borrow_mut()
            .apply(Action::JumpBack, first.as_ref(), "");
        assert_eq!(
            demo.session.borrow().app.document_id(),
            demo.original,
            "turns must not create history entries"
        );
        Ok(())
    }

    #[test]
    fn resize_and_document_history_restore_the_position_in_the_current_pdf_page()
    -> Result<(), Box<dyn std::error::Error>> {
        let demo = Demo::open()?;
        let first = demo.page()?;
        let id = demo.session.borrow().app.document_id();
        let doc = demo.session.document(BODY_VIEW)?.ok_or("no PDF document")?;
        assert_eq!(doc.kind, DocumentKind::Pdf);
        demo.move_by(Motion::End, &first);
        demo.move_by(Motion::LineDown, &first);
        let second = demo.page()?;
        assert_eq!(second.len(), 80);
        assert_eq!(demo.session.document(BODY_VIEW)?, Some(doc));
        demo.move_by(Motion::PageDown, &second);
        demo.move_by(Motion::LineDown, &second);
        assert_eq!(demo.session.borrow().app.top_row(), 25);

        demo.session.borrow_mut().pdf_geometry = Ok(Geometry {
            columns: 40,
            ..FIRST
        });
        let narrow = demo.page()?;
        assert_eq!(narrow.len(), 40);
        assert_eq!(demo.session.borrow().app.top_row(), 12);
        assert_eq!(demo.session.document(BODY_VIEW)?, Some(doc));

        let other = demo.directory.path().join("other.md");
        fs::write(&other, "Other Markdown")?;
        assert!(matches!(
            demo.session.open_file(BODY_VIEW, &other)?,
            FileOpen::Opened
        ));
        let mut state = demo.session.borrow_mut();
        state.pdf.depart(id);
        state.pdf_geometry = Ok(FIRST);
        state.apply(Action::JumpBack, &[], "");
        drop(state);

        let restored = demo.page()?;
        assert_eq!(page_key(&demo.session.borrow().app), Some((id, 1)));
        assert_eq!(demo.session.borrow().app.top_row(), 25);
        assert_eq!(restored.len(), 80);
        assert_ne!(demo.session.document(BODY_VIEW)?, Some(doc));
        Ok(())
    }

    #[test]
    fn unplaceable_pdf_history_keeps_the_current_document_and_can_retry_after_resize()
    -> Result<(), Box<dyn std::error::Error>> {
        let narrow = Geometry {
            columns: 32,
            ..FIRST
        };
        let wide = Geometry {
            columns: 200,
            ..FIRST
        };
        for extension in ["md", "pdf"] {
            let demo = Demo::open_at(narrow)?;
            demo.page()?;
            demo.session.set_pdf_zoom(BODY_VIEW, 8.0)?;
            let zoomed = demo.page()?;
            assert_eq!(zoomed.len(), 512);
            demo.move_by(Motion::PageDown, &zoomed);
            let saved = demo.session.document(BODY_VIEW)?.ok_or("no PDF document")?;
            let (saved_id, saved_top) = {
                let state = demo.session.borrow();
                (state.app.document_id(), state.app.top_row())
            };

            let other = demo.directory.path().join(format!("other.{extension}"));
            if extension == "pdf" {
                fs::write(&other, crate::test_pdf::two_pages())?;
            } else {
                fs::write(&other, "Other Markdown")?;
            }
            demo.session.open_file(BODY_VIEW, &other)?;
            wait(&demo.session, &demo.runtime)?;
            {
                let mut state = demo.session.borrow_mut();
                state.pdf.depart(saved_id); // leaving the document releases its tiles
                state.pdf_geometry = Ok(wide);
            }
            wait(&demo.session, &demo.runtime)?;
            let (current_id, current_layout) = {
                let state = demo.session.borrow();
                (state.app.document_id(), state.pdf.placed(&state.app))
            };
            let current = demo
                .session
                .document(BODY_VIEW)?
                .ok_or("no current document")?;
            let mut input = InputQueue::default();
            let jump = |input: &mut InputQueue| {
                assert!(input.dispatch(
                    Some(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
                    &demo.session,
                    &demo.runtime,
                    &[],
                    "",
                ));
            };
            let logs = demo.directory.path().join("logs");
            logging::capture_for_test(&logs, || {
                jump(&mut input);
                wait(&demo.session, &demo.runtime)
            })?;
            assert_eq!(
                demo.session.document(BODY_VIEW)?,
                Some(current),
                "{extension}"
            );
            {
                let state = demo.session.borrow();
                assert_eq!(state.app.document_id(), current_id);
                assert_eq!(state.app.document_path(), other);
                assert!(
                    state
                        .app
                        .notice()
                        .is_some_and(|notice| notice.contains("display limits"))
                );
                let placed = state.pdf.placed(&state.app);
                match (&current_layout, &placed) {
                    (Some(before), Some(after)) => assert!(Rc::ptr_eq(before, after)),
                    (None, None) => (),
                    _ => panic!("the current PDF placement changed"),
                }
            }

            demo.session.borrow_mut().pdf_geometry = Ok(narrow);
            jump(&mut input);
            let restored = demo.page()?;
            assert_eq!(restored.len(), 512);
            assert_eq!(demo.session.pdf_zoom(BODY_VIEW)?, 8.0);
            let state = demo.session.borrow();
            assert_eq!(state.app.document_id(), saved_id);
            assert_eq!(state.app.top_row(), saved_top);
            assert_eq!(state.app.notice(), None);
            drop(state);
            let restored_doc = demo.session.document(BODY_VIEW)?.ok_or("no restored PDF")?;
            assert_eq!(restored_doc.epoch, current.epoch + 1);
            assert_ne!(restored_doc, saved);

            jump(&mut input); // the prior entry is still the original Markdown
            assert_eq!(demo.session.borrow().app.document_id(), demo.original);
        }
        Ok(())
    }

    #[test]
    fn pdf_history_waits_for_cell_size_then_rechecks_the_completed_layout()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{session::poll_pdf_with_cell_size, terminal::CellSizeStatus};

        let geometry = Geometry {
            columns: 32,
            ..FIRST
        };
        let large = Geometry {
            cell_width: 12,
            cell_height: 24,
            ..geometry
        };
        let compatible = Geometry {
            cell_width: 3,
            cell_height: 6,
            ..geometry
        };
        for (status, refreshed, expected_notice) in [
            (CellSizeStatus::Ready, Ok(large), Some("display limits")),
            (CellSizeStatus::Ready, Ok(compatible), None),
            (CellSizeStatus::Unconfirmed, Ok(geometry), Some("cell size")),
            (
                CellSizeStatus::Ready,
                Err(Failure::terminal()),
                Some("Kitty"),
            ),
        ] {
            let demo = Demo::open_at(geometry)?;
            demo.page()?;
            demo.session.set_pdf_zoom(BODY_VIEW, 8.0)?;
            let original = demo.page()?;
            let id = demo.session.borrow().app.document_id();
            let other = demo.directory.path().join("other.md");
            fs::write(&other, "Other Markdown")?;
            demo.session.open_file(BODY_VIEW, &other)?;
            let current = demo
                .session
                .document(BODY_VIEW)?
                .ok_or("no current document")?;
            let mut keys = InputQueue::default();
            let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
            assert!(keys.dispatch(Some(ctrl_o), &demo.session, &demo.runtime, &[], ""));
            poll_pdf(&demo.session, &demo.runtime); // submit using the old dimensions
            {
                let mut state = demo.session.borrow_mut();
                let mut running = state.pdf.running.take().ok_or("expected history render")?;
                let completed = running.result.recv_timeout(Duration::from_secs(15))?;
                assert!(
                    matches!(&completed, Ok((_, Ok(rendered))) if rendered.layout.len() == original.len())
                );
                let (send, result) = mpsc::channel();
                assert!(send.send(completed).is_ok());
                running.result = result;
                state.pdf.running = Some(running);
            }

            // Replay bypasses the console pump: the row count changed, but the
            // current column count and cached cell geometry still match the job.
            keys.replay
                .push_back(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
            let key = keys.pop_replayed();
            assert!(keys.dispatch(key, &demo.session, &demo.runtime, &[], ""));
            assert!(!poll_pdf_with_cell_size(
                &demo.session,
                &demo.runtime,
                CellSizeStatus::Pending
            ));
            assert_eq!(demo.session.document(BODY_VIEW)?, Some(current));

            demo.session.borrow_mut().pdf_geometry = refreshed;
            let logs = demo.directory.path().join("logs");
            logging::capture_for_test(&logs, || -> io::Result<()> {
                poll_pdf_with_cell_size(&demo.session, &demo.runtime, status);
                wait(&demo.session, &demo.runtime)
            })?;
            if let Some(expected) = expected_notice {
                assert_eq!(demo.session.document(BODY_VIEW)?, Some(current));
                assert!(
                    demo.session
                        .borrow()
                        .app
                        .notice()
                        .is_some_and(|notice| notice.contains(expected))
                );
                assert!(
                    demo.session.borrow().pdf.pending().is_none(),
                    "finished query left a pending Restore"
                );

                demo.session.borrow_mut().pdf_geometry = Ok(geometry);
                assert!(keys.dispatch(Some(ctrl_o), &demo.session, &demo.runtime, &[], ""));
            }
            let restored = demo.page()?;
            assert_eq!(restored.len(), original.len());
            assert_eq!(demo.session.pdf_zoom(BODY_VIEW)?, 8.0);
            assert_eq!(demo.session.borrow().app.document_id(), id);
            assert_eq!(
                demo.session
                    .document(BODY_VIEW)?
                    .ok_or("no restored PDF")?
                    .epoch,
                current.epoch + 1
            );
            assert!(keys.dispatch(Some(ctrl_o), &demo.session, &demo.runtime, &[], ""));
            assert_eq!(
                demo.session.borrow().app.document_id(),
                demo.original,
                "history was consumed more than once"
            );
        }
        Ok(())
    }

    #[test]
    fn failed_and_cancelled_requests_do_not_publish_and_turn_can_retry()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let private = dir.path().join("private-pdf-directory");
        fs::create_dir(&private)?;
        let logs = dir.path().join("logs");
        let good = private.join("good.pdf");
        let bad = private.join("bad.pdf");
        fs::write(&good, crate::test_pdf::two_pages())?;
        fs::write(&bad, "PDF content must not be logged")?;
        let state = test_session();
        let original = state.borrow().app.document_id();
        let markdown = state.document(BODY_VIEW)?.ok_or("no Markdown document")?;
        assert_eq!(markdown.kind, DocumentKind::Markdown);
        let (runtime, error) = Runtime::load_from(state.clone(), None);
        assert!(error.is_none());
        let float = state.open_float(BODY_VIEW, FloatOptions::default())?;

        logging::capture_for_test(&logs, || -> Result<(), Box<dyn std::error::Error>> {
            assert!(matches!(
                state.open_file(BODY_VIEW, &bad)?,
                FileOpen::Pending
            ));
            assert_eq!(state.document(BODY_VIEW)?, Some(markdown));
            wait(&state, &runtime)?;
            assert_eq!(state.document(BODY_VIEW)?, Some(markdown));
            Ok(())
        })?;
        assert_eq!(state.borrow().floats.active(), Some((BODY_VIEW, float)));
        assert_eq!(state.borrow().app.document_id(), original);
        assert!(state.borrow().app.notice().is_some());

        assert!(matches!(
            state.open_file(BODY_VIEW, &good)?,
            FileOpen::Pending
        ));
        poll_pdf(&state, &runtime); // hand off to worker before cancelling the originating float
        state.close_float(BODY_VIEW, float)?;
        wait(&state, &runtime)?;
        assert_eq!(state.borrow().app.document_id(), original);

        assert!(matches!(
            state.open_file(BODY_VIEW, &good)?,
            FileOpen::Pending
        ));
        assert_eq!(state.document(BODY_VIEW)?, Some(markdown));
        wait(&state, &runtime)?;
        let pdf = state.document(BODY_VIEW)?.ok_or("no PDF document")?;
        assert_eq!(pdf.kind, DocumentKind::Pdf);
        assert_ne!(pdf.epoch, markdown.epoch);
        state.set_pdf_zoom(BODY_VIEW, 1.0)?;
        wait(&state, &runtime)?;
        let first = place(&mut state.borrow_mut(), 24)?;
        let id = state.borrow().app.document_id();
        {
            let mut session = state.borrow_mut();
            session.apply(Action::Move(Motion::End), first.as_ref(), "");
            session.apply(Action::Move(Motion::LineDown), first.as_ref(), "");
            // Substitute only the worker's result, so failure is deterministic even on
            // Windows installations that cache both pages in the native PDF object.
            let intent = session
                .pdf
                .queued
                .take()
                .ok_or_else(|| io::Error::other("no turn requested"))?
                .intent;
            let (send, result) = mpsc::channel();
            send.send(Err(Failure::worker()))
                .map_err(|_| io::Error::other("worker disconnected"))?;
            session.pdf.running = Some(Running {
                intent: Some(intent),
                target: Target {
                    geometry: FIRST,
                    scale: Scale::Fixed(1.0),
                },
                marks: None,
                result,
            });
        }
        logging::capture_for_test(&logs, || {
            assert!(poll_pdf(&state, &runtime));
            assert!(!poll_pdf(&state, &runtime));
        });
        assert_eq!(page_key(&state.borrow().app), Some((id, 0)));
        assert_eq!(state.borrow().app.top_row(), 136);
        assert_eq!(state.borrow().app.notice(), Some(TURN_NOTICE));
        state
            .borrow_mut()
            .apply(Action::Move(Motion::LineDown), first.as_ref(), "");
        wait(&state, &runtime)?;
        assert_eq!(page_key(&state.borrow().app), Some((id, 1)));
        assert!(state.borrow().app.notice().is_none());

        let content = fs::read_to_string(logs.join("tvi.log"))?;
        assert!(!content.contains("private-pdf-directory"));
        assert!(!content.contains("PDF content must not be logged"));
        let records = content
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["event"], "document.open_failed");
        assert_eq!(records[0]["trigger"], "open_file");
        assert_eq!(records[0]["document"], "bad.pdf");
        assert_eq!(records[0]["level"], "WARN");
        assert_eq!(records[1]["event"], "pdf.page_failed");
        assert_eq!(records[1]["trigger"], "turn");
        assert_eq!(records[1]["document"], "good.pdf");
        assert_eq!(records[1]["page"], 2);
        Ok(())
    }

    #[test]
    #[ignore = "requires fd or fdfind on PATH"]
    fn bundled_ff_selects_pdf_and_only_closes_after_success()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("manual.PDF");
        let markdown = dir.path().join("start.md");
        fs::write(&path, crate::test_pdf::two_pages())?;
        fs::write(&markdown, "# Original")?;
        fs::create_dir(dir.path().join(".git"))?;
        let state = Rc::new(RefCell::new(Session::with_images(
            super::super::read_document(&markdown)?,
            KITTY,
            None,
        )));
        let (runtime, error) = Runtime::load_from(state.clone(), None);
        assert!(error.is_none());
        let mut keys = InputQueue::default();
        let press = |keys: &mut InputQueue, code| {
            keys.dispatch(
                Some(KeyEvent::new(code, KeyModifiers::NONE)),
                &state,
                &runtime,
                &[],
                "",
            );
        };
        press(&mut keys, KeyCode::Char('f'));
        press(&mut keys, KeyCode::Char('f'));
        let float = state
            .borrow()
            .floats
            .active()
            .ok_or_else(|| io::Error::other("picker not open"))?;
        press(&mut keys, KeyCode::Enter);
        assert_eq!(state.borrow().floats.active(), Some(float));
        assert_eq!(state.borrow().app.document_path(), markdown);
        assert!(
            state.borrow().pdf.pending().is_some(),
            "PDF was not requested"
        );
        wait(&state, &runtime)?;
        assert!(
            state.borrow().floats.active().is_none(),
            "{:?}",
            state.borrow().app.notice()
        );
        assert_eq!(
            state.borrow().app.document_path().canonicalize()?,
            path.canonicalize()?
        );
        let first = place(&mut state.borrow_mut(), 24)?;
        assert_eq!(first.len(), 24);
        assert_eq!(state.pdf_zoom(BODY_VIEW)?, 0.15);
        // Run another Lua mapping to make sure cleanup also released the Lua handle.
        press(&mut keys, KeyCode::Char('f'));
        press(&mut keys, KeyCode::Char('f'));
        assert!(state.borrow().floats.active().is_some());
        press(&mut keys, KeyCode::Esc);
        Ok(())
    }
}
