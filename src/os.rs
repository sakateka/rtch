//! Small Unix boundary. All descriptors are owned by Rust outside these calls.
#![allow(unsafe_code)]
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicI32, Ordering};
use std::{
    fs::File,
    io,
    mem::MaybeUninit,
    os::fd::{AsRawFd, FromRawFd, RawFd},
    process::Command,
};

pub static SIGNALS: AtomicI32 = AtomicI32::new(0);
extern "C" fn signal(sig: libc::c_int) {
    SIGNALS.fetch_or(if sig == libc::SIGWINCH { 2 } else { 1 }, Ordering::Relaxed);
}
pub fn take_signals() -> i32 {
    SIGNALS.swap(0, Ordering::Relaxed)
}
pub fn lock(fd: RawFd) -> io::Result<()> {
    loop {
        // SAFETY: flock borrows the open descriptor for this call.
        if unsafe { libc::flock(fd, libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
pub fn try_lock(fd: RawFd) -> io::Result<bool> {
    // SAFETY: flock borrows the open descriptor for this call.
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error)
    }
}
pub fn signals(server: bool) -> io::Result<()> {
    install_signals(server, false)
}
pub struct PickerSignals {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}
pub fn picker_signals() -> io::Result<PickerSignals> {
    let mut guard = PickerSignals {
        previous: Vec::new(),
    };
    for sig in [
        libc::SIGTERM,
        libc::SIGINT,
        libc::SIGWINCH,
        libc::SIGHUP,
        libc::SIGPIPE,
        libc::SIGQUIT,
    ] {
        // SAFETY: sigaction initializes the plain C struct; the null action
        // argument queries the current disposition without changing it.
        let previous = unsafe {
            let mut previous: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, std::ptr::null(), &raw mut previous) < 0 {
                return Err(io::Error::last_os_error());
            }
            previous
        };
        guard.previous.push((sig, previous));
    }
    install_signals(false, true)?;
    Ok(guard)
}
impl Drop for PickerSignals {
    fn drop(&mut self) {
        for (sig, previous) in &self.previous {
            // SAFETY: each previous action came from sigaction and remains
            // initialized; no borrowed pointer survives this call.
            unsafe {
                libc::sigaction(*sig, previous, std::ptr::null_mut());
            }
        }
    }
}
fn install_signals(server: bool, picker: bool) -> io::Result<()> {
    // SAFETY: sigaction is a plain C struct; handlers only use lock-free atomics.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        libc::sigemptyset(&raw mut action.sa_mask);
        action.sa_sigaction = signal as *const () as libc::sighandler_t;
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGWINCH, libc::SIGHUP]
            .into_iter()
            .chain(picker.then_some(libc::SIGQUIT))
        {
            action.sa_sigaction = if server && sig == libc::SIGHUP {
                libc::SIG_IGN
            } else {
                signal as *const () as libc::sighandler_t
            };
            if libc::sigaction(sig, &raw const action, std::ptr::null_mut()) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        action.sa_sigaction = libc::SIG_IGN;
        if libc::sigaction(libc::SIGPIPE, &raw const action, std::ptr::null_mut()) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
pub fn term(fd: RawFd) -> io::Result<libc::termios> {
    let mut t = MaybeUninit::uninit();
    // SAFETY: tcgetattr initializes t on success; fd is borrowed for the call.
    unsafe {
        if libc::tcgetattr(fd, t.as_mut_ptr()) < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(t.assume_init())
    }
}
pub fn set_term(fd: RawFd, t: &libc::termios, flush: bool) -> io::Result<()> {
    // SAFETY: t is initialized and valid for the duration of tcsetattr.
    if unsafe {
        libc::tcsetattr(
            fd,
            if flush {
                libc::TCSAFLUSH
            } else {
                libc::TCSANOW
            },
            t,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub fn raw(mut t: libc::termios) -> libc::termios {
    // SAFETY: cfmakeraw only modifies the provided initialized termios.
    unsafe { libc::cfmakeraw(&raw mut t) };
    t
}
pub fn size(fd: RawFd) -> libc::winsize {
    // SAFETY: winsize is a plain integer struct; fallback size is valid.
    let mut w: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: ioctl writes a winsize into a valid pointer.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &raw mut w) } < 0 || w.ws_row == 0 {
        w.ws_row = 24;
        w.ws_col = 80;
    }
    w
}
pub fn set_size(fd: RawFd, w: libc::winsize) -> io::Result<()> {
    // SAFETY: ioctl reads a valid winsize for this call.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &raw const w) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub fn pty(t: Option<&libc::termios>, w: &libc::winsize) -> io::Result<(File, File)> {
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty initializes both descriptors on success; pointers are valid.
    if unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            t.map_or(std::ptr::null(), std::ptr::from_ref),
            w,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: each returned descriptor is newly allocated and owned exactly once.
    let pair = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for fd in [master, slave] {
        // SAFETY: F_SETFD takes an integer flag and a live descriptor.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(pair)
}
pub fn child_terminal(cmd: &mut Command) {
    // SAFETY: the pre-exec closure calls only async-signal-safe libc operations;
    // stdin has been dup2'd to the owned slave by Command before this closure.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
pub fn daemonize() -> io::Result<()> {
    // SAFETY: setsid has no pointer arguments, process is a fresh supervisor.
    if unsafe { libc::setsid() } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl operates on a borrowed live descriptor and integer flags.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
pub fn poll(fds: &mut [libc::pollfd], timeout: i32) -> io::Result<()> {
    // SAFETY: poll gets the complete mutable slice and does not retain it.
    if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    Ok(())
}
pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: read writes at most buf.len() bytes; the descriptor is borrowed.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        usize::try_from(n).map_err(io::Error::other)
    }
}
pub fn write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: write reads at most buf.len() bytes; the descriptor is borrowed.
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        usize::try_from(n).map_err(io::Error::other)
    }
}
pub fn uid() -> u32 {
    // SAFETY: getuid has no arguments or memory effects.
    unsafe { libc::getuid() }
}
pub fn umask(mask: libc::mode_t) -> libc::mode_t {
    // SAFETY: the supervisor is single threaded; callers restore the old mask.
    unsafe { libc::umask(mask) }
}
pub fn foreground(fd: RawFd) -> i32 {
    // SAFETY: tcgetpgrp takes a borrowed terminal descriptor.
    unsafe { libc::tcgetpgrp(fd) }
}
pub fn kill_group(pid: i32, sig: i32) {
    if pid > 1 {
        // SAFETY: only a validated positive process-group id is negated.
        unsafe { libc::kill(-pid, sig) };
    }
}
pub fn suspend() {
    // SAFETY: raise addresses the current process only.
    unsafe { libc::raise(libc::SIGSTOP) };
}
pub fn validate_peer(stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    peer_pid(stream)?;
    Ok(())
}
pub fn peer_pid(stream: &std::os::unix::net::UnixStream) -> io::Result<i32> {
    #[cfg(target_os = "linux")]
    {
        let mut cred = MaybeUninit::<libc::ucred>::uninit();
        let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
            .map_err(io::Error::other)?;
        // SAFETY: getsockopt writes to the correctly sized credential struct.
        unsafe {
            if libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                cred.as_mut_ptr().cast(),
                &raw mut len,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
            if len as usize != std::mem::size_of::<libc::ucred>() {
                return Err(io::Error::other("invalid peer credentials length"));
            }
            let cred = cred.assume_init();
            if cred.uid != uid() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "session belongs to another user",
                ));
            }
            Ok(cred.pid)
        }
    }
    #[cfg(not(target_os = "linux"))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "session process identification requires Linux",
    ))
}

