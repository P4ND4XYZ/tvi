use std::{
    fs::{self, File, Metadata, OpenOptions, TryLockError},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use super::MAX_RECORD_BYTES;

const MAX_FILE_BYTES: u64 = 1024 * 1024;
const HISTORY: u8 = 5;

pub(super) fn append(
    directory: &Path,
    record: impl FnOnce() -> io::Result<(String, Vec<u8>)>,
) -> io::Result<()> {
    append_with_first_write(directory, record, File::write_all)
}

fn append_with_first_write(
    directory: &Path,
    record: impl FnOnce() -> io::Result<(String, Vec<u8>)>,
    write_first: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    for path in directory.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() && !is_link(&metadata) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(path).or_else(|e| {
                    if e.kind() == io::ErrorKind::AlreadyExists
                        && fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !is_link(&m))
                    {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })?;
            }
            Ok(_) => return Err(invalid()),
            Err(error) => return Err(error),
        }
    }
    let lock_path = directory.join("tvi.log.lock");
    let lock = open(&lock_path, true)?;
    if lock.metadata()?.len() != 0 {
        return Err(invalid());
    }
    lock.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
        TryLockError::Error(error) => error,
    })?;

    let (date, line) = record()?;
    let path = directory.join("tvi.log");
    let pending = directory.join("tvi.log.pending");
    if existing(&pending)?.is_some() {
        if existing(&path)?.is_some() {
            return Err(invalid());
        }
        finish_rotation(directory, &pending)?;
    }
    if existing(&path)?.is_none() {
        return publish_first(directory, &path, &line, write_first);
    }
    let mut active = open(&path, false)?;
    let (size, first_date) = validate(&mut active)?;
    if size == 0 {
        return Err(invalid());
    }
    if size + line.len() as u64 > MAX_FILE_BYTES || first_date.as_deref() != Some(&date) {
        drop(active);
        fs::rename(&path, &pending)?;
        finish_rotation(directory, &pending)?;
        return publish_first(directory, &path, &line, write_first);
    }
    active.seek(SeekFrom::Start(size))?;
    if let Err(error) = active.write_all(&line) {
        // Roll back only our own failed append, never a pre-existing malformed tail.
        drop(active.set_len(size));
        return Err(error);
    }
    Ok(())
}

fn publish_first(
    directory: &Path,
    active: &Path,
    line: &[u8],
    write_first: impl FnOnce(&mut File, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    // A failed first write (or termination before publish) must not leave an empty active file.
    let mut temp = tempfile::Builder::new()
        .prefix(".tvi-log-")
        .tempfile_in(directory)?;
    write_first(temp.as_file_mut(), line)?;
    temp.persist_noclobber(active)
        .map_err(|error| error.error)?;
    Ok(())
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "unexpected log data or file type",
    )
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        } // reparse point
    }
    metadata.is_symlink()
}

fn existing(path: &Path) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !is_link(&metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    return Err(invalid());
                }
            }
            Ok(Some(metadata))
        }
        Ok(_) => Err(invalid()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn open(path: &Path, allow_create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let create = existing(path)?.is_none() && allow_create;
    if create {
        options.create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
    }
    let file = match options.open(path) {
        Err(e)
            if create && e.kind() == io::ErrorKind::AlreadyExists && existing(path)?.is_some() =>
        {
            options.create_new(false).open(path)
        }
        result => result,
    }?;
    check_handle(&file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current = existing(path)?.ok_or_else(invalid)?;
        let opened = file.metadata()?;
        if current.ino() != opened.ino() || current.dev() != opened.dev() || opened.nlink() != 1 {
            return Err(invalid());
        }
    }
    Ok(file)
}

fn check_handle(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || is_link(&metadata) {
        return Err(invalid());
    }
    #[cfg(windows)]
    windows_single_link(file)?;
    Ok(())
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "stable std does not expose Windows hard-link counts"
)]
fn windows_single_link(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct FileInformation {
        attributes: u32,
        times: [u32; 6],
        volume: u32,
        size: [u32; 2],
        links: u32,
        index: [u32; 2],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            handle: std::os::windows::io::RawHandle,
            info: *mut FileInformation,
        ) -> i32;
    }
    let mut info = std::mem::MaybeUninit::<FileInformation>::uninit();
    // SAFETY: file owns a live handle, and info has the layout of BY_HANDLE_FILE_INFORMATION.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call initialized the complete structure.
    if unsafe { info.assume_init() }.links != 1 {
        return Err(invalid());
    }
    Ok(())
}

fn validate_path(path: &Path) -> io::Result<u64> {
    let mut file = File::open(path)?;
    check_handle(&file)?;
    Ok(validate(&mut file)?.0)
}

