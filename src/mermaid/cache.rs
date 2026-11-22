use std::{
    env,
    fs::{self, OpenOptions, TryLockError},
    io::{self, Write},
    path::PathBuf,
    time::SystemTime,
};

use sha2::{Digest, Sha256};

use super::{CacheIssue, Failure, process::RENDER_ARGS, read_png};

const CAPACITY: u64 = 128 * 1024 * 1024;
const STAGING_PREFIX: &str = ".staging-";

pub(super) struct Cache {
    pub(super) root: PathBuf,
    pub(super) capacity: u64,
}

pub(super) fn from_env() -> Option<Cache> {
    let absolute = |name| {
        env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    #[cfg(windows)]
    let base = absolute("LOCALAPPDATA");
    #[cfg(not(windows))]
    let base = absolute("XDG_CACHE_HOME").or_else(|| absolute("HOME").map(|p| p.join(".cache")));

    Some(Cache {
        root: base?.join("tvi").join("cache").join("mermaid"),
        capacity: CAPACITY,
    })
}

pub(super) fn key(source: &[u8]) -> String {
    let mut hash = Sha256::new();
    // Include the exact arguments so changes to the render command cannot
    // reuse PNGs from an earlier recipe. Bump v1 if the recipe format changes.
    hash.update(b"tvi-mermaid/png/v1\0");
    for arg in RENDER_ARGS {
        hash.update(arg.as_bytes());
        hash.update(b"\0");
    }
    hash.update(source);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = String::with_capacity(68);
    for byte in hash.finalize() {
        name.push(char::from(HEX[usize::from(byte >> 4)]));
        name.push(char::from(HEX[usize::from(byte & 15)]));
    }
    name.push_str(".png");
    name
}

impl Cache {
    pub(super) fn lookup(&self, key: &str) -> Result<Option<Vec<u8>>, CacheIssue> {
        match read_png(&self.root.join(key)) {
            Ok(png) => Ok(Some(png)),
            Err(Failure::MissingOutput) => Ok(None),
            Err(Failure::ReadOutput(kind)) => Err(CacheIssue::Read(kind)),
            Err(_) => Err(CacheIssue::InvalidEntry),
        }
    }

    pub(super) fn save(&self, key: &str, png: &[u8]) -> Result<(), CacheIssue> {
        let size = png.len() as u64;
        if size > self.capacity {
            return Err(CacheIssue::Capacity);
        }
        fs::create_dir_all(&self.root).map_err(write_issue)?;
        // Keep the lock file in place so all writers always lock the same inode.
        // OS file locks release on process exit. Contention skips this write.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join(".lock"))
            .map_err(write_issue)?;
        lock.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => CacheIssue::Busy,
            TryLockError::Error(error) => write_issue(error),
        })?;

        self.make_space(key, size)?;
        let mut pending = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempfile_in(&self.root)
            .map_err(write_issue)?;
        pending.write_all(png).map_err(write_issue)?;
        pending
            .persist(self.root.join(key))
            .map_err(|error| write_issue(error.error))?;
        Ok(())
    }

    fn make_space(&self, key: &str, incoming: u64) -> Result<(), CacheIssue> {
        let mut entries = Vec::new();
        let mut total = 0_u64;
        for entry in fs::read_dir(&self.root).map_err(write_issue)? {
            let entry = entry.map_err(write_issue)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !entry.file_type().map_err(write_issue)?.is_file() {
                continue;
            }
            if name.starts_with(STAGING_PREFIX) {
                // A live writer holds the same lock, so these are orphan files.
                fs::remove_file(entry.path()).map_err(write_issue)?;
            } else if name != key && is_cache_key(name) {
                // This key, if present, is replaced atomically rather than added.
                let metadata = entry.metadata().map_err(write_issue)?;
                let generated = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                total = total.saturating_add(metadata.len());
                entries.push((generated, entry.path(), metadata.len()));
            }
        }
        let remaining = self.capacity - incoming;
        if total > remaining {
            entries.sort_unstable();
            for (_, path, size) in entries {
                fs::remove_file(path).map_err(write_issue)?;
                total -= size;
                if total <= remaining {
                    break;
                }
            }
        }
        if total > remaining {
            return Err(CacheIssue::Capacity);
        }
        Ok(())
    }
}

fn is_cache_key(name: &str) -> bool {
    name.strip_suffix(".png")
        .is_some_and(|stem| stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn write_issue(error: io::Error) -> CacheIssue {
    CacheIssue::Write(error.kind())
}
