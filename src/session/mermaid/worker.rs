use std::{
    io,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use image::DynamicImage;

use crate::{
    images::Budget,
    markdown::mermaid::Diagram,
    mermaid::{Conversion, Failure, Mermaid, TimeoutScope},
};

pub(super) struct Request {
    pub renderer: Mermaid,
    pub diagrams: Vec<Diagram>,
    pub budget: Budget,
}

pub(in crate::session) enum Event {
    Diagram {
        ordinal: usize,
        line: usize,
        conversion: Conversion<DynamicImage>,
        timeout_limit: Option<Duration>,
    },
    Stopped(Option<io::ErrorKind>),
}

impl Event {
    pub(super) fn ends_batch(result: &Result<DynamicImage, Failure>) -> bool {
        matches!(
            result,
            Err(Failure::Launch { .. }
                | Failure::Unavailable
                | Failure::Timeout(TimeoutScope::Document))
        )
    }
}

#[derive(Default)]
pub(in crate::session) struct Jobs {
    running: Option<Worker>,
    queued: Option<Request>,
}

struct Worker {
    cancelled: Arc<AtomicBool>,
    handoff: Arc<Mutex<()>>,
    thread: JoinHandle<()>,
    events: Option<Receiver<Event>>,
}

impl Jobs {
    pub(super) fn queue(&mut self, request: Request) {
        self.queued = Some(request);
    }

    pub(in crate::session) fn cancel(&mut self) {
        drop(self.detach());
    }

    pub(in crate::session) fn cancel_for_history(&mut self) -> Vec<Event> {
        self.detach().map_or_else(Vec::new, |events| {
            // Cancellation and event delivery share the handoff lock. No new
            // result can enter this queue after detaching the receiver.
            events.try_iter().collect()
        })
    }

    fn detach(&mut self) -> Option<Receiver<Event>> {
        self.queued = None;
        let worker = self.running.as_mut()?;
        let _handoff = worker
            .handoff
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        worker.cancelled.store(true, Ordering::Relaxed);
        worker.events.take()
    }

    #[cfg(test)]
    pub(super) fn finished_with_receiver(&self) -> bool {
        self.running
            .as_ref()
            .is_some_and(|worker| worker.events.is_some() && worker.thread.is_finished())
    }

    pub(in crate::session) fn poll(&mut self) -> Vec<Event> {
        if self
            .running
            .as_ref()
            .is_some_and(|worker| worker.events.is_none() && worker.thread.is_finished())
            && let Some(worker) = self.running.take()
        {
            drop(worker.thread.join());
        }
        // Do not start a replacement until the previous renderer has cleaned up.
        if self.running.is_none()
            && let Some(request) = self.queued.take()
        {
            match Worker::start(request) {
                Ok(worker) => self.running = Some(worker),
                Err(error) => return vec![Event::Stopped(Some(error.kind()))],
            }
        }
        let mut output = Vec::new();
        if let Some(worker) = &mut self.running
            && let Some(events) = &worker.events
        {
            loop {
                match events.try_recv() {
                    Ok(event) => output.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        worker.events = None;
                        output.push(Event::Stopped(None));
                        break;
                    }
                }
            }
        }
        output
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.running.take() {
            drop(worker.thread.join());
        }
    }
}

impl Worker {
    fn start(mut request: Request) -> io::Result<Self> {
        let (send, events) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let job_cancelled = Arc::clone(&cancelled);
        let handoff = Arc::new(Mutex::new(()));
        let job_handoff = Arc::clone(&handoff);
        let thread = thread::Builder::new()
            .name("tvi-mermaid".into())
            .spawn(move || {
                let mut document = request
                    .renderer
                    .start_document()
                    .with_cancellation(&job_cancelled);
                for diagram in request.diagrams {
                    let conversion =
                        document.convert(&diagram.source, |bytes| request.budget.png(bytes));
                    let timeout_limit = match &conversion.result {
                        Err(Failure::Cancelled) => break,
                        Err(Failure::Timeout(scope)) => Some(document.timeout_limit(*scope)),
                        _ => None,
                    };
                    let stop = Event::ends_batch(&conversion.result);
                    if let Ok(image) = &conversion.result {
                        request.budget.charge(image);
                    }
                    let _handoff = job_handoff.lock().unwrap_or_else(PoisonError::into_inner);
                    if job_cancelled.load(Ordering::Relaxed)
                        || send
                            .send(Event::Diagram {
                                ordinal: diagram.ordinal,
                                line: diagram.line,
                                conversion,
                                timeout_limit,
                            })
                            .is_err()
                        || stop
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            cancelled,
            handoff,
            thread,
            events: Some(events),
        })
    }
}
