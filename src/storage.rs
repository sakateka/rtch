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
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(
            if side(path, ".ended").exists() || side(path, ".log").exists() {
                State::Ended
            } else {
                State::Missing
            },
        ),
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
    let f = OpenOptions::new()
        .read(true)
        .write(write)
        .create(write)
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
    file: Option<File>,
    pub cap: usize,
    pub history: VecDeque<u8>,
    filter: Filter,
    written: u64,
}
impl Log {
    pub fn open(path: &Path, cap: usize) -> Result<Self> {
        let file = if cap == 0 {
            None
        } else {
            Some(open_file(&side(path, ".log"), true)?)
        };
        let mut result = Self {
            file,
            cap,
            history: VecDeque::new(),
            filter: Filter::default(),
            written: 0,
        };
        if let Some(f) = &mut result.file {
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
        self.history.clear();
        self.filter = Filter::default();
        if let Some(f) = &mut self.file {
            f.set_len(0)?;
            f.rewind()?;
            self.written = 0;
        }
        Ok(())
    }
}
pub fn entries() -> Result<Vec<(String, State)>> {
    let dir = directory()?;
    let mut names = std::collections::BTreeSet::new();
    let entries = match fs::read_dir(&dir) {
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
        if let Some(stem) = text
            .strip_suffix(".ended")
            .or_else(|| text.strip_suffix(".log"))
        {
            names.insert(stem.to_string());
        } else if e.file_type()?.is_socket() {
            names.insert(text.into_owned());
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
    for p in [path.to_owned(), side(path, ".log"), side(path, ".ended")] {
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
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let e = entry?;
        let p = e.path();
        let n = e.file_name();
        let n = n.to_string_lossy();
        let path = if let Some(stem) = n.strip_suffix(".log").or_else(|| n.strip_suffix(".ended")) {
            dir.join(stem)
        } else if e.file_type()?.is_socket() {
            p
        } else {
            continue;
        };
        if matches!(state(&path)?, State::Ended | State::Stale) {
            remove(&path)?;
        }
    }
    Ok(())
}
