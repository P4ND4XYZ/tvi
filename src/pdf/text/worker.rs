use std::{
    env,
    io::{self, IsTerminal as _, Read as _, Write as _},
    os::windows::process::CommandExt as _,
    process::{Child, Command, ExitCode, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::windows_job::Job;

use super::{
    Bounds, Failure, Hit, ResultHits,
    extract::{CHANGED, FAILED, Input, LIMIT, MAX_HITS, MAX_PAGES, find},
};

const SWITCH: &str = "--internal-pdf-text";
const MAX_QUERY: usize = 16 * 1024;
const MAX_REQUEST: usize = 256 * 1024;
const HIT_BYTES: usize = 4 + 6 * 8;
const MAX_RESPONSE: usize = 1 + MAX_HITS * HIT_BYTES;
const MEMORY: usize = 256 * 1024 * 1024;
const TIME: Duration = Duration::from_secs(10);
const TIMEOUT: Failure = Failure::new("pdf_search", "timeout", "PDF search took too long");
const PROCESS: Failure = Failure::new(
    "pdf_search",
    "extractor_exit",
    "PDF text extraction stopped (unsupported PDF or resource limit)",
);

pub(in crate::pdf) fn spawn(
    input: Arc<Input>,
    count: u32,
    query: Arc<str>,
    cancelled: Arc<AtomicBool>,
) -> Result<mpsc::Receiver<ResultHits>, Failure> {
    if count > MAX_PAGES || query.len() > MAX_QUERY {
        return Err(LIMIT);
    }
    let (send, receive) = mpsc::channel();
    let worker = thread::Builder::new()
        .name("tvi-pdf-text-supervisor".into())
        .spawn(move || {
            let result = search(&input, count, &query, &cancelled);
            drop(send.send(result));
        })
        .map_err(|_| Failure::worker())?;
    drop(worker);
    Ok(receive)
}

fn search(input: &Input, count: u32, query: &str, cancelled: &AtomicBool) -> ResultHits {
    search_with_limit(input, count, query, cancelled, MEMORY)
}

fn search_with_limit(
    input: &Input,
    count: u32,
    query: &str,
    cancelled: &AtomicBool,
    memory: usize,
) -> ResultHits {
    let packet = serde_json::to_vec(&(input.packet()?, count, query)).map_err(|_| FAILED)?;
    if packet.len() > MAX_REQUEST {
        return Err(LIMIT);
    }
    let mut command = Command::new(env::current_exe().map_err(|_| Failure::worker())?);
    #[cfg(not(test))]
    command.arg(SWITCH);
    #[cfg(test)]
    command
        .args([
            "--ignored",
            "--exact",
            "pdf::text::worker::tests::extractor_fixture",
        ])
        .env_remove("RUST_TEST_NOCAPTURE");
    let output = supervise(&mut command, &packet, cancelled, TIME, memory)?;
    let output = output.as_slice();
    // libtest writes a header before it calls the worker entry fixture. The
    // production executable writes only the binary protocol to its pipe.
    #[cfg(test)]
    let output = output.strip_prefix(b"\nrunning 1 test\n").ok_or(PROCESS)?;
    decode(output, count)
}

struct Running {
    child: Child,
    job: Option<Job>,
}

impl Drop for Running {
    fn drop(&mut self) {
        // Kill on job close also applies if the viewer exits before this runs.
        drop(self.job.take());
        drop(self.child.kill());
        drop(self.child.wait());
    }
}

fn supervise(
    command: &mut Command,
    packet: &[u8],
    cancelled: &AtomicBool,
    time: Duration,
    memory: usize,
) -> Result<Vec<u8>, Failure> {
    let started = Instant::now();
    if cancelled.load(Ordering::Relaxed) {
        return Err(FAILED);
    }
    let job = Job::new(Some(memory)).map_err(|_| Failure::worker())?;
    let child = command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Failure::worker())?;
    let running = Running {
        child,
        job: Some(job),
    };
    // No PDF path or query is passed until the child is inside its job. If the
    // parent exits before attachment, EOF without a request makes the child exit.
    if let Some(job) = &running.job {
        job.attach(&running.child).map_err(|_| Failure::worker())?;
    }
    thread::scope(|scope| {
        // Drop/terminate the child before the scope waits for pipe I/O on failure.
        let mut running = running;
        let stdin = running.child.stdin.take().ok_or_else(Failure::worker)?;
        let stdout = running.child.stdout.take().ok_or_else(Failure::worker)?;
        let pipes = thread::Builder::new()
            .spawn_scoped(scope, move || exchange(stdin, stdout, packet))
            .map_err(|_| Failure::worker())?;
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(FAILED);
            }
            let remaining = time.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(TIMEOUT);
            }
            if let Some(status) = running.child.try_wait().map_err(|_| PROCESS)? {
                if !status.success() {
                    return Err(PROCESS);
                }
                return pipes.join().map_err(|_| PROCESS)?.map_err(|_| PROCESS);
            }
            thread::sleep(remaining.min(Duration::from_millis(10)));
        }
    })
}

