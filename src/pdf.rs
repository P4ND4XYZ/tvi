use std::{
    io,
    path::Path,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

#[cfg(windows)]
use std::collections::HashMap;

use crate::images::Budget;

pub(crate) mod text;
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
    lease: Arc<Lease>,
    pub count: u32,
}

#[cfg(windows)]
struct Lease {
    id: u64,
    sender: mpsc::Sender<Message>,
    input: Arc<text::Input>,
}

impl Source {
    pub(crate) fn search(
        &self,
        query: Arc<str>,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<mpsc::Receiver<text::ResultHits>, Failure> {
        #[cfg(windows)]
        return text::spawn(self.lease.input.clone(), self.count, query, cancelled);
        #[cfg(not(windows))]
        {
            let _ = (self, query, cancelled);
            Err(Failure::platform())
        }
    }
}

#[cfg(windows)]
impl Drop for Lease {
    fn drop(&mut self) {
        // Last page or pending operation referencing this document was discarded.
        drop(self.sender.send(Message::Release(self.id)));
    }
}

#[derive(Clone)]
pub(crate) struct Page {
    pub source: Source,
    pub index: u32,
    // Keep a bounded raster for the current page of each document in history, not an
    // entire PDF's worth of decoded pages or terminal placements.
    png: Arc<[u8]>,
    aspect_height: f64,
    searchable_geometry: bool,
}

impl Page {
    pub fn decode(&self) -> Result<image::DynamicImage, Failure> {
        Budget::default().png(&self.png).ok_or_else(|| {
            Failure::new(
                "pdf_png",
                "invalid_png",
                "Could not decode the PDF page image",
            )
        })
    }

    pub(crate) fn supports_hit(&self, hit: text::Hit) -> bool {
        self.index == hit.page
            && self.searchable_geometry
            && (self.aspect_height / hit.aspect_height - 1.0).abs() < 0.001
    }

    pub fn aspect_height(&self) -> f64 {
        self.aspect_height
    }
}

pub(crate) struct Renderer {
    #[cfg(windows)]
    documents: HashMap<u64, windows::Document>,
    #[cfg(windows)]
    next_id: u64,
    #[cfg(windows)]
    sender: mpsc::Sender<Message>,
    // Field order ensures that native documents are dropped before RoUninitialize.
    #[cfg(windows)]
    _apartment: windows::Apartment,
}

impl Renderer {
    fn new(sender: mpsc::Sender<Message>) -> Result<Self, Failure> {
        #[cfg(windows)]
        return Ok(Self {
            documents: HashMap::new(),
            next_id: 0,
            sender,
            _apartment: windows::Apartment::initialize()?,
        });
        #[cfg(not(windows))]
        {
            let _ = sender;
            Err(Failure::platform())
        }
    }

    pub fn open(&mut self, path: &Path) -> Result<Page, Failure> {
        #[cfg(windows)]
        {
            let (document, input) = windows::Document::open(path)?;
            let id = self.next_id;
            self.next_id += 1;
            let source = Source {
                lease: Arc::new(Lease {
                    id,
                    sender: self.sender.clone(),
                    input: Arc::new(input),
                }),
                count: document.count,
            };
            let page = document.render(source, 0)?;
            let _previous = self.documents.insert(id, document);
            Ok(page)
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            Err(Failure::platform())
        }
    }

