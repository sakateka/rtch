use crate::{
    Result,
    cli::Options,
    fail, os,
    storage::{self, Log, State},
};
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::{
    collections::VecDeque,
    fs,
    io::{self, Read, Write},
    os::fd::AsRawFd,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
pub const PUSH: u8 = 0;
pub const ATTACH: u8 = 1;
pub const SUSPEND: u8 = 2;
pub const WINCH: u8 = 3;
pub const REDRAW: u8 = 4;
pub const KILL: u8 = 5;
pub const DETACH: u8 = 6;
pub const CLEAR: u8 = 7;
pub fn packet(kind: u8, data: &[u8]) -> [u8; 10] {
    let mut p = [0; 10];
    p[0] = kind;
    p[1] = u8::try_from(data.len()).expect("internal packets contain at most 8 bytes");
    p[2..2 + data.len()].copy_from_slice(data);
    p
}
pub fn resize_packet(kind: u8) -> [u8; 10] {
    let w = os::size(0);
    let mut p = [0; 10];
    p[0] = kind;
    for (dest, n) in
        p[2..]
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip([w.ws_row, w.ws_col, w.ws_xpixel, w.ws_ypixel])
    {
        dest.copy_from_slice(&n.to_ne_bytes());
    }
    p
}
fn winsize(p: &[u8]) -> libc::winsize {
    let n = |i| u16::from_ne_bytes([p[i], p[i + 1]]);
    libc::winsize {
        ws_row: n(2),
        ws_col: n(4),
        ws_xpixel: n(6),
        ws_ypixel: n(8),
    }
}
struct Client {
    stream: UnixStream,
    input: Vec<u8>,
    output: VecDeque<u8>,
    attached: bool,
    close: bool,
    finishing: bool,
}
struct SocketGuard<'a>(&'a Path);
impl Drop for SocketGuard<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}
fn stop(child: i32, pty: i32, sig: i32) {
    os::kill_group(os::foreground(pty), sig);
    os::kill_group(child, sig);
}
struct ChildGuard(i32, i32);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        stop(self.0, self.1, libc::SIGHUP);
    }
}