#[derive(Debug)]
struct ProcessInfo {
    parent: i32,
    session: i32,
    started: u64,
    alive: bool,
}
fn process_info(pid: i32) -> io::Result<ProcessInfo> {
    parse_process_info(&std::fs::read(format!("/proc/{pid}/stat"))?)
}
fn parse_process_info(stat: &[u8]) -> io::Result<ProcessInfo> {
    // comm can contain arbitrary bytes, spaces and parentheses. Only the
    // guaranteed ASCII fields after its final ')' are interpreted as text.
    let end = stat
        .iter()
        .rposition(|&byte| byte == b')')
        .ok_or_else(|| io::Error::other("invalid process status"))?;
    let fields = std::str::from_utf8(&stat[end + 1..])
        .map_err(io::Error::other)?
        .split_whitespace()
        .collect::<Vec<_>>();
    let number = |index: usize| {
        fields
            .get(index)
            .ok_or_else(|| io::Error::other("incomplete process status"))?
            .parse::<i32>()
            .map_err(io::Error::other)
    };
    Ok(ProcessInfo {
        parent: number(1)?,
        session: number(3)?,
        started: fields
            .get(19)
            .ok_or_else(|| io::Error::other("incomplete process status"))?
            .parse()
            .map_err(io::Error::other)?,
        alive: !matches!(fields.first(), Some(&"Z" | &"X")),
    })
}
fn exited(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}
struct SessionProcess {
    pid: i32,
    started: u64,
    descriptor: File,
}
pub struct ChildSession {
    id: i32,
    started: u64,
    members: Vec<SessionProcess>,
}
impl ChildSession {
    pub fn identify(stream: &std::os::unix::net::UnixStream) -> io::Result<Self> {
        let supervisor = peer_pid(stream)?;
        let children =
            std::fs::read_to_string(format!("/proc/{supervisor}/task/{supervisor}/children"))?;
        let mut session = None;
        for child in children.split_whitespace() {
            let pid: i32 = child.parse().map_err(io::Error::other)?;
            let info = match process_info(pid) {
                Ok(info) => info,
                Err(error) if exited(&error) => continue,
                Err(error) => return Err(error),
            };
            if pid > 1 && info.parent == supervisor && info.session == pid && info.alive {
                if session.is_some() {
                    return Err(io::Error::other("ambiguous supervisor child session"));
                }
                session = Some(Self {
                    id: pid,
                    started: info.started,
                    members: Vec::new(),
                });
            }
        }
        let mut session =
            session.ok_or_else(|| io::Error::other("supervisor child session is missing"))?;
        // Capture identities before KILL lets an older supervisor reap its child.
        session.members = session.members()?;
        if !session
            .members
            .iter()
            .any(|member| member.pid == session.id)
        {
            return Err(io::Error::other("supervisor child session has ended"));
        }
        Ok(session)
    }
    fn members(&self) -> io::Result<Vec<SessionProcess>> {
        let mut members = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
                .filter(|pid| *pid > 1)
            else {
                continue;
            };
            // SAFETY: getsid only inspects this positive PID. Checking SID
            // first avoids opening inaccessible stat files of unrelated users.
            let session = unsafe { libc::getsid(pid) };
            if session < 0 {
                let error = io::Error::last_os_error();
                if exited(&error) {
                    continue;
                }
                return Err(error);
            }
            if session != self.id {
                continue;
            }
            let info = match process_info(pid) {
                Ok(info) => info,
                Err(error) if exited(&error) => continue,
                Err(error) => return Err(error),
            };
            if info.session != self.id || !info.alive {
                continue;
            }
            // SAFETY: pid is positive; pidfd_open takes no pointer arguments.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd < 0 {
                let error = io::Error::last_os_error();
                if exited(&error) {
                    continue;
                }
                return Err(error);
            }
            // SAFETY: pidfd_open returned a new owned descriptor.
            let descriptor =
                unsafe { File::from_raw_fd(i32::try_from(fd).map_err(io::Error::other)?) };
            let current = match process_info(pid) {
                Ok(info) => info,
                Err(error) if exited(&error) => continue,
                Err(error) => return Err(error),
            };
            if current.started != info.started || current.session != self.id || !current.alive {
                continue;
            }
            members.push(SessionProcess {
                pid,
                started: info.started,
                descriptor,
            });
        }
        Ok(members)
    }
    pub fn kill(mut self) -> io::Result<()> {
        let start = std::time::Instant::now();
        loop {
            // A reused session-leader PID must never widen the selected scope.
            match process_info(self.id) {
                Ok(info) if info.started != self.started || info.session != self.id => {
                    return Err(io::Error::other("session identity changed"));
                }
                Ok(_) => {}
                Err(error) if exited(&error) => {}
                Err(error) => return Err(error),
            }
            let mut failure = None;
            for member in &self.members {
                let info = match process_info(member.pid) {
                    Ok(info) => info,
                    Err(error) if exited(&error) => continue,
                    Err(error) => {
                        failure.get_or_insert(error);
                        continue;
                    }
                };
                if info.session != self.id || info.started != member.started || !info.alive {
                    continue;
                }
                // SAFETY: the pidfd pins this process identity. The immediately
                // preceding status read verifies membership in the selected SID.
                let result = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        member.descriptor.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                };
                if result < 0 {
                    let error = io::Error::last_os_error();
                    if !exited(&error) {
                        failure.get_or_insert(error);
                    }
                }
            }
            self.members.clear();
            if let Some(error) = failure {
                return Err(error);
            }
            self.members = self.members()?;
            if self.members.is_empty() {
                return Ok(());
            }
            if start.elapsed() > std::time::Duration::from_secs(8) {
                return Err(io::Error::other("session processes did not stop"));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
pub fn null_stdio() -> io::Result<()> {
    let null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    for fd in 0..=2 {
        // SAFETY: dup2 duplicates a live file descriptor to standard I/O.
        if unsafe { libc::dup2(null.as_raw_fd(), fd) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
pub fn flags(fd: RawFd) -> io::Result<i32> {
    // SAFETY: F_GETFL only inspects a borrowed descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(flags)
    }
}
pub fn set_flags(fd: RawFd, flags: i32) -> io::Result<()> {
    // SAFETY: F_SETFL takes an integer mask on a borrowed descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::parse_process_info;

    #[test]
    fn process_status_accepts_non_utf8_names_with_spaces_and_parentheses() {
        for state in ['S', 'Z'] {
            let mut stat = b"456 (raw\xff \xfe name)with)paren)".to_vec();
            stat.extend_from_slice(
                format!(" {state} 123 456 456 {} 789\n", ["0"; 15].join(" ")).as_bytes(),
            );
            assert!(std::str::from_utf8(&stat).is_err());
            let info = parse_process_info(&stat).unwrap();
            assert_eq!(info.parent, 123);
            assert_eq!(info.session, 456);
            assert_eq!(info.started, 789);
            assert_eq!(info.alive, state == 'S');
        }
    }
}