fn exchange(
    mut stdin: impl io::Write,
    stdout: impl io::Read,
    packet: &[u8],
) -> io::Result<Vec<u8>> {
    stdin.write_all(packet)?;
    drop(stdin);
    let mut bytes = Vec::new();
    // Allow for the libtest header when running the test executable.
    let maximum = MAX_RESPONSE + if cfg!(test) { 32 } else { 0 };
    stdout.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(io::Error::other("PDF worker response exceeded its limit"));
    }
    Ok(bytes)
}

pub(crate) fn worker_entry() -> Option<ExitCode> {
    let mut args = env::args_os().skip(1);
    (args.next().is_some_and(|arg| arg == SWITCH) && args.next().is_none()).then(|| {
        if run_extractor().is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    })
}

fn run_extractor() -> io::Result<()> {
    // A parser panic (including panic=abort) is local to this child. Do not
    // display file or text data even when invoked with stderr attached.
    std::panic::set_hook(Box::new(|_| {}));
    if io::stdin().is_terminal() {
        return Err(io::Error::other("PDF worker requires piped input"));
    }
    let mut packet = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_REQUEST as u64 + 1)
        .read_to_end(&mut packet)?;
    if packet.len() > MAX_REQUEST || !Job::current_is_limited(MEMORY) {
        return Err(io::Error::other(
            "PDF worker requires a bounded job and request",
        ));
    }
    let (input, count, query): (_, u32, String) = serde_json::from_slice(&packet)?;
    if query.is_empty() || query.len() > MAX_QUERY || count > MAX_PAGES {
        return Err(io::Error::other("invalid PDF search request"));
    }
    let input = Input::from_packet(input).ok_or_else(|| io::Error::other("invalid PDF source"))?;
    let mut output = io::BufWriter::new(io::stdout().lock());
    match input.read().and_then(|bytes| find(&bytes, count, &query)) {
        Ok(hits) => {
            output.write_all(&[0])?;
            for hit in &*hits {
                output.write_all(&hit.page.to_le_bytes())?;
                output.write_all(&hit.from_top.to_le_bytes())?;
                output.write_all(&hit.aspect_height.to_le_bytes())?;
                for value in hit.bounds.map_or([0.; 4], |bounds| bounds.0) {
                    output.write_all(&value.to_le_bytes())?;
                }
            }
        }
        Err(failure) => {
            let code = match failure.reason {
                "size_limit" => 2,
                "source_changed" => 3,
                _ => 1,
            };
            output.write_all(&[code])?;
        }
    }
    output.flush()
}

fn decode(bytes: &[u8], count: u32) -> ResultHits {
    match bytes {
        [1] => Err(FAILED),
        [2] => Err(LIMIT),
        [3] => Err(CHANGED),
        [0, hits @ ..] if hits.len() <= MAX_HITS * HIT_BYTES && hits.len() % HIT_BYTES == 0 => {
            let mut result = Vec::with_capacity(hits.len() / HIT_BYTES);
            let mut last = (0, 0.0);
            for entry in hits.as_chunks::<HIT_BYTES>().0 {
                let page = u32::from_le_bytes(entry[..4].try_into().map_err(|_| PROCESS)?);
                let from_top = f64::from_le_bytes(entry[4..12].try_into().map_err(|_| PROCESS)?);
                let aspect_height =
                    f64::from_le_bytes(entry[12..20].try_into().map_err(|_| PROCESS)?);
                let bounds = decode_bounds(&entry[20..])?;
                if page >= count
                    || !(0.0..=1.0).contains(&from_top)
                    || !aspect_height.is_finite()
                    || aspect_height <= 0.0
                    || (page, from_top) < last
                {
                    return Err(PROCESS);
                }
                last = (page, from_top);
                result.push(Hit {
                    page,
                    from_top,
                    aspect_height,
                    bounds,
                });
            }
            Ok(result.into())
        }
        _ => Err(PROCESS),
    }
}

