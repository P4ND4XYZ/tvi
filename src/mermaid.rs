//! Cancellable Mermaid conversion. The caller owns scheduling and rendering.

#[path = "mermaid/cache.rs"]
mod cache;
#[path = "mermaid/process.rs"]
mod process;

use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const INPUT: &str = "input.mmd";
const OUTPUT: &str = "output.png";
const MAX_PNG_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub per_diagram: Duration,
    pub per_document: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_diagram: Duration::from_secs(10),
            per_document: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeoutScope {
    Diagram,
    Document,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Discovery,
    Prepare,
    Launch,
    Execute,
    Output,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    Unavailable,
    Prepare(io::ErrorKind),
    Launch {
        kind: io::ErrorKind,
        os_code: Option<i32>,
    },
    Wait(io::ErrorKind),
    Exit {
        code: Option<i32>,
    },
    Timeout(TimeoutScope),
    Cancelled,
    MissingOutput,
    ReadOutput(io::ErrorKind),
    InvalidPng,
}

impl Failure {
    pub fn stage(self) -> Stage {
        match self {
            Self::Unavailable => Stage::Discovery,
            Self::Prepare(_) => Stage::Prepare,
            Self::Launch { .. } => Stage::Launch,
            Self::Wait(_) | Self::Exit { .. } | Self::Timeout(_) | Self::Cancelled => {
                Stage::Execute
            }
            Self::MissingOutput | Self::ReadOutput(_) | Self::InvalidPng => Stage::Output,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheIssue {
    InvalidEntry,
    Read(io::ErrorKind),
    Busy,
    Write(io::ErrorKind),
    Capacity,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheReport {
    pub hit: bool,
    pub saved: bool,
    pub lookup_issue: Option<CacheIssue>,
    pub save_issue: Option<CacheIssue>,
}

#[derive(Debug)]
pub struct Conversion<T> {
    pub result: Result<T, Failure>,
    pub elapsed: Duration,
    pub cache: CacheReport,
}

#[derive(Clone)]
pub struct Mermaid {
    path: Option<OsString>,
    cache: Option<cache::Cache>,
    temp_root: PathBuf,
    limits: Limits,
}

impl Mermaid {
    pub fn from_env() -> Self {
        Self {
            path: env::var_os("PATH"),
            cache: cache::from_env(),
            temp_root: env::temp_dir(),
            limits: Limits::default(),
        }
    }

    #[cfg(test)]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn start_document(&self) -> Document<'_> {
        Document {
            options: self,
            executable: process::find(self.path.as_deref()),
            started: Instant::now(),
            cancelled: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn isolated(
        path: OsString,
        temp_root: PathBuf,
        cache: PathBuf,
        capacity: u64,
    ) -> Self {
        Self {
            path: Some(path),
            cache: Some(cache::Cache {
                root: cache,
                capacity,
            }),
            temp_root,
            limits: Limits::default(),
        }
    }
}

pub struct Document<'a> {
    options: &'a Mermaid,
    executable: Option<PathBuf>,
    started: Instant,
    cancelled: Option<&'a AtomicBool>,
}

struct Deadline<'a> {
    started: Instant,
    budget: Duration,
    scope: TimeoutScope,
    cancelled: Option<&'a AtomicBool>,
}

impl Deadline<'_> {
    fn is_cancelled(&self) -> bool {
        self.cancelled
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    fn check(&self) -> Result<(), Failure> {
        if self.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        if self.started.elapsed() >= self.budget {
            Err(Failure::Timeout(self.scope))
        } else {
            Ok(())
        }
    }
}

#[allow(
    dead_code,
    reason = "the core is also compiled standalone for integration tests"
)]
impl<'a> Document<'a> {
    pub(crate) fn with_cancellation(mut self, cancelled: &'a AtomicBool) -> Self {
        self.cancelled = Some(cancelled);
        self
    }

    pub(crate) fn available(&self) -> bool {
        self.executable.is_some()
    }

    /// Read-only cache lookup; never launches the renderer or modifies the cache.
    pub(crate) fn cached_png(&self, source: &str) -> Result<Option<Vec<u8>>, CacheIssue> {
        if !self.available() {
            return Ok(None);
        }
        self.options.cache.as_ref().map_or(Ok(None), |store| {
            store.lookup(&cache::key(source.as_bytes()))
        })
    }

