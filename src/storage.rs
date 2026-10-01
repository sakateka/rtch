use crate::{Result, fail, history::Filter, logfile, os};
use std::os::unix::{
    fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    net::{UnixListener, UnixStream},
};
use std::{
    collections::VecDeque,
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

pub fn directory() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    let dir = crate::config::load()?
        .session_dir
        .unwrap_or_else(|| home.join(".cache/rtch"));
    if !dir.is_absolute() {
        return fail("invalid session_dir: expected an absolute path");
    }
    Ok(dir)
}
pub fn resolve(path: &Path) -> Result<PathBuf> {
    let path = if path.as_os_str().as_encoded_bytes().contains(&b'/') {
        env::current_dir()?.join(path)
    } else {
        directory()?.join(path)
    };
    let name = path.file_name().ok_or("invalid session name")?;
    if name == "."
        || name == ".."
        || name.to_string_lossy().contains(':')
        || name.as_encoded_bytes().iter().any(u8::is_ascii_control)
        || name.to_string_lossy().ends_with(".log")
        || name.to_string_lossy().ends_with(".ended")
        || name.to_string_lossy().ends_with(".head") && !legacy_head_session(&path)?
    {
        return fail("invalid session name");
    }
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    if let Some(parent) = path.parent()
        && let Ok(parent) = parent.canonicalize()
    {
        return Ok(parent.join(name));
    }
    Ok(path)
}
pub fn side(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    p.into()
}
fn history_artifact(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() || metadata.file_type().is_symlink()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
fn retained_files(path: &Path) -> io::Result<bool> {
    for suffix in [".log", ".ended", ".head"] {
        let candidate = side(path, suffix);
        if history_artifact(&candidate)? && (suffix != ".head" || !legacy_head_session(&candidate)?)
        {
            return Ok(true);
        }
    }
    Ok(false)
}
fn legacy_head_session(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => return Ok(true),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    for suffix in [".log", ".ended", ".head"] {
        if history_artifact(&side(path, suffix))? {
            return Ok(true);
        }
    }
    Ok(false)
}
fn prefix_path(path: &Path) -> io::Result<PathBuf> {
    let head = side(path, ".head");
    if legacy_head_session(&head)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "prefix path conflicts with an existing .head session",
        ));
    }
    Ok(head)
}
pub fn ensure_parent(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or("session has no parent directory")?;
    if !parent.exists() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let m = fs::symlink_metadata(parent)?;
    if !m.is_dir() || m.uid() != os::uid() || m.mode() & 0o022 != 0 {
        return fail("session directory must be owned by you and not writable by group/others");
    }
    Ok(())
}
fn parent_directory(path: &Path) -> Result<File> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path.parent().ok_or("session has no parent directory")?)?;
    let metadata = directory.metadata()?;
    if metadata.uid() != os::uid() || metadata.mode() & 0o022 != 0 {
        return fail("session directory must be owned by you and not writable by group/others");
    }
    Ok(directory)
}
pub fn lock_parent(path: &Path) -> Result<File> {
    let directory = parent_directory(path)?;
    os::lock(directory.as_raw_fd())?;
    Ok(directory)
}
pub fn try_lock_parent(path: &Path) -> Result<Option<File>> {
    let directory = parent_directory(path)?;
    Ok(os::try_lock(directory.as_raw_fd())?.then_some(directory))
}
fn socket_path<T>(path: &Path, f: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
    if path.as_os_str().as_encoded_bytes().len() < 100 {
        return f(path);
    }
    let saved = env::current_dir()?;
    env::set_current_dir(
        path.parent()
            .ok_or_else(|| io::Error::other("invalid socket path"))?,
    )?;
    let result = f(Path::new(
        path.file_name()
            .ok_or_else(|| io::Error::other("invalid socket name"))?,
    ));
    let restored = env::set_current_dir(saved);
    restored?;
    result
}
pub fn connect(path: &Path) -> io::Result<UnixStream> {
    let s = socket_path(path, |p| UnixStream::connect(p))?;
    os::validate_peer(&s)?;
    Ok(s)
}
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    socket_path(path, |p| UnixListener::bind(p))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Attached,
    Ended,
    Stale,
    Missing,
}
pub fn state(path: &Path) -> Result<State> {
    match connect(path) {
        Ok(_) => {
            let m = fs::metadata(path)?;
            Ok(if m.mode() & 0o100 != 0 {
                State::Attached
            } else {
                State::Running
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(if retained_files(path)? {
            State::Ended
        } else {
            State::Missing
        }),
        Err(e) if e.raw_os_error() == Some(libc::ECONNREFUSED) => {
            let m = fs::symlink_metadata(path)?;
            if !m.file_type().is_socket() {
                return fail("session path is not a socket");
            }
            Ok(State::Stale)
        }
        Err(e) => Err(e.into()),
    }
}
pub fn open_file(path: &Path, write: bool) -> io::Result<File> {
    file_options(path, write, write)
}
fn file_options(path: &Path, write: bool, create: bool) -> io::Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(write)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let m = f.metadata()?;
    if !m.is_file() || m.uid() != os::uid() || m.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session file must be regular, owned by you, and private (0600)",
        ));
    }
    Ok(f)
}
pub struct Log {
    path: PathBuf,
    file: Option<logfile::Container>,
    head: Option<logfile::Container>,
    head_written: usize,
    cap: usize,
    pub history: VecDeque<u8>,
    filter: Filter,
    codec: logfile::Codec,
}
impl Log {
    // Keep validation and ordered publication together: neither file may mutate early.
    #[allow(clippy::too_many_lines)]
    pub fn open(path: &Path, cap: usize) -> Result<Self> {
        if cap != 0 && !(256..=256 * 1024 * 1024).contains(&cap) {
            return fail("invalid compressed log size (minimum 256 bytes, maximum 256m)");
        }
        let mut result = Self {
            path: path.to_owned(),
            file: None,
            head: None,
            head_written: 0,
            cap,
            history: VecDeque::new(),
            filter: Filter::default(),
            codec: logfile::Codec::new()?,
        };
        if cap == 0 {
            logfile::cleanup_session(path)?;
            return Ok(result);
        }
        let head_path = prefix_path(path)?;
        let log_path = side(path, ".log");
        // Validate both existing files before creating, truncating or replacing either.
        let existing = |path: &Path| match file_options(path, true, false) {
            Ok(file) => Ok(Some(file)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        };
        let mut log_file = existing(&log_path)?;
        let mut head_file = existing(&head_path)?;
        let log_header = log_file
            .as_mut()
            .map(logfile::header)
            .transpose()?
            .flatten();
        let head_header = head_file
            .as_mut()
            .map(logfile::header)
            .transpose()?
            .flatten();
        let log_valid = if log_header.is_some() {
            Some(logfile::validate(
                log_file.as_mut().expect("log"),
                &mut result.codec,
            )?)
        } else {
            None
        };
        let head_valid = if head_header.is_some() {
            Some(logfile::validate(
                head_file.as_mut().expect("head"),
                &mut result.codec,
            )?)
        } else {
            None
        };
        let mut prefix = if let Some(head) = &mut head_file {
            let prefix = logfile::read(head, PREVIEW_LIMIT, true)?.bytes;
            if prefix.is_empty() && head_header.is_none() {
                if let Some(log) = &mut log_file {
                    logfile::read(log, PREVIEW_LIMIT, true)?.bytes
                } else {
                    prefix
                }
            } else {
                prefix
            }
        } else if let Some(log) = &mut log_file {
            logfile::read(log, PREVIEW_LIMIT, true)?.bytes
        } else {
            Vec::new()
        };
        // Resume a legacy prefix only when the complete retained history is
        // byte-for-byte equal to it. Length alone cannot prove continuity.
        let legacy_gap = if head_header.is_none() && !prefix.is_empty() {
            if let Some(file) = &mut log_file {
                let window = logfile::read(file, PREVIEW_LIMIT + 1, true)?;
                window.cut || window.bytes != prefix
            } else {
                true
            }
        } else {
            false
        };
        logfile::cleanup_session(path)?;
        let generation = if log_valid.is_some_and(|(_, sequence, _)| sequence == 0) {
            // An interrupted compaction may leave no sequence anchor. Start a new
            // generation on reopen so followers cannot confuse reused numbers.
            logfile::generation()?
        } else {
            log_header.map_or_else(logfile::generation, |h| Ok(h.generation))?
        };
        let incomplete_head = head_valid.is_some_and(|(end, _, _)| {
            head_file
                .as_ref()
                .is_some_and(|f| f.metadata().is_ok_and(|m| m.len() > end))
        });
        let mut sealed = incomplete_head
            || head_header.is_some_and(|h| h.sealed)
            || prefix.len() == PREVIEW_LIMIT
            || legacy_gap;
        let preserve_head = head_header.is_some()
            && head_valid.is_some_and(|(end, _, decoded)| {
                end <= (cap / 2) as u64 && decoded <= PREVIEW_LIMIT
            });
        let (head_encoded, head_size) = if preserve_head {
            (
                Vec::new(),
                usize::try_from(head_valid.expect("validated").0).expect("bounded prefix"),
            )
        } else {
            let (encoded, n) =
                result
                    .codec
                    .fitting(&prefix, 1, cap / 2 - logfile::HEADER_BYTES, true)?;
            sealed |= n < prefix.len();
            prefix.truncate(n);
            let size = logfile::HEADER_BYTES + encoded.len();
            (encoded, size)
        };
        let allowance = cap - head_size;
        let legacy_end = log_file
            .as_ref()
            .map_or(Ok(0), |file| file.metadata().map(|m| m.len()))?;
        let mut file = if let Some(mut header) = log_header {
            let (end, sequence, _) = log_valid.expect("validated");
            let mut file = log_file.take().expect("log");
            file.set_len(end)?; // Discard only an uncommitted final append.
            header.generation = generation;
            header.write(&mut file)?;
            file.seek(SeekFrom::Start(end))?;
            let mut container = logfile::Container {
                file,
                path: log_path.clone(),
                header,
                end,
                sequence,
            };
            container.compact(allowance, 0)?;
            container
        } else {
            logfile::migrate(
                &log_path,
                log_file.as_mut(),
                allowance,
                logfile::Header {
                    generation,
                    sealed: false,
                    cut: false,
                    legacy_end,
                },
                &mut result.codec,
            )?
        };
        let head = if preserve_head {
            let (end, sequence, _) = head_valid.expect("validated");
            let mut head = head_file.take().expect("head");
            head.set_len(end)?;
            let mut header = head_header.expect("header");
            header.sealed = sealed;
            header.write(&mut head)?;
            logfile::Container {
                file: head,
                path: head_path,
                header,
                end,
                sequence,
            }
        } else {
            let header = logfile::Header {
                generation,
                sealed,
                cut: false,
                legacy_end: 0,
            };
            let head = logfile::replace(&head_path, |file| {
                header.write(file)?;
                file.write_all(&head_encoded)
            })?;
            logfile::Container {
                file: head,
                path: head_path,
                header,
                end: head_size as u64,
                sequence: u64::from(!head_encoded.is_empty()),
            }
        };
        result.head_written = prefix.len();
        let window = logfile::read(&mut file.file, result.replay_limit(), false)?;
        let start = crate::history::emulator_boundary(&window.bytes, window.cut);
        result
            .filter
            .feed_into(&window.bytes[start..], &mut result.history);
        result.file = Some(file);
        result.head = Some(head);
        Ok(result)
    }
    fn replay_limit(&self) -> usize {
        if self.cap == 0 { 128 * 1024 } else { self.cap }
    }
    pub fn append(&mut self, bytes: &[u8]) -> Result<()> {
        for bytes in bytes.chunks(logfile::FRAME_LIMIT) {
            self.filter.feed_into(bytes, &mut self.history);
            let keep = self.replay_limit();
            if self.history.len() > keep {
                self.history.drain(..self.history.len() - keep);
            }
            if let (Some(file), Some(head)) = (&mut self.file, &mut self.head) {
                let sequence = logfile::next_sequence(file.sequence)?;
                let mut prefix = Vec::new();
                let mut n = 0;
                if !head.header.sealed {
                    let offered = bytes.len().min(PREVIEW_LIMIT - self.head_written);
                    (prefix, n) = self.codec.fitting(
                        &bytes[..offered],
                        logfile::next_sequence(head.sequence)?,
                        self.cap / 2 - usize::try_from(head.end).expect("bounded prefix"),
                        true,
                    )?;
                    if n < offered || self.head_written + n == PREVIEW_LIMIT {
                        head.header.sealed = true;
                    }
                }
                let allowance =
                    self.cap - usize::try_from(head.end).expect("bounded prefix") - prefix.len();
                let (mut encoded, kept) = self.codec.fitting(
                    bytes,
                    sequence,
                    allowance - logfile::HEADER_BYTES,
                    false,
                )?;
                if kept < bytes.len() && !encoded.is_empty() {
                    // A fitted tail starts after a raw gap. Leave a sequence gap
                    // so existing followers reset even when cut was already set.
                    let sequence = logfile::next_sequence(sequence)?;
                    let end = encoded.len();
                    encoded[end - 8..].copy_from_slice(&sequence.to_le_bytes());
                }
                // Reserve head growth and the complete new suffix before publishing either.
                file.compact(allowance, encoded.len())?;
                if !prefix.is_empty() {
                    head.append(&prefix, self.cap / 2)?;
                    self.head_written += n;
                }
                head.header.write(&mut head.file)?;
                if kept < bytes.len() && !file.header.cut {
                    file.header.cut = true;
                    file.header.write(&mut file.file)?;
                }
                file.append(&encoded, allowance)?;
            }
        }
        Ok(())
    }
    pub fn clear(&mut self) -> Result<()> {
        if self.file.is_none() {
            clear_files(&self.path)?;
        }
        self.history.clear();
        self.filter = Filter::default();
        if let (Some(file), Some(head)) = (&mut self.file, &mut self.head) {
            let generation = logfile::generation()?;
            for container in [file, head] {
                container.header = logfile::Header {
                    generation,
                    sealed: false,
                    cut: false,
                    legacy_end: 0,
                };
                container.file =
                    logfile::replace(&container.path, |file| container.header.write(file))?;
                container.end = logfile::HEADER_SIZE;
                container.sequence = 0;
            }
            self.head_written = 0;
        }
        Ok(())
    }
}
pub fn entries() -> Result<Vec<(String, State)>> {
    entries_in(&directory()?)
}
pub fn entries_in(dir: &Path) -> Result<Vec<(String, State)>> {
    let mut names = std::collections::BTreeSet::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(vec![]);
        }
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let e = entry?;
        let name = e.file_name();
        let text = name.to_string_lossy();
        if text.starts_with('.') {
            continue;
        }
        if e.file_type()?.is_socket() {
            names.insert(text.into_owned());
        } else if (e.file_type()?.is_file() || e.file_type()?.is_symlink())
            && let Some(stem) = text
                .strip_suffix(".ended")
                .or_else(|| text.strip_suffix(".log"))
                .or_else(|| text.strip_suffix(".head"))
        {
            names.insert(stem.to_string());
        }
    }
    let mut result = vec![];
    for name in names {
        let state = state(&dir.join(&name))?;
        if state != State::Missing {
            result.push((name, state));
        }
    }
    Ok(result)
}
pub fn list(ended_only: bool) -> Result<()> {
    let mut count = 0;
    for (name, s) in entries()? {
        if ended_only && s != State::Ended {
            continue;
        }
        let label = match s {
            State::Running => "running",
            State::Attached => "attached",
            State::Ended => "ended",
            State::Stale => "stale",
            State::Missing => "missing",
        };
        println!("{name:<24} [{label}]");
        count += 1;
    }
    if count == 0 {
        println!("(no sessions)");
    }
    Ok(())
}
pub fn remove(path: &Path) -> Result<()> {
    if matches!(state(path)?, State::Running | State::Attached) {
        return fail("session is running; kill it first");
    }
    logfile::cleanup_session(path)?;
    for suffix in ["", ".log", ".ended", ".head"] {
        let p = side(path, suffix);
        let removable = match fs::symlink_metadata(&p) {
            Ok(metadata) => {
                if suffix.is_empty() {
                    metadata.file_type().is_socket()
                } else {
                    metadata.is_file() || metadata.file_type().is_symlink()
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        if !removable || suffix == ".head" && legacy_head_session(&p)? {
            continue;
        }
        match fs::remove_file(p) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub fn remove_all() -> Result<()> {
    let dir = directory()?;
    for (name, state) in entries()? {
        if matches!(state, State::Ended | State::Stale) {
            remove(&dir.join(name))?;
        }
    }
    Ok(())
}

pub const PREVIEW_LIMIT: usize = 64 * 1024;

/// Read only validated private regular files, with a fixed memory bound.
pub fn preview(path: &Path, beginning: bool) -> io::Result<Vec<u8>> {
    let mut file = if beginning {
        match open_file(&prefix_path(path)?, false) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => open_file(&side(path, ".log"), false)?,
            Err(e) => return Err(e),
        }
    } else {
        open_file(&side(path, ".log"), false)?
    };
    if beginning {
        return Ok(logfile::read(&mut file, PREVIEW_LIMIT, true)?.bytes);
    }
    if logfile::header(&mut file)?.is_some() {
        return decoded_recent(&mut file, false);
    }
    let len = file.metadata()?.len();
    read_recent(&mut file, len, false)
}
fn decoded_recent(file: &mut File, context: bool) -> io::Result<Vec<u8>> {
    let window = logfile::read(file, 2 * PREVIEW_LIMIT, false)?;
    let requested = if context {
        0
    } else {
        window.bytes.len().saturating_sub(PREVIEW_LIMIT)
    };
    let start = if context {
        crate::history::emulator_boundary(&window.bytes, window.cut)
    } else {
        crate::history::suffix_boundary(&window.bytes, requested, window.cut || requested > 0)
    };
    Ok(window.bytes[start..].to_vec())
}
fn read_recent(file: &mut File, mut len: u64, context: bool) -> io::Result<Vec<u8>> {
    for attempt in 0..2 {
        let offset = len.saturating_sub((2 * PREVIEW_LIMIT) as u64);
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        Read::by_ref(file)
            .take((2 * PREVIEW_LIMIT) as u64)
            .read_to_end(&mut bytes)?;
        let latest = file.metadata()?.len();
        if attempt == 0 && (latest < len || bytes.is_empty() && offset > 0) {
            len = latest;
            continue;
        }
        let requested = if context {
            0
        } else {
            bytes.len().saturating_sub(PREVIEW_LIMIT)
        };
        let start = if context {
            crate::history::emulator_boundary(&bytes, offset > 0)
        } else {
            crate::history::suffix_boundary(&bytes, requested, offset > 0 || requested > 0)
        };
        return Ok(bytes[start..].to_vec());
    }
    unreachable!("bounded retry returns")
}

/// The picker emulator receives bounded earlier context, never an unbounded log.
pub fn preview_context(path: &Path, beginning: bool) -> io::Result<Vec<u8>> {
    if beginning {
        return preview(path, true);
    }
    let mut file = open_file(&side(path, ".log"), false)?;
    if logfile::header(&mut file)?.is_some() {
        return decoded_recent(&mut file, true);
    }
    let len = file.metadata()?.len();
    read_recent(&mut file, len, true)
}

pub fn history_exists(path: &Path) -> io::Result<bool> {
    for suffix in [".log", ".head", ".ended"] {
        match fs::symlink_metadata(side(path, suffix)) {
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

pub fn clear_files(path: &Path) -> Result<()> {
    let head = prefix_path(path)?;
    let mut files = Vec::new();
    let mut codec = logfile::Codec::new()?;
    for candidate in [side(path, ".log"), head] {
        match file_options(&candidate, true, false) {
            Ok(mut file) => {
                if logfile::header(&mut file)?.is_some() {
                    logfile::validate(&mut file, &mut codec)?;
                }
                files.push(file);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    for file in files {
        file.set_len(0)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Session(PathBuf);
    impl Session {
        fn new() -> Self {
            let dir = env::temp_dir().join(format!(
                "rtch-log-tests-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            Self(dir.join("session"))
        }
    }
    impl Drop for Session {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap());
        }
    }
    fn disk_size(path: &Path) -> u64 {
        [".log", ".head"]
            .iter()
            .map(|suffix| fs::metadata(side(path, suffix)).unwrap().len())
            .sum()
    }
    fn random_bytes(length: usize) -> Vec<u8> {
        let mut state = 0x1234_5678_u32;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_le_bytes()[0]
            })
            .collect()
    }
    #[test]
    #[allow(clippy::cast_precision_loss)] // Small fixed synthetic byte totals.
    #[ignore = "run explicitly in release mode for codec measurements"]
    fn rust_codec_measurement_level_one_reused_contexts_eight_kib_frames() {
        use std::time::{Duration, Instant};
        let length = 8 * 1024 * 1024;
        let mut text = b"cargo compiling rtch; test history rotation; retained session output\n"
            .repeat(length / 65 + 1);
        text.resize(length, b'\n');
        for (name, input) in [("text", text), ("random", random_bytes(length))] {
            let session = Session::new();
            let mut file = open_file(&session.0.with_extension("measurement"), true).unwrap();
            let mut compression = Vec::new();
            let mut decompression = Vec::new();
            let mut total_encoded = 0;
            for _ in 0..3 {
                let mut codec = logfile::Codec::new().unwrap();
                let mut encode_time = Duration::ZERO;
                let mut decode_time = Duration::ZERO;
                total_encoded = 0;
                for bytes in input.chunks(8192) {
                    let start = Instant::now();
                    let encoded = codec.encode(bytes, 1).unwrap();
                    encode_time += start.elapsed();
                    total_encoded += encoded.len();
                    file.rewind().unwrap();
                    file.write_all(&encoded).unwrap();
                    let record = logfile::Record {
                        start: 0,
                        end: encoded.len() as u64,
                        encoded: encoded.len() - 24,
                        decoded: bytes.len(),
                        sequence: 1,
                    };
                    let start = Instant::now();
                    let decoded = codec.decode(&mut file, record).unwrap();
                    decode_time += start.elapsed();
                    assert_eq!(decoded, bytes);
                }
                compression.push(encode_time);
                decompression.push(decode_time);
            }
            compression.sort();
            decompression.sort();
            println!(
                "{name}: 8 MiB, 8192-byte frames, level 1 + checksum, reused contexts, median 3 runs; encoded incl trailers={total_encoded} ({:.2}%); encode={:.1} MiB/s; decode incl private cached-file reads={:.1} MiB/s",
                total_encoded as f64 * 100.0 / length as f64,
                8.0 / compression[1].as_secs_f64(),
                8.0 / decompression[1].as_secs_f64()
            );
        }
        let mut reopen = Vec::new();
        for _ in 0..3 {
            let session = Session::new();
            let mut log = Log::open(&session.0, 8 * 1024 * 1024).unwrap();
            log.append(&random_bytes(PREVIEW_LIMIT)).unwrap();
            drop(log);
            let start = Instant::now();
            let log = Log::open(&session.0, 256).unwrap();
            reopen.push(start.elapsed());
            assert!(disk_size(&session.0) <= 256);
            assert!(log.head.as_ref().unwrap().header.sealed);
        }
        reopen.sort();
        println!(
            "small-budget reopen: 64 KiB random prefix, 8 MiB -> 256-byte cap, median 3 runs = {:.3} ms",
            reopen[1].as_secs_f64() * 1000.0
        );
    }
    #[test]
    fn compressed_cap_covers_prefix_growth_for_random_and_compressible_streams() {
        for cap in [256, 1024, 8 * 1024 * 1024] {
            for bytes in [random_bytes(8192), vec![b'x'; 8192]] {
                let session = Session::new();
                let mut log = Log::open(&session.0, cap).unwrap();
                let inode = log.file.as_ref().unwrap().file.metadata().unwrap().ino();
                let iterations = if cap == 8 * 1024 * 1024 && bytes[0] != b'x' {
                    2048
                } else {
                    64
                };
                for _ in 0..iterations {
                    log.append(&bytes).unwrap();
                    assert!(disk_size(&session.0) <= cap as u64);
                    assert!(log.history.len() <= cap);
                }
                if iterations == 2048 {
                    assert_ne!(
                        log.file.as_ref().unwrap().file.metadata().unwrap().ino(),
                        inode
                    );
                    assert!(log.file.as_ref().unwrap().sequence > 1024);
                }
                let decoded = logfile::read(
                    log.file.as_mut().map(|c| &mut c.file).unwrap(),
                    PREVIEW_LIMIT,
                    false,
                )
                .unwrap()
                .bytes;
                assert!(
                    !decoded.is_empty()
                        && bytes.ends_with(&decoded[decoded.len().saturating_sub(bytes.len())..])
                );
                assert!(
                    fs::read_dir(session.0.parent().unwrap())
                        .unwrap()
                        .all(|entry| !entry
                            .unwrap()
                            .file_name()
                            .to_string_lossy()
                            .starts_with('.'))
                );
            }
        }
    }
    #[test]
    fn empty_legacy_prefix_backfills_before_migration_but_sealed_empty_prefix_stays_empty() {
        let legacy = Session::new();
        open_file(&side(&legacy.0, ".log"), true)
            .unwrap()
            .write_all(b"earliest retained\n")
            .unwrap();
        open_file(&side(&legacy.0, ".head"), true).unwrap();
        let mut log = Log::open(&legacy.0, 1024).unwrap();
        assert_eq!(preview(&legacy.0, true).unwrap(), b"earliest retained\n");
        log.append(b"next contiguous bytes\n").unwrap();
        assert_eq!(
            preview(&legacy.0, true).unwrap(),
            b"earliest retained\nnext contiguous bytes\n"
        );
        let sealed = Session::new();
        let mut log = Log::open(&sealed.0, 1024).unwrap();
        let head = log.head.as_mut().unwrap();
        head.header.sealed = true;
        head.header.write(&mut head.file).unwrap();
        log.append(b"omitted prefix\n").unwrap();
        drop(log);
        let mut log = Log::open(&sealed.0, 1024).unwrap();
        log.append(b"must not restart capture\n").unwrap();
        assert!(preview(&sealed.0, true).unwrap().is_empty());
        assert!(log.head.as_ref().unwrap().header.sealed);
    }
    #[test]
    fn prefix_fitting_is_bounded_contiguous_and_within_encoded_budget() {
        let input = random_bytes(PREVIEW_LIMIT);
        let session = Session::new();
        let mut file = open_file(&session.0.with_extension("fitting"), true).unwrap();
        let mut codec = logfile::Codec::new().unwrap();
        for budget in [0, 37, 88, 128, 1024, 4 * 1024 * 1024] {
            let (encoded, n) = codec.fitting(&input, 1, budget, true).unwrap();
            assert!(encoded.len() <= budget);
            if n > 0 {
                file.rewind().unwrap();
                file.write_all(&encoded).unwrap();
                let record = logfile::Record {
                    start: 0,
                    end: encoded.len() as u64,
                    encoded: encoded.len() - 24,
                    decoded: n,
                    sequence: 1,
                };
                assert_eq!(codec.decode(&mut file, record).unwrap(), input[..n]);
            } else {
                assert!(encoded.is_empty());
            }
            if budget == 4 * 1024 * 1024 {
                assert_eq!(n, PREVIEW_LIMIT);
            }
        }
    }
    #[test]
    fn prefix_partial_append_recovery_seals_and_empty_suffix_restart_changes_generation() {
        let session = Session::new();
        let mut log = Log::open(&session.0, 1024).unwrap();
        log.append(b"retained prefix\n").unwrap();
        let generation = log.file.as_ref().unwrap().header.generation;
        let mut codec = logfile::Codec::new().unwrap();
        let incomplete = codec
            .encode(b"omitted gap", log.head.as_ref().unwrap().sequence + 1)
            .unwrap();
        let prefix = preview(&session.0, true).unwrap();
        drop(log);
        let mut head = open_file(&side(&session.0, ".head"), true).unwrap();
        head.seek(SeekFrom::End(0)).unwrap();
        head.write_all(&incomplete[..incomplete.len() - 1]).unwrap();
        // Simulate a crash after an eviction replacement, before its next append.
        open_file(&side(&session.0, ".log"), true)
            .unwrap()
            .set_len(logfile::HEADER_SIZE)
            .unwrap();
        let mut log = Log::open(&session.0, 1024).unwrap();
        assert_ne!(log.file.as_ref().unwrap().header.generation, generation);
        assert!(log.head.as_ref().unwrap().header.sealed);
        log.append(b"after crash\n").unwrap();
        assert_eq!(preview(&session.0, true).unwrap(), prefix);
    }
    #[test]
    fn tiny_prefix_seals_contiguously_across_restart_and_clear() {
        let session = Session::new();
        let input = random_bytes(8192);
        let mut log = Log::open(&session.0, 256).unwrap();
        log.append(&input).unwrap();
        let prefix = preview(&session.0, true).unwrap();
        assert!(!prefix.is_empty() && input.starts_with(&prefix));
        assert!(log.head.as_ref().unwrap().header.sealed);
        log.append(b"must not fill a gap").unwrap();
        drop(log);
        let mut log = Log::open(&session.0, 256).unwrap();
        log.append(b"after restart").unwrap();
        assert_eq!(preview(&session.0, true).unwrap(), prefix);
        log.clear().unwrap();
        log.append(b"new contiguous prefix").unwrap();
        assert_eq!(preview(&session.0, true).unwrap(), b"new contiguous prefix");
        assert!(disk_size(&session.0) <= 256);
    }
    #[test]
    fn one_byte_reads_publish_immediately_and_preserve_full_default_prefix() {
        let session = Session::new();
        let mut log = Log::open(&session.0, 8 * 1024 * 1024).unwrap();
        let bytes = random_bytes(PREVIEW_LIMIT);
        for (i, byte) in bytes.iter().enumerate() {
            log.append(std::slice::from_ref(byte)).unwrap();
            if i < 4 {
                assert_eq!(preview(&session.0, true).unwrap(), bytes[..=i]);
            }
        }
        assert_eq!(preview(&session.0, true).unwrap(), bytes);
        assert!(disk_size(&session.0) <= 8 * 1024 * 1024);
        drop(log);
        let mut log = Log::open(&session.0, 8 * 1024 * 1024).unwrap();
        log.append(b"later").unwrap();
        assert_eq!(preview(&session.0, true).unwrap(), bytes);
    }
    #[test]
    fn bounded_readers_skip_older_frames_and_filter_split_controls_and_utf8() {
        let session = Session::new();
        let mut log = Log::open(&session.0, 8 * 1024 * 1024).unwrap();
        for byte in b"begin \xe2\x82\xac\x1b[6nVISIBLE\x1b]10;?\x07END\n" {
            log.append(std::slice::from_ref(byte)).unwrap();
        }
        let expected = "begin €VISIBLEEND\n".as_bytes();
        assert_eq!(log.history.iter().copied().collect::<Vec<_>>(), expected);
        drop(log);
        let mut log = Log::open(&session.0, 8 * 1024 * 1024).unwrap();
        assert_eq!(log.history.iter().copied().collect::<Vec<_>>(), expected);
        for _ in 0..40 {
            log.append(&b"safe line\n".repeat(900)).unwrap();
        }
        let file = &mut log.file.as_mut().unwrap().file;
        let first = logfile::next(file, logfile::HEADER_SIZE, file.metadata().unwrap().len())
            .unwrap()
            .unwrap();
        file.seek(SeekFrom::Start(first.end - 25)).unwrap();
        let mut byte = [0];
        file.read_exact(&mut byte).unwrap();
        byte[0] ^= 1;
        file.seek(SeekFrom::Start(first.end - 25)).unwrap();
        file.write_all(&byte).unwrap();
        // A recent preview reads its selected suffix, not the corrupt old first frame.
        assert!(
            preview(&session.0, false)
                .unwrap()
                .ends_with(b"safe line\n")
        );
        assert!(preview_context(&session.0, false).unwrap().len() <= 2 * PREVIEW_LIMIT);
        drop(log);
        assert!(Log::open(&session.0, 8 * 1024 * 1024).is_err());
    }
    #[test]
    fn follower_handles_atomic_rotation_clear_and_restart_without_duplicates() {
        let session = Session::new();
        let mut log = Log::open(&session.0, 1024).unwrap();
        log.append(b"already seen\n").unwrap();
        let mut file = open_file(&side(&session.0, ".log"), false).unwrap();
        let window = logfile::read(&mut file, PREVIEW_LIMIT, false).unwrap();
        let initial_inode = file.metadata().unwrap().ino();
        let mut follower = logfile::Follower::new(file, side(&session.0, ".log"), &window).unwrap();
        let mut seen = Vec::<u8>::new();
        let mut expected = Vec::<u8>::new();
        for i in 0..80 {
            let bytes = format!("message {i:03} different line\n");
            log.append(bytes.as_bytes()).unwrap();
            expected.extend(bytes.as_bytes());
            follower
                .poll(|bytes, reset| {
                    assert!(!reset);
                    seen.extend(bytes);
                    Ok(())
                })
                .unwrap();
            assert!(disk_size(&session.0) <= 1024);
        }
        assert_eq!(seen, expected);
        assert_ne!(follower.file.metadata().unwrap().ino(), initial_inode);
        log.clear().unwrap();
        log.append(b"after clear\n").unwrap();
        let mut resets = 0;
        seen.clear();
        follower
            .poll(|bytes, reset| {
                resets += usize::from(reset);
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(resets, 1);
        assert_eq!(seen, b"after clear\n");
        drop(log);
        let mut log = Log::open(&session.0, 1024).unwrap();
        log.append(b"after restart\n").unwrap();
        seen.clear();
        follower
            .poll(|bytes, reset| {
                assert!(!reset);
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, b"after restart\n");
    }
    #[test]
    fn partial_final_record_waits_for_commit_once_and_is_discarded_on_reopen() {
        let session = Session::new();
        let mut log = Log::open(&session.0, 1024).unwrap();
        log.append(b"committed\n").unwrap();
        drop(log);
        let path = side(&session.0, ".log");
        let mut file = open_file(&path, false).unwrap();
        let window = logfile::read(&mut file, PREVIEW_LIMIT, false).unwrap();
        let mut follower = logfile::Follower::new(file, path.clone(), &window).unwrap();
        let mut codec = logfile::Codec::new().unwrap();
        let record = codec
            .encode(b"published exactly once\n", window.sequence + 1)
            .unwrap();
        let mut writer = open_file(&path, true).unwrap();
        writer.seek(SeekFrom::End(0)).unwrap();
        let mut seen = Vec::<u8>::new();
        for (i, byte) in record.iter().enumerate() {
            writer.write_all(std::slice::from_ref(byte)).unwrap();
            follower
                .poll(|bytes, reset| {
                    assert!(!reset);
                    seen.extend(bytes);
                    Ok(())
                })
                .unwrap();
            if i + 1 < record.len() {
                assert!(seen.is_empty());
            }
        }
        assert_eq!(seen, b"published exactly once\n");
        follower
            .poll(|bytes, _| {
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, b"published exactly once\n");
        let length = writer.metadata().unwrap().len();
        let record = codec
            .encode(b"incomplete lost output", window.sequence + 2)
            .unwrap();
        writer.write_all(&record[..record.len() - 1]).unwrap();
        assert!(
            logfile::read(&mut writer, PREVIEW_LIMIT, false)
                .unwrap()
                .bytes
                .ends_with(b"once\n")
        );
        let mut log = Log::open(&session.0, 1024).unwrap();
        assert_eq!(fs::metadata(path).unwrap().len(), length);
        log.append(b"recovered\n").unwrap();
        assert!(
            preview(&session.0, false)
                .unwrap()
                .ends_with(b"recovered\n")
        );
    }
    #[test]
    fn active_legacy_follower_migrates_on_reopen_without_duplicate_retained_output() {
        let session = Session::new();
        let log_path = side(&session.0, ".log");
        let mut writer = open_file(&log_path, true).unwrap();
        writer.write_all(b"legacy beginning\n").unwrap();
        let mut file = open_file(&log_path, false).unwrap();
        let window = logfile::read(&mut file, PREVIEW_LIMIT, false).unwrap();
        let mut follower = logfile::Follower::new(file, log_path.clone(), &window).unwrap();
        writer.write_all(b"live plaintext\n").unwrap();
        let mut seen = Vec::<u8>::new();
        follower
            .poll(|bytes, reset| {
                assert!(!reset);
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, b"live plaintext\n");
        assert_eq!(
            fs::read(&log_path).unwrap(),
            b"legacy beginning\nlive plaintext\n"
        );
        let mut log = Log::open(&session.0, 1024).unwrap();
        assert!(
            logfile::header(&mut log.file.as_mut().unwrap().file)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            preview(&session.0, true).unwrap(),
            b"legacy beginning\nlive plaintext\n"
        );
        assert!(disk_size(&session.0) <= 1024);
        log.append(b"compressed restart\n").unwrap();
        assert!(
            preview(&session.0, false)
                .unwrap()
                .ends_with(b"compressed restart\n")
        );
    }
    #[test]
    fn oversized_append_splits_frames_and_keeps_replay_independently_bounded() {
        let session = Session::new();
        let input = random_bytes(3 * logfile::FRAME_LIMIT + 17);
        let mut log = Log::open(&session.0, 8 * 1024 * 1024).unwrap();
        log.append(&input).unwrap();
        let file = &mut log.file.as_mut().unwrap().file;
        let end = file.metadata().unwrap().len();
        let mut offset = logfile::HEADER_SIZE;
        let mut records = 0;
        while let Some(record) = logfile::next(file, offset, end).unwrap() {
            assert!(record.decoded <= logfile::FRAME_LIMIT);
            offset = record.end;
            records += 1;
        }
        assert_eq!(records, 4);
        assert_eq!(
            logfile::read(file, input.len(), false).unwrap().bytes,
            input
        );
        let disabled = Session::new();
        let mut memory = Log::open(&disabled.0, 0).unwrap();
        memory
            .append(&vec![b'x'; 4 * logfile::FRAME_LIMIT])
            .unwrap();
        assert_eq!(memory.history.len(), logfile::FRAME_LIMIT);
    }
    #[test]
    fn first_suffix_fitting_preserves_truncation_provenance_without_rotation() {
        let session = Session::new();
        let mut log = Log::open(&session.0, 256).unwrap();
        let bytes = random_bytes(logfile::FRAME_LIMIT);
        log.append(&bytes).unwrap();
        let file = &mut log.file.as_mut().unwrap().file;
        let window = logfile::read(file, logfile::FRAME_LIMIT, false).unwrap();
        assert!(window.cut && !window.earlier);
        assert!(!window.bytes.is_empty() && window.bytes.len() < bytes.len());
        assert_eq!(window.bytes, bytes[bytes.len() - window.bytes.len()..]);
        assert!(disk_size(&session.0) <= 256);
    }
    #[test]
    fn follower_resets_on_each_fitted_suffix_gap_after_observed_osc_opener() {
        let session = Session::new();
        let mut log = Log::open(&session.0, 1024).unwrap();
        log.append(b"\x1b]0;").unwrap();
        let path = side(&session.0, ".log");
        let mut file = open_file(&path, false).unwrap();
        let window = logfile::read(&mut file, PREVIEW_LIMIT, false).unwrap();
        let mut filter = Filter::default();
        assert!(filter.feed(&window.bytes).is_empty());
        let mut follower = logfile::Follower::new(file, path, &window).unwrap();
        let mut output = Vec::new();
        let mut resets = 0;
        for marker in [b"FIRST_VISIBLE\n".as_slice(), b"SECOND_VISIBLE\n"] {
            let mut bytes = b"\x1b\\".to_vec();
            bytes.extend(
                random_bytes(logfile::FRAME_LIMIT / 2)
                    .into_iter()
                    .map(|byte| b' ' + byte % 95),
            );
            bytes.push(b'\n');
            bytes.extend(marker);
            log.append(&bytes).unwrap();
            assert!(log.file.as_ref().unwrap().header.cut);
            for _ in 0..2 {
                follower
                    .poll(|bytes, reset| {
                        if reset {
                            resets += 1;
                            filter = Filter::default();
                        }
                        output.extend(filter.feed(bytes));
                        Ok(())
                    })
                    .unwrap();
            }
            assert_eq!(
                output
                    .windows(marker.len())
                    .filter(|w| *w == marker)
                    .count(),
                1
            );
            if marker == b"FIRST_VISIBLE\n" {
                log.append(b"\x1b]0;").unwrap();
                follower
                    .poll(|bytes, reset| {
                        assert!(!reset);
                        assert!(filter.feed(bytes).is_empty());
                        Ok(())
                    })
                    .unwrap();
            }
        }
        assert_eq!(resets, 2);
        assert!(disk_size(&session.0) <= 1024);
    }
    #[test]
    fn committed_block_length_corruption_errors_before_either_history_file_mutates() {
        let session = Session::new();
        let mut log = Log::open(&session.0, 1024).unwrap();
        log.append(&random_bytes(32)).unwrap();
        drop(log);
        let path = side(&session.0, ".log");
        let mut bytes = fs::read(&path).unwrap();
        let start = usize::try_from(logfile::HEADER_SIZE).unwrap();
        let descriptor = bytes[start + 4];
        let single = descriptor & 0x20 != 0;
        let content_size = match descriptor >> 6 {
            0 => usize::from(single),
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let dictionary = match descriptor & 3 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let block = start + 5 + usize::from(!single) + dictionary + content_size;
        let old = u32::from_le_bytes([bytes[block], bytes[block + 1], bytes[block + 2], 0]);
        let corrupted = ((old & 7) | 131_071 << 3).to_le_bytes();
        bytes[block..block + 3].copy_from_slice(&corrupted[..3]);
        fs::write(&path, &bytes).unwrap();
        let head = fs::read(side(&session.0, ".head")).unwrap();
        assert!(preview(&session.0, false).is_err());
        assert!(Log::open(&session.0, 1024).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::read(side(&session.0, ".head")).unwrap(), head);
    }
    #[test]
    fn differing_shorter_or_equal_legacy_suffix_seals_prefix_before_new_output() {
        for suffix in [b"S".as_slice(), b"OTHER_PREFIX"] {
            let session = Session::new();
            open_file(&side(&session.0, ".head"), true)
                .unwrap()
                .write_all(b"FIRST_PREFIX")
                .unwrap();
            open_file(&side(&session.0, ".log"), true)
                .unwrap()
                .write_all(suffix)
                .unwrap();
            let mut log = Log::open(&session.0, 1024).unwrap();
            assert!(log.head.as_ref().unwrap().header.sealed);
            log.append(b"AFTER_GAP").unwrap();
            assert_eq!(preview(&session.0, true).unwrap(), b"FIRST_PREFIX");
        }
    }
    #[test]
    fn supervisor_append_rejects_near_maximum_suffix_or_prefix_sequences_without_mutation() {
        for (suffix, sequence, oversized) in [
            (".log", u64::MAX - 1, false),
            (".head", u64::MAX - 1, false),
            (".log", u64::MAX - 2, true),
        ] {
            let session = Session::new();
            let mut log = Log::open(&session.0, 1024).unwrap();
            log.append(b"retained\n").unwrap();
            drop(log);
            let path = side(&session.0, suffix);
            let mut file = open_file(&path, true).unwrap();
            file.seek(SeekFrom::End(-8)).unwrap();
            file.write_all(&sequence.to_le_bytes()).unwrap();
            let mut log = Log::open(&session.0, 1024).unwrap();
            let before = fs::read(side(&session.0, ".log")).unwrap();
            let head = fs::read(side(&session.0, ".head")).unwrap();
            let bytes = if oversized {
                random_bytes(logfile::FRAME_LIMIT)
            } else {
                b"must not publish".to_vec()
            };
            assert!(log.append(&bytes).is_err());
            assert_eq!(fs::read(side(&session.0, ".log")).unwrap(), before);
            assert_eq!(fs::read(side(&session.0, ".head")).unwrap(), head);
        }
    }
    #[test]
    fn reopen_and_removal_clean_abandoned_session_scoped_encoded_copies() {
        let session = Session::new();
        let mut log = Log::open(&session.0, 1024).unwrap();
        log.append(b"KEEP_CANONICAL\n").unwrap();
        drop(log);
        let stage = session
            .0
            .with_file_name(".session.log.rtch-0000000000000011");
        let mut file = open_file(&stage, true).unwrap();
        logfile::Header {
            generation: 2,
            sealed: false,
            cut: false,
            legacy_end: 0,
        }
        .write(&mut file)
        .unwrap();
        file.write_all(b"\x28\xb5").unwrap();
        drop(file); // Simulate descriptors closed by an interrupted writer.
        let log = Log::open(&session.0, 1024).unwrap();
        assert!(!stage.exists());
        assert_eq!(preview(&session.0, false).unwrap(), b"KEEP_CANONICAL\n");
        drop(log);
        let stage = session
            .0
            .with_file_name(".session.head.rtch-0000000000000012");
        let mut file = open_file(&stage, true).unwrap();
        logfile::Header {
            generation: 3,
            sealed: false,
            cut: false,
            legacy_end: 0,
        }
        .write(&mut file)
        .unwrap();
        drop(file);
        remove(&session.0).unwrap();
        assert!(!stage.exists());
    }
    #[test]
    fn partially_observed_legacy_migration_emits_unseen_and_compressed_output_once() {
        let session = Session::new();
        let path = side(&session.0, ".log");
        let mut writer = open_file(&path, true).unwrap();
        writer.write_all(b"SEEN\n").unwrap();
        let mut file = open_file(&path, false).unwrap();
        let window = logfile::read(&mut file, PREVIEW_LIMIT, false).unwrap();
        let mut follower = logfile::Follower::new(file, path, &window).unwrap();
        writer.write_all(b"UNSEEN\n").unwrap();
        let mut log = Log::open(&session.0, 1024).unwrap();
        let mut seen = Vec::<u8>::new();
        follower
            .poll(|bytes, reset| {
                assert!(!reset);
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        follower
            .poll(|bytes, _| {
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, b"UNSEEN\n");
        log.append(b"COMPRESSED_NEW\n").unwrap();
        follower
            .poll(|bytes, reset| {
                assert!(!reset);
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        follower
            .poll(|bytes, _| {
                seen.extend(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, b"UNSEEN\nCOMPRESSED_NEW\n");
    }
    #[test]
    fn compressed_orphan_control_window_preserves_visible_marker_without_payload() {
        let session = Session::new();
        let mut log = Log::open(&session.0, 8 * 1024 * 1024).unwrap();
        log.append(b"SAFE_BEGIN\n\x1b]0;").unwrap();
        log.append(&b"ORPHAN_PAYLOAD\n".repeat(30_000)).unwrap();
        log.append(b"\x1b\\VISIBLE_MARKER\n").unwrap();
        let context = preview_context(&session.0, false).unwrap();
        assert!(context.len() <= 2 * PREVIEW_LIMIT);
        let screen = crate::history::terminal_preview(&context, 24, 80, false);
        assert!(screen.contains("VISIBLE_MARKER"), "{screen}");
        assert!(!screen.contains("ORPHAN_PAYLOAD"), "{screen}");
    }
    #[test]
    fn committed_corruption_lengths_versions_and_unsafe_files_fail_before_mutation() {
        for damage in [0, 1, 2, 3] {
            let session = Session::new();
            let mut log = Log::open(&session.0, 1024).unwrap();
            log.append(b"safe output\n").unwrap();
            drop(log);
            let path = side(&session.0, ".log");
            let mut bytes = fs::read(&path).unwrap();
            let position = match damage {
                0 => bytes.len() - 25, // zstd checksum
                1 => bytes.len() - 9,  // decoded length bound
                2 => 16,               // unsupported format version
                _ => bytes.len() - 16, // compressed length
            };
            bytes[position] ^= 0xff;
            fs::write(&path, &bytes).unwrap();
            let head = fs::read(side(&session.0, ".head")).unwrap();
            assert!(Log::open(&session.0, 1024).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(fs::read(side(&session.0, ".head")).unwrap(), head);
        }
        let session = Session::new();
        let path = side(&session.0, ".log");
        open_file(&path, true)
            .unwrap()
            .write_all(b"legacy unchanged")
            .unwrap();
        let head = side(&session.0, ".head");
        fs::write(&head, b"unsafe").unwrap();
        fs::set_permissions(&head, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Log::open(&session.0, 1024).is_err());
        assert!(clear_files(&session.0).is_err());
        assert_eq!(fs::read(path).unwrap(), b"legacy unchanged");
    }
    #[test]
    fn prefix_survives_rotation_restart_and_clear() {
        let session = Session::new();
        let path = &session.0;
        let mut log = Log::open(path, 256).unwrap();
        log.append(b"FIRST\n").unwrap();
        for _ in 0..10 {
            log.append(&[b'x'; 2000]).unwrap();
        }
        let prefix = preview(path, true).unwrap();
        assert!(prefix.starts_with(b"FIRST\n"));
        assert!(prefix.len() <= PREVIEW_LIMIT);
        assert!(!preview(path, false).unwrap().starts_with(b"FIRST"));
        drop(log);
        let mut log = Log::open(path, 256).unwrap();
        log.append(b"RESTARTED").unwrap();
        assert_eq!(preview(path, true).unwrap(), prefix);
        log.clear().unwrap();
        assert!(preview(path, true).unwrap().is_empty());
        assert!(preview(path, false).unwrap().is_empty());
        log.append(b"NEW BEGINNING").unwrap();
        assert_eq!(preview(path, true).unwrap(), b"NEW BEGINNING");
        drop(log);
        clear_files(path).unwrap();
        assert!(preview(path, true).unwrap().is_empty());
        remove(path).unwrap();
        assert!(!side(path, ".head").exists());
    }
    #[test]
    fn legacy_prefix_is_backfilled_before_rotation_and_disabled_logs_create_nothing() {
        let session = Session::new();
        let path = &session.0;
        open_file(&side(path, ".log"), true)
            .unwrap()
            .write_all(b"OLDEST retained output LATEST")
            .unwrap();
        assert!(preview(path, true).unwrap().starts_with(b"OLDEST"));
        let mut log = Log::open(path, 256).unwrap();
        assert_eq!(
            preview(path, true).unwrap(),
            b"OLDEST retained output LATEST"
        );
        log.append(&[b'x'; 100]).unwrap();
        assert_eq!(
            preview(path, true).unwrap(),
            b"OLDEST retained output LATEST"
        );
        let disabled = Session::new();
        let mut log = Log::open(&disabled.0, 0).unwrap();
        log.append(b"memory only").unwrap();
        assert!(!side(&disabled.0, ".log").exists());
        assert!(!side(&disabled.0, ".head").exists());
    }
    #[test]
    fn preview_bounds_and_rejects_unsafe_prefix_without_fallback() {
        use std::os::unix::fs::symlink;
        let session = Session::new();
        let path = &session.0;
        open_file(&side(path, ".log"), true)
            .unwrap()
            .write_all(&b"zzzzzzzz\n".repeat(12_000))
            .unwrap();
        assert!(preview(path, false).unwrap().len() <= PREVIEW_LIMIT);
        let victim = path.with_file_name("victim");
        fs::write(&victim, b"SECRET").unwrap();
        symlink(&victim, side(path, ".head")).unwrap();
        assert!(preview(path, true).is_err());
        assert!(Log::open(path, 1024).is_err());
        assert_eq!(fs::read(victim).unwrap(), b"SECRET");
    }
    #[test]
    fn disabled_restart_clear_erases_retained_files_without_creating_them() {
        let session = Session::new();
        let mut log = Log::open(&session.0, 1024).unwrap();
        log.append(b"retained history").unwrap();
        drop(log);
        let mut log = Log::open(&session.0, 0).unwrap();
        log.clear().unwrap();
        assert!(preview(&session.0, true).unwrap().is_empty());
        assert!(preview(&session.0, false).unwrap().is_empty());
        let fresh = Session::new();
        Log::open(&fresh.0, 0).unwrap().clear().unwrap();
        assert!(!side(&fresh.0, ".log").exists() && !side(&fresh.0, ".head").exists());
    }
    #[test]
    fn bounded_tail_preserves_plain_text_and_omits_control_payloads() {
        let session = Session::new();
        let path = side(&session.0, ".log");
        let mut file = open_file(&path, true).unwrap();
        file.write_all(&b"ordinary line\n".repeat(20_000)).unwrap();
        file.write_all(b"FINAL_PLAIN_LINE\n").unwrap();
        let recent = preview(&session.0, false).unwrap();
        assert!(recent.len() <= PREVIEW_LIMIT && recent.ends_with(b"FINAL_PLAIN_LINE\n"));
        for start in [b"\x1b]0;".as_slice(), b"\x1bP", b"\x9d0;"] {
            file.set_len(0).unwrap();
            file.rewind().unwrap();
            file.write_all(b"SAFE_BEGIN\n").unwrap();
            file.write_all(start).unwrap();
            file.write_all(&b"PAYLOAD_SECRET\n".repeat(10_000)).unwrap();
            file.write_all(b"\x1b\\SAFE_END\n").unwrap();
            let recent = preview(&session.0, false).unwrap();
            assert_eq!(crate::history::preview(&recent), "SAFE_END\n");
        }
    }
    #[test]
    fn tail_retries_when_rotation_invalidates_a_metadata_offset() {
        let session = Session::new();
        let mut file = open_file(&side(&session.0, ".log"), true).unwrap();
        file.write_all(&b"old line\n".repeat(40_000)).unwrap();
        let stale_len = file.metadata().unwrap().len();
        file.set_len(0).unwrap();
        file.rewind().unwrap();
        file.write_all(b"AFTER_ROTATION\n").unwrap();
        assert_eq!(
            read_recent(&mut file, stale_len, false).unwrap(),
            b"AFTER_ROTATION\n"
        );
    }
    #[test]
    fn emulator_context_retains_large_redraw_streams_without_newlines() {
        let session = Session::new();
        let mut file = open_file(&side(&session.0, ".log"), true).unwrap();
        file.write_all(&b"\x1b[2J\x1b[1;1HOLD_DRAW".repeat(10_000))
            .unwrap();
        file.write_all(b"\x1b[2J\x1b[1;1HFINAL_DRAW").unwrap();
        let bytes = preview_context(&session.0, false).unwrap();
        assert!(!bytes.is_empty() && bytes.len() <= 2 * PREVIEW_LIMIT);
        assert_eq!(
            crate::history::terminal_preview(&bytes, 95, 196, false),
            "FINAL_DRAW"
        );
    }
    #[test]
    fn head_file_contains_exact_first_64_kib_at_larger_capacity() {
        let session = Session::new();
        let bytes = (0..3 * PREVIEW_LIMIT)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect::<Vec<_>>();
        let mut log = Log::open(&session.0, 2 * PREVIEW_LIMIT).unwrap();
        log.append(&bytes).unwrap();
        let head = side(&session.0, ".head");
        assert!(fs::metadata(&head).unwrap().len() <= PREVIEW_LIMIT as u64);
        assert_eq!(preview(&session.0, true).unwrap(), bytes[..PREVIEW_LIMIT]);
    }
    #[test]
    fn legacy_head_socket_is_not_a_prefix_or_parent_removal_target() {
        let session = Session::new();
        let head = side(&session.0, ".head");
        let live = UnixListener::bind(&head).unwrap();
        fs::set_permissions(&head, fs::Permissions::from_mode(0o600)).unwrap();
        let resolved = head.canonicalize().unwrap();
        assert_eq!(state(&session.0).unwrap(), State::Missing);
        assert_eq!(state(&head).unwrap(), State::Running);
        assert_eq!(resolve(&head).unwrap(), resolved);
        assert!(Log::open(&session.0, 1024).is_err());
        assert!(!side(&session.0, ".log").exists());
        remove(&session.0).unwrap();
        assert!(head.exists());
        drop(live);
        assert_eq!(state(&head).unwrap(), State::Stale);
        remove(&head).unwrap();
        assert!(!head.exists());
        open_file(&side(&head, ".log"), true)
            .unwrap()
            .write_all(b"LEGACY")
            .unwrap();
        assert_eq!(resolve(&head).unwrap(), resolved);
        assert_eq!(state(&head).unwrap(), State::Ended);
        assert!(Log::open(&session.0, 1024).is_err());
        assert_eq!(fs::read(side(&head, ".log")).unwrap(), b"LEGACY");
        assert!(!side(&session.0, ".log").exists() && !head.exists());
        remove(&head).unwrap();
        assert!(resolve(&head).is_err());
    }
    #[test]
    fn remove_unlinks_prefix_symlinks_without_touching_their_target() {
        let session = Session::new();
        let target = session.0.with_file_name("private-target");
        fs::write(&target, b"PRESERVE").unwrap();
        let head = side(&session.0, ".head");
        std::os::unix::fs::symlink(&target, &head).unwrap();
        remove(&session.0).unwrap();
        assert!(fs::symlink_metadata(head).is_err());
        assert_eq!(fs::read(target).unwrap(), b"PRESERVE");
    }
}