fn decode_bounds(bytes: &[u8]) -> Result<Option<Bounds>, Failure> {
    let mut edges = [0.; 4];
    for (edge, bytes) in edges.iter_mut().zip(bytes.chunks_exact(8)) {
        *edge = f64::from_le_bytes(bytes.try_into().map_err(|_| PROCESS)?);
    }
    let [left, top, right, bottom] = edges;
    if bytes == [0; 32] {
        Ok(None)
    } else if left >= 0. && top >= 0. && right <= 1. && bottom <= 1. && left < right && top < bottom
    {
        Ok(Some(Bounds(edges)))
    } else {
        Err(PROCESS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf_extract::{Document, Object, SaveOptions};
    use std::{fs, os::windows::fs::OpenOptionsExt as _, path::Path};

    fn input(path: &Path, data: &[u8]) -> io::Result<Input> {
        fs::write(path, data)?;
        Ok(Input::new(path.to_owned(), &fs::metadata(path)?))
    }

    #[derive(Clone, Copy, Debug)]
    enum StreamKind {
        Page,
        Font,
        Object,
    }

    fn compressed_pdf(kind: StreamKind) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        // A modest, generated PDF with benign filler. Force the limit by
        // lowering the job memory cap, not by generating huge real inputs.
        const FILLER: usize = 24 * 1024 * 1024;
        let fixture = crate::test_pdf::cid_text_page("65 [500 500]", "Identity-H", false);
        let mut doc = Document::load_mem(&fixture)?;
        let mut output = Vec::new();
        match kind {
            StreamKind::Page | StreamKind::Font => {
                let id = if matches!(kind, StreamKind::Page) {
                    4
                } else {
                    9
                };
                let stream = doc.get_object_mut((id, 0))?.as_stream_mut()?;
                let mut content = std::mem::take(&mut stream.content);
                content.resize(content.len() + FILLER, b' ');
                stream.set_content(content);
                stream.compress()?;
                doc.save_to(&mut output)?;
            }
            StreamKind::Object => {
                // Put benign filler in a required object so a partial decode
                // cannot silently omit just an unreferenced object.
                doc.get_dictionary_mut((3, 0))?
                    .set("Fixture", Object::string_literal(vec![b'X'; FILLER]));
                doc.save_with_options(
                    &mut output,
                    SaveOptions::builder()
                        .use_object_streams(true)
                        .use_xref_streams(true)
                        .compression_level(1)
                        .build(),
                )?;
            }
        }
        assert!(output.len() < 512 * 1024, "{kind:?} fixture not compressed");
        Ok(output)
    }

    #[test]
    fn memory_cap_covers_page_font_and_object_stream_expansion()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cancelled = AtomicBool::new(false);
        let small_cap = 16 * 1024 * 1024;
        let path = dir.path().join("small.pdf");
        let small = input(
            &path,
            &crate::test_pdf::cid_text_page("65 [500 500]", "Identity-H", false),
        )?;
        assert_eq!(
            search_with_limit(&small, 1, "ハイ", &cancelled, small_cap)
                .map_err(io::Error::from)?
                .len(),
            2
        );
        for kind in [StreamKind::Page, StreamKind::Font, StreamKind::Object] {
            let path = dir.path().join(format!("{kind:?}.pdf"));
            let large = input(&path, &compressed_pdf(kind)?)?;
            let normal = search(&large, 1, "ハイ", &cancelled);
            assert!(
                matches!(&normal, Ok(hits) if hits.len() == 2),
                "{kind:?}: {normal:?}"
            );
            let limited = search_with_limit(&large, 1, "ハイ", &cancelled, small_cap);
            assert!(
                matches!(limited, Err(failure) if failure.reason == PROCESS.reason
                || (matches!(kind, StreamKind::Object) && failure.reason == FAILED.reason)),
                "{kind:?}: {limited:?}"
            );
        }
        Ok(())
    }

    fn test_child(test: &str) -> io::Result<Command> {
        let mut command = Command::new(env::current_exe()?);
        command.args(["--ignored", "--exact", test]);
        Ok(command)
    }

    fn stalled_command() -> io::Result<Command> {
        test_child("pdf::text::worker::tests::stall_fixture")
    }

    fn wait_for_marker(marker: &Path) {
        let deadline = Instant::now() + Duration::from_secs(4);
        while !marker.is_file() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_marker_unlocked(path: &Path) -> io::Result<()> {
        // The fixture locks the file for the lifetime of the child.
        assert!(path.is_file(), "child never started");
        fs::OpenOptions::new().read(true).share_mode(0).open(path)?;
        Ok(())
    }

    #[test]
    fn kills_stalled_child_on_timeout_or_cancel_and_allows_next_search()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let marker = dir.path().join("timeout");
        let result = supervise(
            &mut stalled_command()?,
            &serde_json::to_vec(&marker)?,
            &AtomicBool::new(false),
            Duration::from_secs(3),
            MEMORY,
        );
        assert!(matches!(result, Err(failure) if failure.reason == TIMEOUT.reason));
        assert_marker_unlocked(&marker)?;

        let marker = dir.path().join("cancel");
        let packet = serde_json::to_vec(&marker)?;
        let cancelled = AtomicBool::new(false);
        let mut command = stalled_command()?;
        thread::scope(|scope| -> io::Result<()> {
            let work = scope.spawn(|| supervise(&mut command, &packet, &cancelled, TIME, MEMORY));
            wait_for_marker(&marker);
            cancelled.store(true, Ordering::Relaxed);
            let result = work
                .join()
                .map_err(|_| io::Error::other("supervisor panicked"))?;
            assert!(matches!(result, Err(failure) if failure.reason == FAILED.reason));
            assert_marker_unlocked(&marker)
        })?;
        let normal = input(
            &dir.path().join("after.pdf"),
            &crate::test_pdf::cid_text_page("65 66 500", "Identity-H", false),
        )?;
        assert_eq!(
            search(&normal, 1, "ハイ", &AtomicBool::new(false))
                .map_err(io::Error::from)?
                .len(),
            2
        );
        Ok(())
    }

    #[test]
    fn viewer_process_exit_closes_job_even_without_rust_cleanup() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let marker = dir.path().join("exit");
        let mut child = test_child("pdf::text::worker::tests::exiting_parent_fixture")?
            .arg("--nocapture")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let completed = (|| -> io::Result<bool> {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("missing fixture stdin"))?;
            serde_json::to_writer(&mut stdin, &marker)?;
            drop(stdin);
            let deadline = Instant::now() + Duration::from_secs(6);
            while Instant::now() < deadline {
                if let Some(status) = child.try_wait()? {
                    return Ok(status.success());
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(io::Error::other("parent fixture did not exit"))
        })();
        if completed.is_err() {
            drop(child.kill());
            drop(child.wait());
        }
        assert!(completed?);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if assert_marker_unlocked(&marker).is_ok() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_marker_unlocked(&marker)
    }

    #[test]
    #[ignore = "exit the fixture while its supervised child is blocked"]
    fn exiting_parent_fixture() -> io::Result<()> {
        let marker: std::path::PathBuf = serde_json::from_reader(io::stdin().lock())?;
        let packet = serde_json::to_vec(&marker)?;
        let mut command = stalled_command()?;
        let worker = thread::spawn(move || {
            supervise(&mut command, &packet, &AtomicBool::new(false), TIME, MEMORY)
        });
        wait_for_marker(&marker);
        if marker.is_file() {
            std::process::exit(0); // Deliberately skip Rust destructors.
        }
        drop(worker);
        Err(io::Error::other("blocked fixture did not start"))
    }

    #[test]
    #[ignore = "generated blocked child for supervision tests"]
    fn stall_fixture() -> io::Result<()> {
        let marker: std::path::PathBuf = serde_json::from_reader(io::stdin().lock())?;
        let _locked = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .share_mode(0)
            .open(marker)?;
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }

    #[test]
    #[ignore = "invoked as the extraction child when the viewer is a libtest binary"]
    fn extractor_fixture() {
        std::process::exit(i32::from(super::run_extractor().is_err()));
    }
}
