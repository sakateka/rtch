use crate::{
    Result,
    cli::Options,
    fail, os, server,
    storage::{self, State},
};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    os::fd::AsRawFd,
    path::Path,
    time::{Duration, Instant},
};

// Restore modes applications can leave enabled when their client disappears.
// CSI = 0 u resets kitty keyboard reporting; >4;0m resets modifyOtherKeys.
const RESET:&[u8]=b"\x1b[?2026l\x1b[=0u\x1b[>4;0m\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?1l\x1b>\x1b[?1049l\x1b[r\x1b[0m\x1b[?25h";
struct Terminal {
    original: libc::termios,
    flags: i32,
    ansi: bool,
}
impl Terminal {
    fn enter(ansi: bool) -> Result<Self> {
        let original = os::term(0).map_err(|_| "attaching requires a terminal")?;
        let flags = os::flags(1)?;
        let guard = Self {
            original,
            flags,
            ansi,
        };
        os::nonblocking(1)?;
        os::set_term(0, &os::raw(original), false)?;
        Ok(guard)
    }
    fn restore(&self) {
        if self.ansi {
            let mut left = RESET;
            let start = Instant::now();
            while !left.is_empty() && start.elapsed() < Duration::from_millis(250) {
                let mut fds = [libc::pollfd {
                    fd: 1,
                    events: libc::POLLOUT,
                    revents: 0,
                }];
                if os::poll(&mut fds, 20).is_err() {
                    break;
                }
                if fds[0].revents & libc::POLLOUT != 0 {
                    match os::write(1, left) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => left = &left[n..],
                    }
                }
            }
        }
        let _ = os::set_term(0, &self.original, true);
        let _ = os::set_flags(1, self.flags);
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        self.restore();
    }
}
// Keep the single-threaded terminal event loop in one place.
#[allow(clippy::too_many_lines)]
pub fn attach(o: &Options, path: &Path) -> Result<()> {
    match storage::state(path)? {
        State::Running | State::Attached => {}
        State::Ended => {
            return fail("session ended; use tail to read its history or new to restart it");
        }
        State::Stale => return fail("session is stale; its supervisor is not running"),
        State::Missing => return fail("session does not exist"),
    }
    if std::env::var("RTCH_SESSION")
        .unwrap_or_default()
        .split(':')
        .any(|s| Path::new(s) == path)
    {
        return fail("cannot attach to a session from within itself");
    }
    let mut socket = storage::connect(path)?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    os::signals(false)?;
    let terminal = Terminal::enter(o.ansi)?;
    socket.write_all(&server::packet(server::ATTACH, &[]))?;
    socket.write_all(&server::resize_packet(server::WINCH))?;
    if o.redraw != "none" {
        let mut p = server::resize_packet(server::REDRAW);
        p[1] = if o.redraw == "ctrl_l" { 2 } else { 3 };
        socket.write_all(&p)?;
    }
    if o.clear == "move" {
        let _ = os::write(1, b"\x1bc");
    }
    let mut output = VecDeque::new();
    let mut done = false;
    let mut detached = false;
    let mut reactor = crate::reactor::Reactor::new()?;
    loop {
        let signals = os::take_signals();
        if signals & 1 != 0 {
            detached = true;
            break;
        }
        if signals & 2 != 0 {
            socket.write_all(&server::resize_packet(server::WINCH))?;
        }
        if done && output.is_empty() {
            break;
        }
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: socket.as_raw_fd(),
                events: if !done && output.len() < 1024 * 1024 {
                    libc::POLLIN
                } else {
                    0
                },
                revents: 0,
            },
            libc::pollfd {
                fd: 1,
                events: if output.is_empty() { 0 } else { libc::POLLOUT },
                revents: 0,
            },
        ];
        reactor.wait(&mut fds, 100)?;
        if fds[0].revents & libc::POLLIN != 0 {
            let mut buf = [0; 4096];
            let n = os::read(0, &mut buf)?;
            if n == 0 {
                detached = true;
                break;
            }
            let mut start = 0;
            for (i, &b) in buf[..n].iter().enumerate() {
                if Some(b) == o.detach || (o.suspend && b == terminal.original.c_cc[libc::VSUSP]) {
                    push_bytes(&mut socket, &buf[start..i])?;
                    if Some(b) == o.detach {
                        detached = true;
                        break;
                    }
                    socket.write_all(&server::packet(server::SUSPEND, &[]))?;
                    terminal.restore();
                    os::suspend();
                    os::nonblocking(1)?;
                    os::set_term(0, &os::raw(terminal.original), false)?;
                    socket.write_all(&server::packet(server::ATTACH, &[]))?;
                    socket.write_all(&server::resize_packet(server::WINCH))?;
                    start = i + 1;
                }
            }
            if detached {
                break;
            }
            push_bytes(&mut socket, &buf[start..n])?;
        }
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            detached = true;
            break;
        }
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 && !done {
            let mut buf = [0; 8192];
            match socket.read(&mut buf) {
                Ok(0) => done = true,
                Ok(n) => output.extend(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        if fds[2].revents & libc::POLLOUT != 0 && !output.is_empty() {
            match os::write(1, output.as_slices().0) {
                Ok(n) => {
                    output.drain(..n);
                }
                Err(e)
                    if [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted]
                        .contains(&e.kind()) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if fds[2].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            break;
        }
    }
    drop(terminal);
    if !o.quiet {
        println!(
            "\r\n[rtch: session '{}' {}]",
            path.file_name().unwrap_or_default().to_string_lossy(),
            if detached { "detached" } else { "disconnected" }
        );
    }
    Ok(())
}
fn push_bytes(stream: &mut std::os::unix::net::UnixStream, buf: &[u8]) -> Result<()> {
    // Keep the existing ten-byte wire format, but submit packets in batches.
    let mut packets = [0; 10 * 1024];
    for batch in buf.chunks(8 * 1024) {
        for (dest, chunk) in packets
            .as_chunks_mut::<10>()
            .0
            .iter_mut()
            .zip(batch.chunks(8))
        {
            dest.copy_from_slice(&server::packet(server::PUSH, chunk));
        }
        stream.write_all(&packets[..batch.len().div_ceil(8) * 10])?;
    }
    Ok(())
}
pub fn push(path: &Path) -> Result<()> {
    let mut stream = storage::connect(path)?;
    let mut input = io::stdin().lock();
    let mut buf = [0; 8192];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        push_bytes(&mut stream, &buf[..n])?;
    }
    Ok(())
}
pub fn control(path: &Path, kind: u8, force: bool) -> Result<()> {
    let mut s = storage::connect(path)?;
    s.set_read_timeout(Some(Duration::from_secs(8)))?;
    let mut p = server::packet(kind, &[]);
    if force {
        p[1] = 1;
    }
    s.write_all(&p)?;
    let mut ack = [0; 2];
    s.read_exact(&mut ack)?;
    if &ack != b"OK" {
        return fail("unexpected response from supervisor");
    }
    if kind == server::KILL {
        let start = Instant::now();
        while matches!(storage::state(path)?, State::Running | State::Attached) {
            if start.elapsed() > Duration::from_secs(8) {
                return fail("session did not stop");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(())
}