fn validate(file: &mut File) -> io::Result<(u64, Option<String>)> {
    let size = file.metadata()?.len();
    if size > MAX_FILE_BYTES {
        return Err(invalid());
    }
    let mut reader = BufReader::new(file.take(size));
    let mut line = Vec::new();
    let mut date = None;
    while reader
        .by_ref()
        .take(MAX_RECORD_BYTES as u64)
        .read_until(b'\n', &mut line)?
        != 0
    {
        if line.last() != Some(&b'\n') {
            return Err(invalid());
        }
        let event: serde_json::Value = serde_json::from_slice(&line)?;
        let ts = event["ts"].as_str().ok_or_else(invalid)?;
        let day = ts.get(..10).ok_or_else(invalid)?;
        if ts.len() != 24
            || !ts.ends_with('Z')
            || !ts.bytes().enumerate().all(|(i, b)| match i {
                4 | 7 => b == b'-',
                10 => b == b'T',
                13 | 16 => b == b':',
                19 => b == b'.',
                23 => b == b'Z',
                _ => b.is_ascii_digit(),
            })
            || !matches!(event["level"].as_str(), Some("DEBUG" | "WARN" | "ERROR"))
            || event["pid"].as_u64().is_none()
            || event["event"].as_str().is_none_or(str::is_empty)
            || date.as_deref().is_some_and(|date| date != day)
        {
            return Err(invalid());
        }
        date.get_or_insert_with(|| day.to_owned());
        line.clear();
    }
    Ok((size, date))
}

fn archive(directory: &Path, index: u8) -> PathBuf {
    directory.join(format!("tvi.log.{index}"))
}

fn finish_rotation(directory: &Path, pending: &Path) -> io::Result<()> {
    // Preflight every existing name before modifying history, including the staged active file.
    if validate_path(pending)? == 0 {
        return Err(invalid());
    }
    let mut first_hole = None;
    for index in 1..=HISTORY {
        let path = archive(directory, index);
        if existing(&path)?.is_some() {
            if validate_path(&path)? == 0 {
                return Err(invalid());
            }
        } else {
            first_hole.get_or_insert(index);
        }
    }
    // Once the oldest entry is removed, there is a hole until the pending file is installed.
    // Each successful rename moves that hole towards slot 1, so retry cannot delete again.
    let hole = if let Some(hole) = first_hole {
        hole
    } else {
        fs::remove_file(archive(directory, HISTORY))?;
        HISTORY
    };
    for index in (1..hole).rev() {
        fs::rename(archive(directory, index), archive(directory, index + 1))?;
    }
    fs::rename(pending, archive(directory, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &[u8] =
        b"{\"ts\":\"2026-11-20T00:00:00.000Z\",\"level\":\"WARN\",\"event\":\"first\",\"pid\":1}\n";
    const SECOND: &[u8] = b"{\"ts\":\"2026-11-21T00:00:00.000Z\",\"level\":\"WARN\",\"event\":\"second\",\"pid\":1}\n";

    fn record(date: &str, line: &[u8]) -> io::Result<(String, Vec<u8>)> {
        Ok((date.to_owned(), line.to_vec()))
    }

    fn fail_after_partial_write(file: &mut File, line: &[u8]) -> io::Result<()> {
        file.write_all(&line[..8])?;
        Err(io::Error::other("injected partial write failure"))
    }

    #[test]
    fn failed_first_write_leaves_no_active_log_and_retry_succeeds() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path();
        let active = path.join("tvi.log");
        let first = || record("2026-11-20", FIRST);
        assert!(append_with_first_write(path, first, fail_after_partial_write).is_err());
        assert!(!active.exists());
        // A failed write should also clean up its staging file when the process survives.
        assert_eq!(fs::read_dir(path)?.count(), 1); // only the lock
        append(path, first)?;
        assert_eq!(fs::read(active)?, FIRST);
        Ok(())
    }

    #[test]
    fn failed_first_write_after_rotation_does_not_rotate_twice() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path();
        append(path, || record("2026-11-20", FIRST))?;
        let second = || record("2026-11-21", SECOND);
        assert!(append_with_first_write(path, second, fail_after_partial_write).is_err());
        assert!(!path.join("tvi.log").exists());
        assert_eq!(fs::read(path.join("tvi.log.1"))?, FIRST);
        append(path, second)?;
        assert_eq!(fs::read(path.join("tvi.log"))?, SECOND);
        assert_eq!(fs::read(path.join("tvi.log.1"))?, FIRST);
        assert!(!path.join("tvi.log.2").exists());
        Ok(())
    }

    #[test]
    fn publishing_first_record_never_overwrites_an_existing_name() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path();
        let active = path.join("tvi.log");
        let result = append_with_first_write(
            path,
            || record("2026-11-20", FIRST),
            |file, line| {
                file.write_all(line)?;
                // Another writer creates the destination immediately before publication.
                fs::write(&active, b"do not replace")
            },
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(active)?, b"do not replace");
        Ok(())
    }
}