// One event loop owns all session I/O; splitting it obscures event ordering.
#[allow(clippy::too_many_lines)]
pub fn serve(o: &Options, path: &Path, ready: bool, wait_attach: bool) -> Result<i32> {
    let mut reactor = crate::reactor::Reactor::new()?;
    storage::ensure_parent(path)?;
    match storage::state(path)? {
        State::Running | State::Attached => return fail("session is already running"),
        State::Stale => fs::remove_file(path)?,
        _ => {}
    }
    let previous_mask = os::umask(0o077);
    let listener = storage::bind(path);
    os::umask(previous_mask);
    let listener = listener?;
    let _socket = SocketGuard(path);
    listener.set_nonblocking(true)?;
    let term = os::term(0).ok();
    let size = os::size(0);
    if ready {
        os::daemonize()?;
    }
    let (master, slave) = os::pty(term.as_ref(), &size)?;
    let mut log = Log::open(path, o.cap)?;
    let mut cmd = if let Some(p) = o.program.first() {
        Command::new(p)
    } else {
        let shell = std::env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let mut login_name = std::ffi::OsString::from("-");
        login_name.push(Path::new(&shell).file_name().ok_or("invalid SHELL path")?);
        let mut command = Command::new(shell);
        // The leading '-' requests the shell's native login startup files;
        // functions from .profile stay in the interactive shell itself.
        command.arg0(login_name);
        command
    };
    if !o.program.is_empty() {
        cmd.args(&o.program[1..]);
    }
    let prev = std::env::var("RTCH_SESSION").unwrap_or_default();
    let chain = if prev.is_empty() {
        path.display().to_string()
    } else {
        format!("{prev}:{}", path.display())
    };
    cmd.env("RTCH_SESSION", &chain)
        .env("ATCH_SESSION", &chain)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    os::child_terminal(&mut cmd);
    let mut child = cmd.spawn()?;
    let child_pid = i32::try_from(child.id())?;
    let mut child_guard = ChildGuard(child_pid, master.as_raw_fd());
    os::signals(true)?;
    os::nonblocking(master.as_raw_fd())?;
    if ready {
        println!("READY");
        io::stdout().flush()?;
    }
    os::null_stdio()?;
    match fs::remove_file(storage::side(path, ".ended")) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut clients: Vec<Client> = vec![];
    let mut input = VecDeque::new();
    let mut waiting = wait_attach;
    let mut pty_done = false;
    let mut exit = None;
    let mut ended_at = None;
    let mut shutdown = None;
    let mut marked_attached = false;
    let mut fds = Vec::with_capacity(66);
    loop {
        let signals = os::take_signals();
        if signals & 1 != 0 && shutdown.is_none() {
            stop(child_pid, master.as_raw_fd(), libc::SIGTERM);
            shutdown = Some(Instant::now());
        }
        if shutdown.is_some_and(|t: Instant| t.elapsed() > Duration::from_secs(5)) {
            stop(child_pid, master.as_raw_fd(), libc::SIGKILL);
        }
        if exit.is_none() {
            exit = child.try_wait()?;
            if exit.is_some() {
                child_guard.0 = 0;
                ended_at = Some(Instant::now());
            }
        }
        if pty_done
            && exit.is_some()
            && (clients.iter().all(|c| c.output.is_empty())
                || ended_at.is_some_and(|t| t.elapsed() > Duration::from_millis(500)))
        {
            break;
        }
        if exit.is_some() && ended_at.is_some_and(|t| t.elapsed() > Duration::from_millis(500)) {
            break;
        }
        let attached = clients.iter().any(|c| c.attached && !c.close);
        if attached {
            waiting = false;
        }
        if attached != marked_attached {
            fs::set_permissions(
                path,
                fs::Permissions::from_mode(if attached { 0o700 } else { 0o600 }),
            )?;
            marked_attached = attached;
        }
        fds.clear();
        fds.extend([
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: master.as_raw_fd(),
                events: if pty_done {
                    0
                } else {
                    (if waiting { 0 } else { libc::POLLIN })
                        | if input.is_empty() { 0 } else { libc::POLLOUT }
                },
                revents: 0,
            },
        ]);
        for c in &clients {
            fds.push(libc::pollfd {
                fd: c.stream.as_raw_fd(),
                events: libc::POLLIN
                    | if c.output.is_empty() {
                        0
                    } else {
                        libc::POLLOUT
                    },
                revents: 0,
            });
        }
        reactor.wait(&mut fds, 100)?;
        let mut detach_all = false;
        for (c, pfd) in clients.iter_mut().zip(&fds[2..]) {
            if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                let mut buf = [0; 8192];
                match c.stream.read(&mut buf) {
                    Ok(0) => c.close = true,
                    Ok(n) => c.input.extend_from_slice(&buf[..n]),
                    Err(e)
                        if [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted]
                            .contains(&e.kind()) => {}
                    Err(_) => c.close = true,
                }
                let (packets, remainder) = c.input.as_chunks::<10>();
                let complete = c.input.len() - remainder.len();
                for p in packets {
                    match p[0] {
                        PUSH if p[1] <= 8 && !pty_done => {
                            if input.len() + usize::from(p[1]) > 1024 * 1024 {
                                c.close = true;
                                break;
                            }
                            input.extend(&p[2..2 + usize::from(p[1])]);
                        }
                        ATTACH if !c.attached => {
                            c.attached = true;
                            if c.output.len() + log.history.len()
                                > o.cap.saturating_add(8 * 1024 * 1024)
                            {
                                c.close = true;
                                break;
                            }
                            c.output.extend(&log.history);
                        }
                        ATTACH => {}
                        SUSPEND => c.attached = false,
                        WINCH | REDRAW => {
                            os::set_size(master.as_raw_fd(), winsize(p))?;
                            if p[0] == REDRAW && p[1] == 2 {
                                input.push_back(12);
                            } else if p[0] == REDRAW && p[1] == 3 {
                                os::kill_group(os::foreground(master.as_raw_fd()), libc::SIGWINCH);
                            }
                        }
                        KILL => {
                            stop(
                                child_pid,
                                master.as_raw_fd(),
                                if p[1] == 1 {
                                    libc::SIGKILL
                                } else {
                                    libc::SIGTERM
                                },
                            );
                            shutdown = Some(Instant::now());
                            c.output.extend(b"OK");
                            c.finishing = true;
                        }
                        DETACH => {
                            detach_all = true;
                            c.output.extend(b"OK");
                            c.finishing = true;
                        }
                        CLEAR => {
                            log.clear()?;
                            c.output.extend(b"OK");
                            c.finishing = true;
                        }
                        _ => {
                            c.close = true;
                            break;
                        }
                    }
                    if c.finishing {
                        break;
                    }
                }
                c.input.drain(..complete);
            }
            if pfd.revents & libc::POLLOUT != 0 && !c.output.is_empty() {
                match c.stream.write(c.output.as_slices().0) {
                    Ok(0) => c.close = true,
                    Ok(n) => {
                        c.output.drain(..n);
                        if c.finishing && c.output.is_empty() {
                            c.close = true;
                        }
                    }
                    Err(e)
                        if [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted]
                            .contains(&e.kind()) => {}
                    Err(_) => c.close = true,
                }
            }
            // HUP can accompany unread packets after `push` exits. Drain to
            // read(0) before closing, otherwise a batched input loses its tail.
            if pfd.revents & libc::POLLNVAL != 0 {
                c.close = true;
            }
        }
        if detach_all {
            for c in &mut clients {
                if c.attached {
                    c.close = true;
                }
            }
        }
        for c in clients.iter().filter(|c| c.close) {
            reactor.remove(c.stream.as_raw_fd())?;
        }
        clients.retain(|c| !c.close);
        if fds[1].revents & libc::POLLOUT != 0 && !input.is_empty() {
            match os::write(master.as_raw_fd(), input.as_slices().0) {
                Ok(n) => {
                    input.drain(..n);
                }
                Err(e)
                    if [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted]
                        .contains(&e.kind()) => {}
                Err(_) => {
                    pty_done = true;
                    input.clear();
                }
            }
        }
        if !waiting
            && !pty_done
            && fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
        {
            let mut buf = [0; 8192];
            match os::read(master.as_raw_fd(), &mut buf) {
                Ok(0) => pty_done = true,
                Ok(n) => {
                    log.append(&buf[..n])?;
                    for c in &mut clients {
                        if c.attached {
                            if c.output.len() > o.cap.saturating_add(8 * 1024 * 1024) {
                                c.close = true;
                            } else {
                                c.output.extend(&buf[..n]);
                            }
                        }
                    }
                }
                Err(e)
                    if [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted]
                        .contains(&e.kind()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EIO) => pty_done = true,
                Err(e) => return Err(e.into()),
            }
        }
        if fds[0].revents & libc::POLLIN != 0 {
            for _ in 0..64 {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if clients.len() >= 64 || os::validate_peer(&stream).is_err() {
                            continue;
                        }
                        stream.set_nonblocking(true)?;
                        clients.push(Client {
                            stream,
                            input: vec![],
                            output: VecDeque::new(),
                            attached: false,
                            close: false,
                            finishing: false,
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    let code = exit.map_or(128, |s| {
        s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
    });
    let mut ended = storage::open_file(&storage::side(path, ".ended"), true)?;
    ended.set_len(0)?;
    writeln!(
        ended,
        "ended\nexit={code}\ntime={}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
    )?;
    Ok(code)
}
pub fn start(o: &Options, path: &Path, wait: bool) -> Result<()> {
    use std::io::BufRead;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__serve").arg("-C").arg(o.cap.to_string());
    if wait {
        cmd.arg("--wait");
    }
    cmd.arg(path).arg("--").args(&o.program);
    let mut child = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut reply = String::new();
    let stdout = child
        .stdout
        .take()
        .ok_or("missing supervisor status pipe")?;
    std::io::BufReader::new(stdout).read_line(&mut reply)?;
    if reply != "READY\n" {
        let status = child.wait()?;
        return fail(format!("session could not start ({status})"));
    }
    Ok(())
}
