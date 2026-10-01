use crate::{Result, fail, history::Filter, os};
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
    file: Option<File>,
    head: Option<File>,
    head_written: usize,
    pub cap: usize,
    pub history: VecDeque<u8>,
    filter: Filter,
    written: u64,
}
impl Log {
    pub fn open(path: &Path, cap: usize) -> Result<Self> {
        let head_path = (cap != 0).then(|| prefix_path(path)).transpose()?;
        let file = if cap == 0 {
            None
        } else {
            Some(open_file(&side(path, ".log"), true)?)
        };
        let mut result = Self {
            path: path.to_owned(),
            file,
            head: None,
            head_written: 0,
            cap,
            history: VecDeque::new(),
            filter: Filter::default(),
            written: 0,
        };
        if let Some(f) = &mut result.file {
            let head_cap = cap.min(PREVIEW_LIMIT);
            let mut head = open_file(head_path.as_ref().expect("enabled prefix"), true)?;
            if head.metadata()?.len() > head_cap as u64 {
                head.set_len(head_cap as u64)?;
            }
            // Legacy logs can only supply their earliest retained bytes. Copy
            // before suffix rotation, and preserve that prefix across restarts.
            if head.metadata()?.len() == 0 {
                let mut beginning = Vec::new();
                f.rewind()?;
                Read::by_ref(f)
                    .take(head_cap as u64)
                    .read_to_end(&mut beginning)?;
                head.write_all(&beginning)?;
            }
            result.head_written = usize::try_from(head.seek(SeekFrom::End(0))?)?;
            result.head = Some(head);
            let len = f.metadata()?.len();
            let offset = len.saturating_sub(cap as u64);
            f.seek(SeekFrom::Start(offset))?;
            let mut bytes = Vec::new();
            f.take(cap as u64).read_to_end(&mut bytes)?;
            result.filter.feed_into(&bytes, &mut result.history);
            if len > cap as u64 {
                f.set_len(0)?;
                f.seek(SeekFrom::Start(0))?;
                f.write_all(&bytes)?;
            }
            result.written = f.seek(SeekFrom::End(0))?;
        }
        Ok(result)
    }
    pub fn append(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(head) = &mut self.head {
            let n = bytes
                .len()
                .min(self.cap.min(PREVIEW_LIMIT) - self.head_written);
            head.write_all(&bytes[..n])?;
            self.head_written += n;
        }
        self.filter.feed_into(bytes, &mut self.history);
        let keep = if self.cap == 0 { 128 * 1024 } else { self.cap };
        if self.history.len() > keep {
            self.history.drain(..self.history.len() - keep);
        }
        if let Some(f) = &mut self.file {
            f.write_all(bytes)?;
            self.written += bytes.len() as u64;
            if self.written > self.cap.saturating_mul(2) as u64 {
                f.seek(SeekFrom::Start(
                    self.written.saturating_sub(self.cap as u64),
                ))?;
                let mut recent = Vec::with_capacity(self.cap);
                f.take(self.cap as u64).read_to_end(&mut recent)?;
                f.set_len(0)?;
                f.seek(SeekFrom::Start(0))?;
                f.write_all(&recent)?;
                self.written = recent.len() as u64;
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
        if let Some(f) = &mut self.file {
            f.set_len(0)?;
            f.rewind()?;
            self.written = 0;
        }
        if let Some(head) = &mut self.head {
            head.set_len(0)?;
            head.rewind()?;
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
        let mut bytes = Vec::new();
        file.take(PREVIEW_LIMIT as u64).read_to_end(&mut bytes)?;
        return Ok(bytes);
    }
    let len = file.metadata()?.len();
    read_recent(&mut file, len, false)
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
    for suffix in [".log", ".head"] {
        match file_options(&side(path, suffix), true, false) {
            Ok(file) => file.set_len(0)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
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
    #[test]
    fn prefix_survives_rotation_restart_and_clear() {
        let session = Session::new();
        let path = &session.0;
        let mut log = Log::open(path, 32).unwrap();
        log.append(b"FIRST\n").unwrap();
        log.append(&[b'x'; 200]).unwrap();
        let prefix = preview(path, true).unwrap();
        assert!(prefix.starts_with(b"FIRST\n"));
        assert_eq!(prefix.len(), 32);
        assert!(!preview(path, false).unwrap().starts_with(b"FIRST"));
        drop(log);
        let mut log = Log::open(path, 32).unwrap();
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
        let mut log = Log::open(path, 8).unwrap();
        assert_eq!(preview(path, true).unwrap(), b"OLDEST r");
        log.append(&[b'x'; 100]).unwrap();
        assert_eq!(preview(path, true).unwrap(), b"OLDEST r");
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
        assert_eq!(fs::metadata(&head).unwrap().len(), PREVIEW_LIMIT as u64);
        assert_eq!(fs::read(head).unwrap(), bytes[..PREVIEW_LIMIT]);
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
