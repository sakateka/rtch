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
pub fn signals(server: bool) -> io::Result<()> {
    // SAFETY: sigaction is a plain C struct; handlers only use lock-free atomics.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        libc::sigemptyset(&raw mut action.sa_mask);
        action.sa_sigaction = signal as *const () as libc::sighandler_t;
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGWINCH, libc::SIGHUP] {
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
            if cred.assume_init().uid != uid() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "session belongs to another user",
                ));
            }
        }
    }
    Ok(())
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