    pub(crate) fn timeout_limit(&self, scope: TimeoutScope) -> Duration {
        match scope {
            TimeoutScope::Diagram => self.options.limits.per_diagram,
            TimeoutScope::Document => self.options.limits.per_document,
        }
    }

    /// Decode/validate the exact PNG bytes eligible for caching, returning an
    /// owned value. Synchronous filesystem and decode operations cannot be
    /// interrupted; their results are checked against the deadline afterward.
    pub fn convert<T>(
        &mut self,
        source: &str,
        mut decode: impl FnMut(&[u8]) -> Option<T>,
    ) -> Conversion<T> {
        let started = Instant::now();
        let mut cache = CacheReport::default();

        let document_remaining = self
            .options
            .limits
            .per_document
            .saturating_sub(started.duration_since(self.started));
        let (budget, scope) = if document_remaining <= self.options.limits.per_diagram {
            (document_remaining, TimeoutScope::Document)
        } else {
            (self.options.limits.per_diagram, TimeoutScope::Diagram)
        };
        let deadline = Deadline {
            started,
            budget,
            scope,
            cancelled: self.cancelled,
        };
        let result = deadline
            .check()
            .and_then(|()| self.convert_inner(source, &mut decode, &deadline, &mut cache));
        if matches!(result, Err(Failure::Launch { .. })) {
            self.executable = None;
        }
        // This check also covers dropping the private workspace.
        let result = deadline.check().and(result);
        Conversion {
            result,
            elapsed: started.elapsed(),
            cache,
        }
    }

    fn convert_inner<T>(
        &self,
        source: &str,
        decode: &mut impl FnMut(&[u8]) -> Option<T>,
        deadline: &Deadline<'_>,
        report: &mut CacheReport,
    ) -> Result<T, Failure> {
        let executable = self.executable.as_deref().ok_or(Failure::Unavailable)?;
        let store = self.options.cache.as_ref();
        let key = store.map(|_| cache::key(source.as_bytes()));
        deadline.check()?;
        if let Some((store, key)) = store.zip(key.as_ref()) {
            let cached = store.lookup(key);
            deadline.check()?;
            match cached {
                Ok(Some(png)) => {
                    let image = decode(&png);
                    deadline.check()?;
                    if let Some(image) = image {
                        report.hit = true;
                        return Ok(image);
                    }
                    report.lookup_issue = Some(CacheIssue::InvalidEntry);
                }
                Ok(None) => {}
                Err(issue) => report.lookup_issue = Some(issue),
            }
        }

        // Never resolve a relative temp directory against the document or cwd.
        if !self.options.temp_root.is_absolute() {
            return Err(Failure::Prepare(io::ErrorKind::InvalidInput));
        }
        let work = tempfile::Builder::new()
            .prefix("tvi-mermaid-")
            .tempdir_in(&self.options.temp_root)
            .map_err(|error| Failure::Prepare(error.kind()))?;
        deadline.check()?;
        fs::write(work.path().join(INPUT), source)
            .map_err(|error| Failure::Prepare(error.kind()))?;
        process::run(
            executable,
            self.options
                .path
                .as_deref()
                .unwrap_or_else(|| OsStr::new("")),
            work.path(),
            deadline,
        )?;

        deadline.check()?;
        let png = read_png(&work.path().join(OUTPUT))?;
        deadline.check()?;
        let image = decode(&png);
        deadline.check()?;
        let image = image.ok_or(Failure::InvalidPng)?;
        if let Some((store, key)) = store.zip(key.as_ref()) {
            match store.save(key, &png) {
                Ok(()) => report.saved = true,
                Err(issue) => report.save_issue = Some(issue),
            }
        }
        Ok(image)
    }
}

fn read_png(path: &Path) -> Result<Vec<u8>, Failure> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            Failure::MissingOutput
        } else {
            Failure::ReadOutput(error.kind())
        }
    })?;
    if !metadata.is_file() || metadata.len() > MAX_PNG_BYTES {
        return Err(Failure::InvalidPng);
    }

    let mut png = Vec::new();
    File::open(path)
        .map_err(|error| Failure::ReadOutput(error.kind()))?
        .take(MAX_PNG_BYTES + 1)
        .read_to_end(&mut png)
        .map_err(|error| Failure::ReadOutput(error.kind()))?;
    if png.len() as u64 > MAX_PNG_BYTES || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(Failure::InvalidPng);
    }
    Ok(png)
}
