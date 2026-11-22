use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, TryRecvError},
};

use crate::{
    app::{App, Direction, PdfSearch},
    logging::{self, Level},
    pdf::{
        Failure, Source,
        text::{Hit, ResultHits},
    },
    ui::layout::Layout,
};

pub(super) const SEARCHING: &str = "Searching PDF… (Esc to cancel)";
pub(super) const GEOMETRY: Failure = Failure::new(
    "pdf_search",
    "unmapped_page",
    "Could not locate the match on this PDF page",
);

#[derive(Clone, Copy)]
pub(super) struct Origin {
    page: u32,
    start: f64,
    end: f64,
}

impl Origin {
    pub(super) fn new(page: u32, top: usize, layout: &Layout<'_>) -> Option<Self> {
        Some(Self {
            page,
            start: layout.pdf_row_boundary(top)?,
            end: layout.pdf_row_boundary(top + 1)?,
        })
    }

    fn select(self, hits: &[Hit], direction: Direction) -> usize {
        let before = |boundary| {
            hits.partition_point(|hit| {
                hit.page < self.page || (hit.page == self.page && hit.from_top < boundary)
            })
        };
        match direction {
            Direction::Forward => before(self.start) % hits.len(),
            Direction::Backward => before(self.end).checked_sub(1).unwrap_or(hits.len() - 1),
        }
    }
}

struct Request {
    from: (u64, u32),
    query: Arc<str>,
    direction: Direction,
    origin: Option<Origin>,
    hits: Option<Arc<[Hit]>>,
}

struct Extraction {
    cancelled: Arc<AtomicBool>,
    result: Receiver<ResultHits>,
}

impl Drop for Extraction {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub(super) struct SearchJobs {
    request: Option<Request>,
    running: Option<Extraction>,
}

impl SearchJobs {
    pub(super) fn from(&self) -> Option<(u64, u32)> {
        self.request.as_ref().map(|request| request.from)
    }

    pub(super) fn needs_origin(&self) -> Option<(u64, u32)> {
        self.request
            .as_ref()
            .filter(|request| request.origin.is_none())
            .map(|request| request.from)
    }

    pub(super) fn cancel(&mut self) {
        if self.request.take().is_some()
            && let Some(running) = &self.running
        {
            // The supervisor terminates and reaps the child. Keep the slot until
            // that cleanup completes, even if a newer request is already waiting.
            running.cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn start(
        &mut self,
        app: &mut App,
        from: (u64, u32),
        input: String,
        direction: Direction,
        origin: Option<Origin>,
    ) {
        self.cancel();
        let reuse = app
            .pdf_search()
            .filter(|previous| input.is_empty() || *previous.query == input);
        let (query, hits) = if let Some(previous) = reuse {
            (previous.query.clone(), Some(previous.hits.clone()))
        } else if input.is_empty() {
            app.set_notice("No previous search".into());
            return;
        } else {
            (input.into(), None)
        };
        self.request = Some(Request {
            from,
            query,
            direction,
            origin,
            hits,
        });
        app.set_notice(SEARCHING.into());
    }

    pub(super) fn poll(
        &mut self,
        source: Option<&Source>,
        origin: Option<Origin>,
    ) -> Result<Option<PdfSearch>, Failure> {
        self.receive()?;

        let Some(request) = &mut self.request else {
            return Ok(None);
        };
        request.origin = request.origin.or(origin);
        if let Some(hits) = &request.hits {
            let selected = if hits.is_empty() {
                0
            } else if let Some(origin) = request.origin {
                origin.select(hits, request.direction)
            } else {
                return Ok(None); // History placement is still being refitted.
            };
            let result = PdfSearch {
                query: request.query.clone(),
                direction: request.direction,
                hits: hits.clone(),
                selected,
                highlights: !hits.is_empty(),
            };
            self.request = None;
            return Ok(Some(result));
        }
        if self.running.is_some() {
            return Ok(None);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let result = source
            .ok_or_else(Failure::worker)
            .and_then(|source| source.search(request.query.clone(), cancelled.clone()));
        match result {
            Ok(result) => self.running = Some(Extraction { cancelled, result }),
            Err(failure) => {
                self.request = None;
                return Err(failure);
            }
        }
        Ok(None)
    }

    fn receive(&mut self) -> Result<(), Failure> {
        let Some(running) = &self.running else {
            return Ok(());
        };
        let result = match running.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Disconnected) => Err(Failure::worker()),
            Err(TryRecvError::Empty) => return Ok(()),
        };
        let cancelled = running.cancelled.load(Ordering::Relaxed);
        self.running = None;
        if !cancelled && let Some(request) = &mut self.request {
            match result {
                Ok(hits) => request.hits = Some(hits),
                Err(failure) => {
                    self.request = None;
                    return Err(failure);
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn busy(&self) -> bool {
        self.request.is_some() || self.running.is_some()
    }
}

pub(super) fn fail(app: &mut App, failure: Failure) {
    logging::emit(
        Level::Warn,
        "pdf.search_failed",
        serde_json::json!({
            "stage": failure.stage, "reason": failure.reason,
        }),
    );
    app.set_notice(failure.message.into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn supersession_keeps_the_extraction_slot_until_the_cancelled_worker_finishes() {
        let mut app = App::new("test".into(), String::new());
        let mut jobs = SearchJobs::default();
        let from = (app.document_id(), 0);
        jobs.start(&mut app, from, "first".into(), Direction::Forward, None);
        let (send, result) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        jobs.running = Some(Extraction {
            cancelled: cancelled.clone(),
            result,
        });

        for query in ["second", "third"] {
            jobs.start(&mut app, from, query.into(), Direction::Forward, None);
            assert!(cancelled.load(Ordering::Relaxed));
            // Without a source a launch would fail, so None confirms the slot is still occupied.
            assert!(matches!(jobs.poll(None, None), Ok(None)));
        }
        assert!(send.send(Ok(Arc::from([]))).is_ok());
        // The stale result must not be returned even though the originating page is unchanged.
        assert!(
            matches!(jobs.poll(None, None), Err(failure) if failure.reason == "worker_unavailable")
        );
        assert!(!jobs.busy());
    }
}