    pub fn render(&self, source: Source, index: u32) -> Result<Page, Failure> {
        #[cfg(windows)]
        return self
            .documents
            .get(&source.lease.id)
            .ok_or_else(Failure::worker)?
            .render(source, index);
        #[cfg(not(windows))]
        {
            let _ = (source, index);
            Err(Failure::platform())
        }
    }
}

type Task = Box<dyn FnOnce(Result<&mut Renderer, Failure>) + Send>;

enum Message {
    Run(Task),
    #[cfg(windows)]
    Release(u64),
    Shutdown,
}

const SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

#[derive(Default)]
pub(crate) struct Worker(Option<(mpsc::Sender<Message>, mpsc::Receiver<()>)>);

impl Worker {
    pub fn submit<T: Send + 'static>(
        &mut self,
        run: impl FnOnce(&mut Renderer) -> Result<T, Failure> + Send + 'static,
    ) -> Result<mpsc::Receiver<Result<T, Failure>>, Failure> {
        if self.0.is_none() {
            let (sender, receiver) = mpsc::channel();
            let (finished, done) = mpsc::channel();
            let worker_sender = sender.clone();
            let thread = thread::Builder::new()
                .name("tvi-pdf".into())
                .spawn(move || {
                    let mut renderer = None;
                    for message in receiver {
                        match message {
                            Message::Run(task) => {
                                let renderer = match &mut renderer {
                                    Some(renderer) => Ok(renderer),
                                    slot @ None => Renderer::new(worker_sender.clone())
                                        .map(|renderer| slot.insert(renderer)),
                                };
                                task(renderer);
                            }
                            #[cfg(windows)]
                            Message::Release(id) => {
                                if let Some(renderer) = &mut renderer {
                                    drop(renderer.documents.remove(&id));
                                }
                            }
                            Message::Shutdown => break,
                        }
                    }
                    // Acknowledge only after releasing the documents and WinRT on this thread.
                    drop(renderer);
                    let _notification = finished.send(());
                })
                .map_err(|_| Failure::worker())?;
            // In-flight file or native calls may not return; never join this thread.
            drop(thread);
            self.0 = Some((sender, done));
        }
        let (sender, _) = self.0.as_ref().ok_or_else(Failure::worker)?;
        let (send, receive) = mpsc::channel();
        sender
            .send(Message::Run(Box::new(move |renderer| {
                drop(send.send(renderer.and_then(run)));
            })))
            .map_err(|_| Failure::worker())?;
        Ok(receive)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some((sender, done)) = self.0.take() {
            // Leases can still hold senders, so closing the channel isn't enough.
            drop(sender.send(Message::Shutdown));
            let _completion = done.recv_timeout(SHUTDOWN_GRACE);
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::{
        env,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    use super::*;

    const STALL_FIXTURE: &str = "TVI_TEST_STALLED_PDF_WORKER";
    const STALL_STARTED: &str = "PDF fixture reached the stall";

    #[test]
    fn stalled_pdf_worker_does_not_keep_the_process_alive() -> io::Result<()> {
        // Keep the intentionally blocked worker in a child so a regression cannot
        // hang this test harness or leave a worker running here.
        let mut child = Command::new(env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                "pdf::tests::stalled_pdf_worker_fixture",
                "--nocapture",
            ])
            .env(STALL_FIXTURE, "1")
            .stdout(Stdio::piped())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let waited = loop {
            match child.try_wait() {
                Ok(Some(_)) => break Ok(()),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    break Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "PDF process did not exit",
                    ));
                }
                Err(error) => break Err(error),
            }
        };
        let kill = waited.is_err().then(|| child.kill());
        let output = child.wait_with_output()?;
        if let Some(kill) = kill {
            kill?;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(STALL_STARTED),
            "fixture did not start: {stdout}"
        );
        waited?;
        assert!(output.status.success(), "{}: {stdout}", output.status);
        Ok(())
    }

    #[test]
    #[ignore = "run in a child test harness as a process fixture"]
    fn stalled_pdf_worker_fixture() -> Result<(), Box<dyn std::error::Error>> {
        if env::var_os(STALL_FIXTURE).is_none() {
            return Ok(());
        }
        let (_resume, stall) = mpsc::channel::<()>();
        let (started, running) = mpsc::channel();
        let mut worker = Worker::default();
        let result = worker
            .submit(move |_| {
                started.send(()).map_err(|_| Failure::worker())?;
                stall.recv().map_err(|_| Failure::worker())
            })
            .map_err(io::Error::from)?;
        running.recv_timeout(Duration::from_secs(5))?;
        println!("{STALL_STARTED}");
        drop(worker);
        assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
        Ok(())
    }
}
